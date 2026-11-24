//! Task identity, memory and filesystem context. Every context call starts a new epoch.
use super::*;
use std::os::unix::fs::FileExt;

pub(super) struct ResolutionRoot {
    pub(super) fd: Rc<OwnedFd>,
    pub(super) path: PathBuf,
}
impl ResolutionRoot {
    pub(super) fn new(pid: u32) -> io::Result<Self> {
        let fd = sys::open_at(
            libc::AT_FDCWD,
            &cstr(OsStr::new(&format!("/proc/{pid}/root")))?,
            libc::O_PATH | libc::O_DIRECTORY,
            0,
        )?;
        let path = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        Ok(Self {
            fd: Rc::new(fd),
            path,
        })
    }
    pub(super) fn resolve(
        &self,
        pid: u32,
        dirfd: i32,
        path: &OsStr,
        follow: bool,
    ) -> io::Result<Resolved> {
        let base_path = if path.as_bytes().starts_with(b"/") {
            self.path.clone()
        } else {
            let base = if dirfd == libc::AT_FDCWD {
                format!("/proc/{pid}/cwd")
            } else {
                format!("/proc/{pid}/fd/{dirfd}")
            };
            let base = sys::open_at(
                libc::AT_FDCWD,
                &cstr(OsStr::new(&base))?,
                libc::O_PATH | libc::O_DIRECTORY,
                0,
            )?;
            fs::read_link(format!("/proc/self/fd/{}", base.as_raw_fd()))?
        };
        resolve_from(self, &base_path, path, follow, 0)
    }
}
pub(super) struct Base {
    pub(super) dir: i32,
    pub(super) fd: Rc<OwnedFd>,
    pub(super) path: PathBuf,
    pub(super) stat: libc::stat,
    pub(super) mount: Option<u64>,
}
pub(super) struct TaskContext {
    pub(super) pid: u32,
    stat: File,
    pub(super) pidfd: Option<OwnedFd>,
    start: u64,
    pub(super) mem: Option<File>,
    pub(super) root: Option<ResolutionRoot>,
    pub(super) cwd: Option<Base>,
}
impl TaskContext {
    pub(super) fn new(pid: u32) -> io::Result<Self> {
        let stat = File::open(format!("/proc/{pid}/stat"))?;
        let start = Self::start(&stat)?;
        let pidfd = sys::pidfd_open(pid).ok();
        #[cfg(debug_assertions)]
        let pidfd = if std::env::var_os("SAQTA_TEST_NO_PIDFD").is_some_and(|s| s == "1") {
            None
        } else {
            pidfd
        };
        Ok(Self {
            pid,
            pidfd,
            stat,
            start,
            mem: None,
            root: None,
            cwd: None,
        })
    }
    fn start(stat: &File) -> io::Result<u64> {
        let mut bytes = [0u8; 4096];
        let n = stat.read_at(&mut bytes, 0)?;
        let end = bytes[..n]
            .iter()
            .rposition(|&b| b == b')')
            .ok_or_else(|| io::Error::other("task exited during identity check"))?;
        bytes[end + 1..n]
            .split(|b| b.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .nth(19)
            .ok_or_else(|| io::Error::other("short task identity"))
            .and_then(|bytes| std::str::from_utf8(bytes).map_err(io::Error::other))
            .and_then(|text| text.parse().map_err(io::Error::other))
    }
    fn live(&self, pid: u32) -> bool {
        if self.pid != pid {
            return false;
        }
        if let Some(fd) = &self.pidfd {
            let mut watch = [libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            return sys::poll(&mut watch, 0) == 0;
        }
        Self::start(&self.stat).is_ok_and(|start| start == self.start)
    }
}
pub(super) struct TaskCache {
    pub(super) eligible: Option<u32>,
    pub(super) task: Option<TaskContext>,
}
impl TaskCache {
    pub(super) fn invalidate(&mut self, pid: u32) {
        self.task = None;
        self.eligible = Some(pid);
    }
    pub(super) fn take(&mut self, pid: u32) -> io::Result<TaskContext> {
        if let Some(task) = self.task.take()
            && task.live(pid)
        {
            return Ok(task);
        }
        TaskContext::new(pid)
    }
    pub(super) fn prune(&mut self) {
        if self.task.as_ref().is_some_and(|task| !task.live(task.pid)) {
            self.task = None;
        }
    }
    pub(super) fn put(&mut self, task: TaskContext) {
        if self.eligible == Some(task.pid) {
            self.task = Some(task);
        }
    }
}
pub(super) struct NotificationResolver {
    pub(super) pid: u32,
    pub(super) root: Option<ResolutionRoot>,
    pub(super) root_display: Option<PathBuf>,
    pub(super) base: Option<Base>,
}
impl NotificationResolver {
    pub(super) fn new(pid: u32) -> Self {
        Self {
            pid,
            root: None,
            root_display: None,
            base: None,
        }
    }
    pub(super) fn resolve(&mut self, dir: i32, path: &OsStr, follow: bool) -> io::Result<Resolved> {
        let mut bytes = path.as_bytes();
        while let Some(rest) = bytes.strip_prefix(b"./") {
            bytes = rest;
        }
        if !bytes.is_empty() && bytes != b"." && bytes != b".." && !bytes.contains(&b'/') {
            if self.base.as_ref().is_none_or(|base| base.dir != dir) {
                let proc = if dir == libc::AT_FDCWD {
                    format!("/proc/{}/cwd", self.pid)
                } else {
                    format!("/proc/{}/fd/{dir}", self.pid)
                };
                let fd = sys::open_at(
                    libc::AT_FDCWD,
                    &cstr(OsStr::new(&proc))?,
                    libc::O_PATH | libc::O_DIRECTORY,
                    0,
                )?;
                let name = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
                let (stat, mount) = sys::stat_mount_fd(fd.as_raw_fd())?;
                self.base = Some(Base {
                    dir,
                    fd: Rc::new(fd),
                    path: name,
                    stat,
                    mount,
                });
            }
            if self.root_display.is_none() {
                self.root_display = Some(fs::read_link(format!("/proc/{}/root", self.pid))?);
            }
            if let (Some(base), Some(root)) = (&self.base, &self.root_display) {
                base.path.strip_prefix(root).map_err(io::Error::other)?;
                let fd = &base.fd;
                let parent = &base.stat;
                let name = cstr(OsStr::from_bytes(bytes))?;
                let (stat, mount) = sys::stat_mount_at(fd.as_raw_fd(), &name)?;
                if !follow
                    || !stat
                        .as_ref()
                        .is_some_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFLNK)
                {
                    let mut canonical = base.path.clone();
                    if parent.st_nlink == 0
                        && let Some(b) =
                            canonical.as_os_str().as_bytes().strip_suffix(b" (deleted)")
                    {
                        canonical = PathBuf::from(OsString::from_vec(b.to_vec()));
                    }
                    return Ok(Resolved {
                        parent: Rc::clone(fd),
                        name,
                        path: canonical.join(OsStr::from_bytes(bytes)),
                        mount: if stat.is_some() { mount } else { base.mount },
                        stat,
                        requests: if parent.st_nlink == 0 {
                            vec![base.path.join(path)]
                        } else {
                            Vec::new()
                        },
                        parent_dev: parent.st_dev,
                        incarnation: std::cell::OnceCell::new(),
                    });
                }
            }
        }
        if self.root.is_none() {
            self.root = Some(ResolutionRoot::new(self.pid)?);
        }
        self.root
            .as_ref()
            .ok_or_else(|| io::Error::other("missing pinned root"))?
            .resolve(self.pid, dir, path, follow)
    }
}
pub fn resolve(pid: u32, dirfd: i32, path: &OsStr, follow: bool) -> io::Result<Resolved> {
    NotificationResolver::new(pid).resolve(dirfd, path, follow)
}
fn resolve_from(
    root: &ResolutionRoot,
    base_path: &Path,
    path: &OsStr,
    follow: bool,
    depth: usize,
) -> io::Result<Resolved> {
    if depth > MAX_SYMLINKS {
        return Err(io::Error::from_raw_os_error(libc::ELOOP));
    }
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let raw = path.as_bytes();
    let raw = &raw[raw.iter().position(|&b| b != b'/').unwrap_or(raw.len())..];
    let raw = if raw.is_empty() { b".".as_slice() } else { raw };
    let end = raw.iter().rposition(|&b| b != b'/').map_or(0, |n| n + 1);
    let raw = if end == 0 {
        b".".as_slice()
    } else {
        &raw[..end]
    };
    let (parent, last) = match raw.iter().rposition(|&b| b == b'/') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (b".".as_slice(), raw),
    };
    let base_relative = base_path
        .strip_prefix(&root.path)
        .map_err(io::Error::other)?;
    let parent_path = base_relative.join(OsStr::from_bytes(parent));
    let parent = sys::open_target_parent(root.fd.as_raw_fd(), &cstr(parent_path.as_os_str())?)?;
    let requested = base_path.join(OsStr::from_bytes(raw));
    let name = cstr(OsStr::from_bytes(last))?;
    let (stat, mount) = sys::stat_mount_at(parent.as_raw_fd(), &name)?;
    let mut canonical = fs::read_link(format!("/proc/self/fd/{}", parent.as_raw_fd()))?;
    let (parent_stat, parent_mount) = sys::stat_mount_fd(parent.as_raw_fd())?;
    if parent_stat.st_nlink == 0 {
        let bytes = canonical.as_os_str().as_bytes();
        if let Some(b) = bytes.strip_suffix(b" (deleted)") {
            canonical = PathBuf::from(OsString::from_vec(b.to_vec()));
        }
    }
    let result = Resolved {
        parent: Rc::new(parent),
        name,
        path: canonical.join(OsStr::from_bytes(last)),
        mount: if stat.is_some() { mount } else { parent_mount },
        stat,
        requests: vec![requested],
        parent_dev: parent_stat.st_dev,
        incarnation: std::cell::OnceCell::new(),
    };
    if follow
        && result
            .stat
            .as_ref()
            .is_some_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFLNK)
    {
        let target = fs::read_link(result.proc_path())?;
        let mut followed = if target.is_absolute() {
            resolve_from(root, &root.path, target.as_os_str(), true, depth + 1)?
        } else {
            let base = result
                .path
                .parent()
                .ok_or_else(|| io::Error::other("missing symlink parent"))?;
            resolve_from(root, base, target.as_os_str(), true, depth + 1)?
        };
        followed.requests.extend(result.requests);
        followed.requests.push(result.path);
        return Ok(followed);
    }
    Ok(result)
}
