//! Thin signal wrapper shared by integration-test process cleanup.
use std::io;

pub fn signal(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: kill takes integer values only. Test callers own the process or
    // process group; cleanup additionally checks each process's start time.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
