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
pub struct PathReader<'a> {
    pid: i32,
    page: usize,
    unavailable: &'a mut bool,
    mem: &'a mut Option<File>,
    buffer: [u8; libc::PATH_MAX as usize],
    at: u64,
    length: usize,
}
impl<'a> PathReader<'a> {
    pub fn new(
        pid: i32,
        page: usize,
        unavailable: &'a mut bool,
        mem: &'a mut Option<File>,
    ) -> Self {
        Self {
            pid,
            page,
            unavailable,
            mem,
            buffer: [0; libc::PATH_MAX as usize],
            at: 0,
            length: 0,
        }
    }
    pub fn read(&mut self, address: u64) -> io::Result<OsString> {
        match self.read_once(address) {
            Err(_) if self.mem.is_some() => {
                *self.mem = None;
                self.length = 0;
                self.read_once(address)
            }
            result => result,
        }
    }
    fn read_page(&mut self, address: u64) -> io::Result<()> {
        let page_at = address - address % self.page as u64;
        self.at = if address - page_at < self.buffer.len() as u64 {
            page_at
        } else {
            address
        };
        let count = (self.page - self.at as usize % self.page).min(self.buffer.len());
        if *self.unavailable && self.mem.is_none() {
            *self.mem = Some(File::open(format!("/proc/{}/mem", self.pid))?);
        }
        let slice = &mut self.buffer[..count];
        self.length = if let Some(mem) = &self.mem {
            mem.read_at(slice, self.at)?
        } else {
            match read_vm(self.pid, self.at, slice) {
                Ok(n) => n,
                Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::ENOSYS)) => {
                    *self.unavailable = true;
                    let mem = File::open(format!("/proc/{}/mem", self.pid))?;
                    let n = mem.read_at(slice, self.at)?;
                    *self.mem = Some(mem);
                    n
                }
                Err(e) => return Err(e),
            }
        };
        Ok(())
    }
    fn read_once(&mut self, address: u64) -> io::Result<OsString> {
        let mut bytes = Vec::with_capacity(128);
        while bytes.len() < self.buffer.len() {
            let at = address
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
            if at < self.at || at - self.at >= self.length as u64 {
                self.read_page(at)?;
            }
            let offset = at.saturating_sub(self.at) as usize;
            if at < self.at || offset >= self.length {
                return Err(io::Error::from_raw_os_error(libc::EFAULT));
            }
            let count = (self.length - offset).min(self.buffer.len() - bytes.len());
            let slice = &self.buffer[offset..offset + count];
            if let Some(end) = slice.iter().position(|&b| b == 0) {
                bytes.extend_from_slice(&slice[..end]);
                return Ok(OsString::from_vec(bytes));
            }
            bytes.extend_from_slice(slice);
        }
        Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
    }
}
