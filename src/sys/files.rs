//! Descriptor-relative filesystem calls. All newly opened descriptors are CLOEXEC.
use super::cv;
use std::{
    ffi::CStr,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Incarnation {
    pub birth: Option<(i64, u32)>,
    pub generation: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unverified: bool,
}
impl Incarnation {
    pub fn available(&self) -> bool {
        !self.unverified && (self.birth.is_some() || self.generation.is_some())
    }
    pub fn captured(base: i32, name: &CStr, stat: &libc::stat, birth: Option<(i64, u32)>) -> Self {
        let generation = generation_at(base, name, stat);
        Self {
            birth,
            generation,
            // An unpinned name cannot be completed later. Do not turn a failed
            // generation lookup into a birth-time-only alias identity.
            unverified: generation.is_none()
                && matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFDIR),
        }
    }
}

/// Read immutable identity fields from a pinned, non-followed inode.
pub fn incarnation_at(base: i32, name: &CStr, expected: &libc::stat) -> Incarnation {
    let regular = expected.st_mode & libc::S_IFMT == libc::S_IFREG;
    let directory = expected.st_mode & libc::S_IFMT == libc::S_IFDIR;
    let fd = open_at(
        base,
        name,
        (if regular || directory {
            libc::O_RDONLY | libc::O_NONBLOCK
        } else {
            libc::O_PATH
        }) | libc::O_NOFOLLOW,
        0,
    )
    .or_else(|_| open_at(base, name, libc::O_PATH | libc::O_NOFOLLOW, 0));
    let Ok(fd) = fd else {
        return Incarnation::default();
    };
    if !stat_fd(fd.as_raw_fd())
        .is_ok_and(|st| st.st_dev == expected.st_dev && st.st_ino == expected.st_ino)
    {
        return Incarnation::default();
    }
    // SAFETY: statx is plain output storage initialized by the kernel.
    let mut sx: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: fd is owned, the empty name is terminated, and sx is writable.
    let result = unsafe {
        libc::statx(
            fd.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BTIME,
            &mut sx,
        )
    };
    let birth = (result == 0 && sx.stx_mask & libc::STATX_BTIME != 0)
        .then_some((sx.stx_btime.tv_sec, sx.stx_btime.tv_nsec));
    Incarnation {
        birth,
        generation: (regular || directory)
            .then(|| generation_fd(fd.as_raw_fd()))
            .flatten(),
        unverified: false,
    }
}

pub fn open_at(base: i32, name: &CStr, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    // SAFETY: name is terminated; the kernel validates base and flags.
    let fd = cv(unsafe { libc::openat(base, name.as_ptr(), flags | libc::O_CLOEXEC, mode) })?;
    // SAFETY: openat returned a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
pub fn stat_at(base: i32, name: &CStr) -> io::Result<Option<libc::stat>> {
    // SAFETY: zeroed output is initialized by fstatat; name is terminated.
    let mut st = unsafe { std::mem::zeroed() };
    // SAFETY: st is writable and name is live for this call.
    if unsafe { libc::fstatat(base, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(Some(st))
    } else {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(e)
        }
    }
}
pub fn stat_fd(fd: i32) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat is valid output storage.
    let mut st = unsafe { std::mem::zeroed() };
    // SAFETY: st is writable; fd is validated by the kernel.
    cv(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}
pub type CaptureStat = (Option<libc::stat>, Option<u64>, Option<(i64, u32)>);

pub fn stat_capture_at(base: i32, name: &CStr) -> io::Result<CaptureStat> {
    stat_with_mount(base, name, 0)
}
pub fn generation_fd(fd: i32) -> Option<u32> {
    // Linux fs.h encodes GETVERSION with sizeof(long), but returns an int.
    const FS_IOC_GETVERSION: libc::c_ulong = 0x80087601;
    let mut generation: libc::c_int = 0;
    // SAFETY: the ioctl writes one int to live output storage; the caller keeps fd live.
    (unsafe { libc::ioctl(fd, FS_IOC_GETVERSION, &mut generation) } == 0)
        .then_some(generation as u32)
}
pub fn generation_at(base: i32, name: &CStr, expected: &libc::stat) -> Option<u32> {
    #[cfg(debug_assertions)]
    if std::env::var_os("SAQTA_TEST_NO_FILE_HANDLES").is_some_and(|value| value == "1") {
        return None;
    }
    if !matches!(
        expected.st_mode & libc::S_IFMT,
        libc::S_IFREG | libc::S_IFDIR
    ) {
        return None;
    }
    #[repr(C)]
    struct Handle {
        bytes: u32,
        kind: i32,
        words: [u32; 2],
    }
    let mut handle = Handle {
        bytes: 8,
        kind: 0,
        words: [0; 2],
    };
    let mut mount = 0i32;
    // SAFETY: Handle has the file_handle header followed by eight writable bytes;
    // name is terminated, mount is writable, and the kernel validates base.
    let result = unsafe {
        libc::name_to_handle_at(
            base,
            name.as_ptr(),
            (&mut handle as *mut Handle).cast(),
            &mut mount,
            0,
        )
    };
    // FILEID_INO32_GEN is the kernel's generic (inode, generation) format.
    // Reject private formats, oversized handles, and a raced inode lookup.
    (result == 0
        && handle.kind == 1
        && handle.bytes == 8
        && u64::from(handle.words[0]) == expected.st_ino)
        .then_some(handle.words[1])
}
pub fn stat_mount_fd(fd: i32) -> io::Result<(libc::stat, Option<u64>)> {
    let (stat, mount, _) = stat_with_mount(fd, c"", libc::AT_EMPTY_PATH)?;
    Ok((
        stat.ok_or_else(|| io::Error::other("missing pinned directory"))?,
        mount,
    ))
}
fn stat_with_mount(base: i32, name: &CStr, flags: i32) -> io::Result<CaptureStat> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static UNAVAILABLE: AtomicBool = AtomicBool::new(false);
    if !UNAVAILABLE.load(Ordering::Relaxed) {
        // SAFETY: statx is a plain C output structure, initialized by the kernel.
        let mut sx: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: name is terminated and sx is writable for the whole syscall.
        let result = unsafe {
            libc::statx(
                base,
                name.as_ptr(),
                flags | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS | libc::STATX_MNT_ID | libc::STATX_BTIME,
                &mut sx,
            )
        };
        if result == 0 && sx.stx_mask & libc::STATX_BASIC_STATS == libc::STATX_BASIC_STATS {
            // SAFETY: libc::stat is plain integer storage; all public stat fields are assigned below.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            st.st_dev = libc::makedev(sx.stx_dev_major, sx.stx_dev_minor);
            st.st_ino = sx.stx_ino;
            st.st_mode = u32::from(sx.stx_mode);
            st.st_nlink = sx.stx_nlink.into();
            st.st_uid = sx.stx_uid;
            st.st_gid = sx.stx_gid;
            st.st_rdev = libc::makedev(sx.stx_rdev_major, sx.stx_rdev_minor);
            st.st_size = i64::try_from(sx.stx_size).map_err(io::Error::other)?;
            st.st_blksize = sx.stx_blksize.into();
            st.st_blocks = i64::try_from(sx.stx_blocks).map_err(io::Error::other)?;
            st.st_atime = sx.stx_atime.tv_sec;
            st.st_atime_nsec = sx.stx_atime.tv_nsec.into();
            st.st_mtime = sx.stx_mtime.tv_sec;
            st.st_mtime_nsec = sx.stx_mtime.tv_nsec.into();
            st.st_ctime = sx.stx_ctime.tv_sec;
            st.st_ctime_nsec = sx.stx_ctime.tv_nsec.into();
            #[cfg(debug_assertions)]
            if std::env::var_os("SAQTA_TEST_NO_MOUNT_ID").is_some_and(|s| s == "1") {
                return Ok((
                    Some(st),
                    None,
                    (sx.stx_mask & libc::STATX_BTIME != 0)
                        .then_some((sx.stx_btime.tv_sec, sx.stx_btime.tv_nsec)),
                ));
            }
            return Ok((
                Some(st),
                (sx.stx_mask & libc::STATX_MNT_ID != 0).then_some(sx.stx_mnt_id),
                (sx.stx_mask & libc::STATX_BTIME != 0)
                    .then_some((sx.stx_btime.tv_sec, sx.stx_btime.tv_nsec)),
            ));
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENOENT) => return Ok((None, None, None)),
                Some(libc::EPERM | libc::ENOSYS) => UNAVAILABLE.store(true, Ordering::Relaxed),
                Some(libc::EOPNOTSUPP | libc::EINVAL) => {}
                _ => return Err(error),
            }
        }
    }
    // Without a mount identity use the original metadata and fresh classification.
    let stat = if flags & libc::AT_EMPTY_PATH != 0 {
        Some(stat_fd(base)?)
    } else {
        stat_at(base, name)?
    };
    Ok((stat, None, None))
}
pub fn fs_type(fd: i32) -> io::Result<i64> {
    // SAFETY: zeroed statfs is valid output storage.
    let mut st = unsafe { std::mem::zeroed() };
    // SAFETY: st is writable; fd is validated by the kernel.
    cv(unsafe { libc::fstatfs(fd, &mut st) })?;
    Ok(st.f_type)
}
pub fn link_at(from: i32, name: &CStr, to: i32, dest: &CStr) -> io::Result<()> {
    // SAFETY: both names are terminated. Zero flags preserves the final symlink itself.
    cv(unsafe { libc::linkat(from, name.as_ptr(), to, dest.as_ptr(), 0) }).map(|_| ())
}
pub fn rename_at(from: i32, name: &CStr, to: i32, dest: &CStr, flags: u32) -> io::Result<()> {
    // SAFETY: names are terminated and the kernel validates the directory fds and flags.
    cv(unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from,
            name.as_ptr(),
            to,
            dest.as_ptr(),
            flags,
        ) as i32
    })
    .map(|_| ())
}
pub fn unlink_at(base: i32, name: &CStr, directory: bool) -> io::Result<()> {
    // SAFETY: name is terminated; unlinkat never follows the final symlink.
    cv(unsafe {
        libc::unlinkat(
            base,
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    })
    .map(|_| ())
}
pub fn mkdir_at(base: i32, name: &CStr, mode: u32) -> io::Result<()> {
    // SAFETY: name is terminated and mode is a value.
    cv(unsafe { libc::mkdirat(base, name.as_ptr(), mode) }).map(|_| ())
}
pub fn symlink_at(target: &CStr, base: i32, name: &CStr) -> io::Result<()> {
    // SAFETY: target and name are terminated; symlinkat does not dereference target.
    cv(unsafe { libc::symlinkat(target.as_ptr(), base, name.as_ptr()) }).map(|_| ())
}
pub fn metadata_fd(fd: i32, mode: u32, sec: i64, nsec: i64) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
    ];
    // SAFETY: times has the two required entries; these calls act on an owned fd.
    unsafe {
        cv(libc::fchmod(fd, mode & 0o7777))?;
        cv(libc::futimens(fd, times.as_ptr()))?;
    }
    Ok(())
}
pub fn symlink_mtime(base: i32, name: &CStr, sec: i64, nsec: i64) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
    ];
    // SAFETY: name is terminated and times contains two valid entries; do not follow the link.
    cv(unsafe {
        libc::utimensat(
            base,
            name.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .map(|_| ())
}
const FICLONE: libc::c_ulong = 0x40049409;
const COPY_CHUNK: usize = 1024 * 1024;
pub fn copy_regular(mut src: File, mut dst: File, st: &libc::stat) -> io::Result<()> {
    let actual = src.metadata()?;
    use std::os::unix::fs::MetadataExt;
    if !actual.is_file() || actual.dev() != st.st_dev || actual.ino() != st.st_ino {
        return Err(io::Error::other("capture source changed"));
    }
    // SAFETY: both descriptors refer to live regular files; ioctl takes the source fd by value.
    let cloned = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) } == 0;
    if !cloned {
        let mut left = st.st_size as u64;
        while left > 0 {
            // SAFETY: null offsets use owned file positions; count is bounded; flags must be zero.
            let n = unsafe {
                libc::copy_file_range(
                    src.as_raw_fd(),
                    std::ptr::null_mut(),
                    dst.as_raw_fd(),
                    std::ptr::null_mut(),
                    left.min(COPY_CHUNK as u64) as usize,
                    0,
                )
            };
            if n > 0 {
                left -= n as u64;
                continue;
            }
            // Restart both streams so a partial copy_file_range cannot duplicate bytes.
            src.seek(SeekFrom::Start(0))?;
            dst.set_len(0)?;
            dst.seek(SeekFrom::Start(0))?;
            let n = io::copy(&mut (&mut src).take(st.st_size as u64), &mut dst)?;
            if n != st.st_size as u64 {
                return Err(io::Error::other("short capture copy"));
            }
            break;
        }
    }
    dst.flush()?;
    metadata_fd(dst.as_raw_fd(), st.st_mode, st.st_mtime, st.st_mtime_nsec)
}
pub fn flock_exclusive(fd: i32, nonblocking: bool) -> io::Result<()> {
    let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    loop {
        // SAFETY: flock only changes the lock associated with fd.
        match cv(unsafe { libc::flock(fd, flags) }) {
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            other => return other.map(|_| ()),
        }
    }
}
pub fn unlock(fd: i32) {
    // SAFETY: kernel validates fd; unlocking does not access process memory.
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
    }
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_IN_ROOT: u64 = 0x10;
pub fn open_target_parent(root: i32, path: &CStr) -> io::Result<OwnedFd> {
    open_parent(root, path, RESOLVE_IN_ROOT)
}
pub fn open_restore_parent(root: i32, path: &CStr) -> io::Result<OwnedFd> {
    open_parent(root, path, RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH)
}
fn open_parent(root: i32, path: &CStr, resolve: u64) -> io::Result<OwnedFd> {
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve,
    };
    let mut retries = 0;
    let fd = loop {
        // SAFETY: path and how are live; how's size matches the Linux open_how ABI.
        match cv(unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root,
                path.as_ptr(),
                &how,
                size_of::<OpenHow>(),
            ) as i32
        }) {
            Ok(fd) => break fd,
            // openat2 documents EAGAIN when concurrent renames prevent proving
            // that '..' stays beneath the root. Retry with identical restrictions.
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) && retries < 7 => retries += 1,
            Err(e) => return Err(e),
        }
    };
    // SAFETY: the successful syscall returned a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
pub fn inherit(fd: i32, yes: bool) -> io::Result<()> {
    // SAFETY: fcntl validates fd; this changes only its close-on-exec flag.
    cv(unsafe { libc::fcntl(fd, libc::F_SETFD, if yes { 0 } else { libc::FD_CLOEXEC }) })
        .map(|_| ())
}
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal zero only checks process existence and permission.
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pid and zero flags are values. Linux creates pidfds with CLOEXEC.
    let fd = cv(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 })?;
    // SAFETY: pidfd_open returned a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(debug_assertions)]
pub fn truncate_path(path: &CStr, length: u64) -> io::Result<()> {
    // SAFETY: path is terminated and length is a bounded test value.
    cv(unsafe { libc::truncate(path.as_ptr(), length as libc::off_t) }).map(|_| ())
}
