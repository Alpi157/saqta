use std::{
    ffi::OsString,
    fs::File,
    io,
    os::unix::{ffi::OsStringExt, fs::FileExt},
};

pub fn page_size() -> io::Result<usize> {
    // SAFETY: sysconf takes a constant and has no pointer arguments.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        Ok(size as usize)
    } else {
        Err(io::Error::last_os_error())
    }
}
fn read_vm(pid: i32, address: u64, buffer: &mut [u8]) -> io::Result<usize> {
    let local = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let remote = libc::iovec {
        iov_base: address as usize as *mut libc::c_void,
        iov_len: buffer.len(),
    };
    // SAFETY: local covers buffer; remote is interpreted only by the kernel in pid's address space.
    let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
pub fn read_path(pid: i32, address: u64, force_proc: bool, page: usize) -> io::Result<OsString> {
    let mut bytes = Vec::new();
    let mut mem = if force_proc {
        Some(File::open(format!("/proc/{pid}/mem"))?)
    } else {
        None
    };
    let mut buffer = [0u8; libc::PATH_MAX as usize];
    while bytes.len() < buffer.len() {
        let at = address
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let count = (page - at as usize % page).min(buffer.len() - bytes.len());
        let slice = &mut buffer[..count];
        let n = if let Some(mem) = &mem {
            mem.read_at(slice, at)?
        } else {
            match read_vm(pid, at, slice) {
                Ok(n) => n,
                Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::ENOSYS)) => {
                    let f = File::open(format!("/proc/{pid}/mem"))?;
                    let n = f.read_at(slice, at)?;
                    mem = Some(f);
                    n
                }
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            return Err(io::Error::from_raw_os_error(libc::EFAULT));
        }
        if let Some(end) = slice[..n].iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&slice[..end]);
            return Ok(OsString::from_vec(bytes));
        }
        bytes.extend_from_slice(&slice[..n]);
    }
    Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
}
