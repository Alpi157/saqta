//! Descriptor-relative filesystem calls. All newly opened descriptors are CLOEXEC.
use super::cv;
use std::{
    ffi::CStr,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

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
