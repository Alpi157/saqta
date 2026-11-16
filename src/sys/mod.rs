//! All unsafe operations live here. Forked paths use only stack storage and syscalls.
mod files;
mod memory;
pub use files::*;
use libc::{c_int, pid_t};
pub use memory::*;
use std::{
    ffi::{CStr, CString},
    io, ptr,
};

#[repr(C)]
#[derive(Default)]
pub struct SeccompData {
    pub nr: i32,
    pub arch: u32,
    pub ip: u64,
    pub args: [u64; 6],
}
#[repr(C)]
#[derive(Default)]
pub struct Notification {
    pub id: u64,
    pub pid: u32,
    pub flags: u32,
    pub data: SeccompData,
}
#[repr(C)]
#[derive(Default)]
pub struct Response {
    pub id: u64,
    pub val: i64,
    pub error: i32,
    pub flags: u32,
}
// seccomp(2) operation and flags, named here to keep the syscall boundary explicit.
const SECCOMP_SET_MODE_FILTER: libc::c_uint = libc::SECCOMP_SET_MODE_FILTER;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32;
const SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP: u64 = 1;

// Linux x86_64 _IOC encoding: direction:2, size:14, type:8, number:8.
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const fn seccomp_ioctl<T>(direction: u32, number: u8) -> libc::c_ulong {
    ((direction << 30)
        | ((std::mem::size_of::<T>() as u32) << 16)
        | ((b'!' as u32) << 8)
        | number as u32) as libc::c_ulong
}
const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong =
    seccomp_ioctl::<Notification>(IOC_READ | IOC_WRITE, 0);
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = seccomp_ioctl::<Response>(IOC_READ | IOC_WRITE, 1);
const SECCOMP_IOCTL_NOTIF_ID_VALID: libc::c_ulong = seccomp_ioctl::<u64>(IOC_WRITE, 2);
const SECCOMP_IOCTL_NOTIF_SET_FLAGS: libc::c_ulong = seccomp_ioctl::<u64>(IOC_WRITE, 4);

fn cv(v: c_int) -> io::Result<c_int> {
    if v < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
}
pub fn fork() -> io::Result<pid_t> {
    // SAFETY: single threaded caller; children take only syscall paths until exec or _exit.
    cv(unsafe { libc::fork() })
}
pub fn exit(code: i32) -> ! {
    // SAFETY: immediate process exit does not run inherited destructors.
    unsafe { libc::_exit(code) }
}
pub fn close(fd: i32) {
    // SAFETY: caller relinquishes this descriptor; close accepts negative fds too.
    unsafe {
        libc::close(fd);
    }
}
pub fn pair() -> io::Result<[i32; 2]> {
    let mut f = [-1; 2];
    // SAFETY: f provides two writable descriptors.
    cv(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            f.as_mut_ptr(),
        )
    })?;
    Ok(f)
}
pub fn read(fd: i32, b: &mut [u8]) -> isize {
    // SAFETY: b is writable for its full length.
    unsafe { libc::read(fd, b.as_mut_ptr().cast(), b.len()) }
}
pub fn write(fd: i32, b: &[u8]) -> isize {
    // SAFETY: b is readable for its full length.
    unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) }
}
pub fn poll(p: &mut [libc::pollfd], ms: i32) -> i32 {
    // SAFETY: p is a valid pollfd array.
    unsafe { libc::poll(p.as_mut_ptr(), p.len() as _, ms) }
}
pub fn pid() -> i32 {
    // SAFETY: getpid has no preconditions.
    unsafe { libc::getpid() }
}
pub fn group() -> i32 {
    // SAFETY: getpgrp has no preconditions.
    unsafe { libc::getpgrp() }
}
pub fn set_group(group: i32) -> io::Result<()> {
    // SAFETY: changes only the calling process group.
    cv(unsafe { libc::setpgid(0, group) }).map(|_| ())
}
pub fn subreaper() -> io::Result<()> {
    // SAFETY: prctl takes an integer for this operation.
    cv(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }).map(|_| ())
}
pub fn action(sig: i32, new: Option<&libc::sigaction>) -> io::Result<libc::sigaction> {
    // SAFETY: zero is a valid initial sigaction representation.
    let mut old = unsafe { std::mem::zeroed() };
    // SAFETY: pointers are either valid sigactions or null.
    cv(unsafe { libc::sigaction(sig, new.map_or(ptr::null(), |n| n), &mut old) })?;
    Ok(old)
}
pub fn disposition(sig: i32, handler: usize) {
    // SAFETY: zero initialized mask is subsequently initialized with sigemptyset.
    let mut a: libc::sigaction = unsafe { std::mem::zeroed() };
    a.sa_sigaction = handler;
    // SAFETY: a owns a valid mask.
    unsafe {
        libc::sigemptyset(&mut a.sa_mask);
    }
    let _ = action(sig, Some(&a));
}
pub fn default_sigpipe() {
    disposition(libc::SIGPIPE, libc::SIG_DFL);
}
pub fn ignore_sigpipe() {
    disposition(libc::SIGPIPE, libc::SIG_IGN);
}

pub fn terminal_columns() -> Option<usize> {
    // SAFETY: winsize is a C integer struct and zero is a valid representation.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: size is writable; ioctl validates stdout and the request.
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0).then_some(size.ws_col as usize)
}

pub fn local_time(ns: u64) -> io::Result<String> {
    let seconds: libc::time_t = (ns / 1_000_000_000).try_into().map_err(io::Error::other)?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: seconds is readable and local points to writable storage for tm.
    if unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful localtime_r initialized local.
    let local = unsafe { local.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    ))
}
fn mask(signals: &[i32]) -> libc::sigset_t {
    // SAFETY: the local mask is initialized before use.
    unsafe {
        let mut s = std::mem::zeroed();
        libc::sigemptyset(&mut s);
        for &sig in signals {
            libc::sigaddset(&mut s, sig);
        }
        s
    }
}
pub fn signal_fd(signals: &[i32]) -> io::Result<i32> {
    let m = mask(signals);
    // SAFETY: m is an initialized signal set.
    cv(unsafe { libc::signalfd(-1, &m, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) })
}
pub fn block(signals: &[i32]) -> io::Result<()> {
    let m = mask(signals);
    // SAFETY: m is initialized; no old mask requested.
    cv(unsafe { libc::sigprocmask(libc::SIG_BLOCK, &m, ptr::null_mut()) }).map(|_| ())
}
pub fn clear_mask() {
    let m = mask(&[]);
    // SAFETY: empty initialized set unblocks all signals.
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, &m, ptr::null_mut());
    }
}
pub fn signal_event(fd: i32) -> Option<i32> {
    let mut b = [0u8; 128];
    (read(fd, &mut b) == 128).then(|| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as i32)
}
pub fn kill(pid: i32, sig: i32) {
    // SAFETY: kernel validates pid and signal.
    unsafe {
        libc::kill(pid, sig);
    }
}
pub fn wait(pid: i32, blocking: bool) -> Option<(i32, i32)> {
    let mut st = 0;
    loop {
        // SAFETY: st is a valid writable wait status.
        let r = unsafe { libc::waitpid(pid, &mut st, if blocking { 0 } else { libc::WNOHANG }) };
        if r > 0 {
            return Some((r, st));
        }
        if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return None;
    }
}
pub fn reraise(sig: i32) -> i32 {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid resource limit structure.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &limit);
    }
    disposition(sig, libc::SIG_DFL);
    clear_mask();
    kill(pid(), sig);
    128 + sig
}
pub fn install(p: &[libc::sock_filter]) -> io::Result<i32> {
    let bpf = libc::sock_fprog {
        len: p.len() as u16,
        filter: p.as_ptr().cast_mut(),
    };
    // SAFETY: prctl accepts integer arguments here; bpf points to live filter instructions.
    unsafe {
        cv(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
        cv(libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &bpf,
        ) as i32)
    }
}
pub fn sync_wake(fd: i32) -> bool {
    // SAFETY: this ioctl takes flags by value, not a pointer.
    unsafe {
        libc::ioctl(
            fd,
            SECCOMP_IOCTL_NOTIF_SET_FLAGS,
            SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP,
        ) == 0
    }
}
pub fn serve(fd: i32) {
    let mut n = Notification::default();
    // SAFETY: n and r match the kernel ABI and are live for each ioctl.
    unsafe {
        if libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_RECV, &mut n) == 0 {
            let r = Response {
                id: n.id,
                flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
                ..Response::default()
            };
            libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_SEND, &r);
        }
    }
}
pub fn receive(fd: i32) -> io::Result<Notification> {
    let mut n = Notification::default();
    // SAFETY: n matches the kernel ABI and is writable for the duration of ioctl.
    cv(unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_RECV, &mut n) })?;
    Ok(n)
}
pub fn valid(fd: i32, id: u64) -> bool {
    // SAFETY: id is readable and has the ioctl's required u64 layout.
    unsafe { libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_ID_VALID, &id) == 0 }
}
pub fn answer(fd: i32, id: u64, deny: bool) {
    let r = Response {
        id,
        error: if deny { -libc::EPERM } else { 0 },
        flags: if deny {
            0
        } else {
            SECCOMP_USER_NOTIF_FLAG_CONTINUE
        },
        ..Response::default()
    };
    // SAFETY: r is live and matches the kernel response ABI. A vanished request is harmless.
    unsafe {
        libc::ioctl(fd, SECCOMP_IOCTL_NOTIF_SEND, &r);
    }
}
pub fn pass_fd(sock: i32, fd: Option<i32>) -> i32 {
    // SAFETY: msghdr, iovec and aligned ancillary storage remain live; lengths are bounded.
    unsafe {
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 4];
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &mut iov;
        m.msg_iovlen = 1;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = libc::CMSG_SPACE(4) as usize;
        if let Some(fd) = fd {
            let c = libc::CMSG_FIRSTHDR(&m);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as usize;
            ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd);
            if libc::sendmsg(sock, &m, libc::MSG_NOSIGNAL) == 1 {
                0
            } else {
                -1
            }
        } else {
            if libc::recvmsg(sock, &mut m, libc::MSG_CMSG_CLOEXEC) != 1 {
                return -1;
            }
            let c = libc::CMSG_FIRSTHDR(&m);
            if c.is_null()
                || (*c).cmsg_level != libc::SOL_SOCKET
                || (*c).cmsg_type != libc::SCM_RIGHTS
                || (*c).cmsg_len < libc::CMSG_LEN(4) as usize
            {
                return -1;
            }
            ptr::read_unaligned(libc::CMSG_DATA(c).cast::<i32>())
        }
    }
}
pub fn detach_stdio() {
    // SAFETY: constant terminated paths; dup2 replaces only standard streams.
    unsafe {
        let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd >= 0 {
            for to in 0..3 {
                libc::dup2(fd, to);
            }
            if fd > 2 {
                libc::close(fd);
            }
        } else {
            for fd in 0..3 {
                libc::close(fd);
            }
        }
        libc::chdir(c"/".as_ptr());
    }
}
pub fn exec(
    program: &CStr,
    argv: &[*const libc::c_char],
    shell_argv: &[*const libc::c_char],
    env: &[*const libc::c_char],
) -> ! {
    // SAFETY: all strings and null terminated pointer arrays were prepared before fork.
    unsafe {
        libc::execve(program.as_ptr(), argv.as_ptr(), env.as_ptr());
    }
    let mut error = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO);
    if error == libc::ENOEXEC {
        // SAFETY: shell pointer array was prepared and null terminated before fork.
        unsafe {
            libc::execve(c"/bin/sh".as_ptr(), shell_argv.as_ptr(), env.as_ptr());
        }
        error = io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
    }
    write(2, b"saqta: cannot execute command\n");
    exit(if error == libc::ENOENT || error == libc::ENOTDIR {
        127
    } else {
        126
    })
}
pub fn executable(path: &CStr) -> bool {
    // SAFETY: path is terminated and readable.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}
pub fn monotonic_ms() -> i64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid writable timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t);
    }
    t.tv_sec * 1000 + t.tv_nsec / 1_000_000
}
// Stack-only timestamp logging is also safe immediately before S re-execs.
pub fn debug_timestamp(fd: i32, phase: &[u8]) {
    const MAX_PHASE_BYTES: usize = 64;
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    const DECIMAL_DIGITS: usize = 20;
    if fd < 0 || phase.len() > MAX_PHASE_BYTES {
        return;
    }
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: t is writable; clock_gettime is async-signal-safe.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return;
    }
    let mut ns = (t.tv_sec as u64)
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(t.tv_nsec as u64);
    let mut digits = [0u8; DECIMAL_DIGITS];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (ns % 10) as u8;
        count += 1;
        ns /= 10;
        if ns == 0 {
            break;
        }
    }
    let mut line = [0u8; MAX_PHASE_BYTES + DECIMAL_DIGITS + 2];
    line[..phase.len()].copy_from_slice(phase);
    line[phase.len()] = b'\t';
    for i in 0..count {
        line[phase.len() + 1 + i] = digits[count - 1 - i];
    }
    let end = phase.len() + 1 + count;
    line[end] = b'\n';
    write(fd, &line[..end + 1]);
}
// Decimal formatting without allocation, also used after fork.
fn decimal(mut n: i32, out: &mut [u8]) -> usize {
    let mut digits = [0u8; 12];
    let mut k = 0;
    loop {
        digits[k] = b'0' + (n % 10) as u8;
        k += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for i in 0..k {
        out[i] = digits[k - 1 - i];
    }
    k
}
pub fn marker(prefix: &[u8], create: bool) {
    let mut path = [0u8; 4096];
    if prefix.len() + 20 >= path.len() {
        return;
    }
    path[..prefix.len()].copy_from_slice(prefix);
    let k = decimal(pid(), &mut path[prefix.len()..]);
    path[prefix.len() + k] = 0;
    // SAFETY: stack path is null terminated; marker is private to this pid.
    unsafe {
        if create {
            let fd = libc::open(
                path.as_ptr().cast(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                0o600,
            );
            if fd >= 0 {
                libc::close(fd);
            }
        } else {
            libc::unlink(path.as_ptr().cast());
        }
    }
}
// Snapshot /proc using getdents64. No allocator, stdio or directory locks after fork.
fn snapshot(rows: &mut [(i32, i32)]) -> usize {
    // SAFETY: constant terminated path.
    let fd = unsafe {
        libc::open(
            c"/proc".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return 0;
    }
    let mut buf = [0u8; 32768];
    let mut count = 0;
    loop {
        // SAFETY: buf is writable and kernel returns bounded directory records.
        let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 {
            break;
        }
        let mut at = 0;
        while at + 19 < n as usize {
            let len = u16::from_ne_bytes([buf[at + 16], buf[at + 17]]) as usize;
            if len < 20 || at + len > n as usize {
                break;
            }
            let name = &buf[at + 19..at + len];
            let mut p = 0i32;
            for &b in name {
                if b == 0 {
                    break;
                }
                if !b.is_ascii_digit() {
                    p = 0;
                    break;
                }
                p = p.saturating_mul(10).saturating_add((b - b'0') as i32);
            }
            if p > 0 && count < rows.len() {
                let mut path = [0u8; 64];
                let k = decimal(p, &mut path);
                path[k..k + 6].copy_from_slice(b"/stat\0");
                // SAFETY: path is terminated; fd refers to /proc.
                let f = unsafe {
                    libc::openat(fd, path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC)
                };
                if f >= 0 {
                    let mut stat = [0u8; 1024];
                    let n = read(f, &mut stat);
                    close(f);
                    if n > 0
                        && let Some(end) = stat[..n as usize].iter().rposition(|&b| b == b')')
                    {
                        let mut pp = 0i32;
                        for &b in stat.get(end + 4..n as usize).unwrap_or(&[]) {
                            if !b.is_ascii_digit() {
                                break;
                            }
                            pp = pp.saturating_mul(10).saturating_add((b - b'0') as i32);
                        }
                        rows[count] = (p, pp);
                        count += 1;
                    }
                }
            }
            at += len;
        }
    }
    close(fd);
    count
}
pub fn tree_signal(sig: i32) -> (usize, u64) {
    let mut rows = [(0i32, 0i32); 65536];
    let n = snapshot(&mut rows);
    let root = pid();
    // Mark descendants by negating their parent; repeated passes handle arbitrary ordering.
    loop {
        let mut changed = false;
        for i in 0..n {
            if rows[i].1 > 0
                && (rows[i].1 == root || rows[..n].iter().any(|r| r.0 == rows[i].1 && r.1 < 0))
            {
                rows[i].1 = -rows[i].1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let (mut count, mut fingerprint) = (0, 0u64);
    for &(p, pp) in &rows[..n] {
        if pp < 0 {
            kill(p, sig);
            count += 1;
            fingerprint = fingerprint.wrapping_add(p as u64);
        }
    }
    (count, fingerprint)
}
pub fn kill_tree() {
    // Stop roots first, then rescan to catch forks that raced the first snapshot.
    let mut previous = (usize::MAX, 0);
    loop {
        let current = tree_signal(libc::SIGSTOP);
        if current == previous {
            break;
        }
        previous = current;
    }
    tree_signal(libc::SIGKILL);
}
pub fn filesystem(path: &CString) -> io::Result<(i64, u64)> {
    // SAFETY: zero is valid storage for statfs output; path is terminated.
    unsafe {
        let mut s = std::mem::zeroed();
        cv(libc::statfs(path.as_ptr(), &mut s))?;
        Ok((s.f_type, s.f_bavail.saturating_mul(s.f_bsize as u64)))
    }
}
pub fn probe(bpf: &[libc::sock_filter]) -> io::Result<bool> {
    let sockets = pair()?;
    let child = fork()?;
    if child == 0 {
        close(sockets[0]);
        let fd = match install(bpf) {
            Ok(fd) => fd,
            Err(e) => exit(e.raw_os_error().unwrap_or(125)),
        };
        if pass_fd(sockets[1], Some(fd)) < 0 {
            exit(125);
        }
        close(fd);
        close(sockets[1]);
        // SAFETY: constant invalid path cannot remove anything; ENOTDIR proves CONTINUE ran it.
        let r = unsafe { libc::unlink(c"/dev/null/saqta-doctor".as_ptr()) };
        exit(
            if r == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTDIR) {
                0
            } else {
                125
            },
        );
    }
    close(sockets[1]);
    let fd = pass_fd(sockets[0], None);
    close(sockets[0]);
    let sync = if fd >= 0 {
        let sync = sync_wake(fd);
        let mut pollfd = [libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        if poll(&mut pollfd, 2000) > 0 {
            serve(fd);
        } else {
            kill(child, libc::SIGKILL);
        }
        close(fd);
        sync
    } else {
        false
    };
    let (_, st) =
        wait(child, true).ok_or_else(|| io::Error::other("probe child status unavailable"))?;
    if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 {
        Ok(sync)
    } else {
        Err(io::Error::from_raw_os_error(if libc::WIFEXITED(st) {
            libc::WEXITSTATUS(st)
        } else {
            libc::EIO
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ioctl_numbers_match_kernel_abi() {
        assert_eq!(SECCOMP_IOCTL_NOTIF_RECV, 0xc0502100);
        assert_eq!(SECCOMP_IOCTL_NOTIF_SEND, 0xc0182101);
        assert_eq!(SECCOMP_IOCTL_NOTIF_ID_VALID, 0x40082102);
        assert_eq!(SECCOMP_IOCTL_NOTIF_SET_FLAGS, 0x40082104);
    }
    #[test]
    fn abi_sizes() {
        assert_eq!(std::mem::size_of::<SeccompData>(), 64);
        assert_eq!(std::mem::size_of::<Notification>(), 80);
        assert_eq!(std::mem::size_of::<Response>(), 24);
    }
}
