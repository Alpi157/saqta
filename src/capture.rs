//! Decode a stopped syscall, pin its parent directories, and journal its pre-images.
use crate::{
    journal::{self, Detail, Effect, Journal, State},
    sys,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
};

const DEFAULT_MAX_COPY: u64 = 256 * 1024 * 1024;
const MAX_SYMLINKS: usize = 40;
// linux/magic.h constants missing from libc on this target.
const RAMFS_MAGIC: i64 = 0x858458f6;
const PSTOREFS_MAGIC: i64 = 0x6165676c;
const CONFIGFS_MAGIC: i64 = 0x62656570;
const FUSECTL_MAGIC: i64 = 0x65735543;
const MQUEUE_MAGIC: i64 = 0x19800202;
const BINFMTFS_MAGIC: i64 = 0x42494e4d;
const SKIPPED_FILESYSTEMS: &[i64] = &[
    libc::PROC_SUPER_MAGIC,
    libc::SYSFS_MAGIC,
    libc::CGROUP_SUPER_MAGIC,
    libc::CGROUP2_SUPER_MAGIC,
    libc::TMPFS_MAGIC,
    RAMFS_MAGIC,
    libc::DEVPTS_SUPER_MAGIC,
    libc::DEBUGFS_MAGIC,
    libc::TRACEFS_MAGIC,
    libc::SECURITYFS_MAGIC,
    libc::BPF_FS_MAGIC,
    PSTOREFS_MAGIC,
    CONFIGFS_MAGIC,
    FUSECTL_MAGIC,
    MQUEUE_MAGIC,
    libc::HUGETLBFS_MAGIC,
    BINFMTFS_MAGIC,
];
fn cstr(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes()).map_err(io::Error::other)
}
pub struct Resolved {
    pub parent: OwnedFd,
    pub name: CString,
    pub path: PathBuf,
    pub stat: Option<libc::stat>,
    requests: Vec<PathBuf>,
}
impl Resolved {
    pub fn state(&self) -> Option<State> {
        self.stat.as_ref().map(State::from_stat)
    }
    fn skipped(&self) -> io::Result<bool> {
        let fd = if self.stat.is_some() {
            Some(sys::open_at(
                self.parent.as_raw_fd(),
                &self.name,
                libc::O_PATH | libc::O_NOFOLLOW,
                0,
            )?)
        } else {
            None
        };
        Ok(SKIPPED_FILESYSTEMS.contains(&sys::fs_type(
            fd.as_ref()
                .map_or(self.parent.as_raw_fd(), AsRawFd::as_raw_fd),
        )?))
    }
    fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.parent.as_raw_fd()))
            .join(OsStr::from_bytes(self.name.to_bytes()))
    }
}
pub fn resolve(pid: u32, dirfd: i32, path: &OsStr, follow: bool) -> io::Result<Resolved> {
    let base = if path.as_bytes().starts_with(b"/") {
        format!("/proc/{pid}/root")
    } else if dirfd == libc::AT_FDCWD {
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
    resolve_from(pid, base, path, follow, 0)
}
fn resolve_from(
    pid: u32,
    base: OwnedFd,
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
    let root = sys::open_at(
        libc::AT_FDCWD,
        &cstr(OsStr::new(&format!("/proc/{pid}/root")))?,
        libc::O_PATH | libc::O_DIRECTORY,
        0,
    )?;
    let root_path = fs::read_link(format!("/proc/self/fd/{}", root.as_raw_fd()))?;
    let base_path = fs::read_link(format!("/proc/self/fd/{}", base.as_raw_fd()))?;
    let base_relative = base_path
        .strip_prefix(&root_path)
        .map_err(io::Error::other)?;
    let parent_path = base_relative.join(OsStr::from_bytes(parent));
    let parent = sys::open_target_parent(root.as_raw_fd(), &cstr(parent_path.as_os_str())?)?;
    let requested = base_path.join(OsStr::from_bytes(raw));
    let name = cstr(OsStr::from_bytes(last))?;
    let stat = sys::stat_at(parent.as_raw_fd(), &name)?;
    let mut canonical = fs::read_link(format!("/proc/self/fd/{}", parent.as_raw_fd()))?;
    if sys::stat_fd(parent.as_raw_fd())?.st_nlink == 0 {
        let bytes = canonical.as_os_str().as_bytes();
        if let Some(b) = bytes.strip_suffix(b" (deleted)") {
            canonical = PathBuf::from(OsString::from_vec(b.to_vec()));
        }
    }
    let result = Resolved {
        parent,
        name,
        path: canonical.join(OsStr::from_bytes(last)),
        stat,
        requests: vec![requested],
    };
    if follow
        && result
            .stat
            .as_ref()
            .is_some_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFLNK)
    {
        let target = fs::read_link(result.proc_path())?;
        let mut followed = if target.is_absolute() {
            let root = sys::open_at(
                libc::AT_FDCWD,
                &cstr(OsStr::new(&format!("/proc/{pid}/root")))?,
                libc::O_PATH | libc::O_DIRECTORY,
                0,
            )?;
            resolve_from(pid, root, target.as_os_str(), true, depth + 1)?
        } else {
            resolve_from(pid, result.parent, target.as_os_str(), true, depth + 1)?
        };
        followed.requests.extend(result.requests);
        followed.requests.push(result.path);
        return Ok(followed);
    }
    Ok(result)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Truncate,
    Delete,
    Rmdir,
    Create,
    Rename,
    Exchange,
    Link,
}
struct Call {
    kind: Kind,
    dir: i32,
    address: u64,
    dir2: i32,
    address2: Option<u64>,
    flags: u64,
    size: u64,
    follow: bool,
}
impl Call {
    fn decode(n: &sys::Notification) -> io::Result<Self> {
        let a = n.data.args;
        let mut c = Self {
            kind: Kind::Create,
            dir: libc::AT_FDCWD,
            address: a[0],
            dir2: libc::AT_FDCWD,
            address2: None,
            flags: 0,
            size: 0,
            follow: false,
        };
        match n.data.nr as i64 {
            libc::SYS_open => {
                c.kind = Kind::Open;
                c.flags = a[1];
            }
            libc::SYS_openat => {
                c.kind = Kind::Open;
                c.dir = a[0] as i32;
                c.address = a[1];
                c.flags = a[2];
            }
            libc::SYS_creat => {
                c.kind = Kind::Open;
                c.flags = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as u64;
            }
            libc::SYS_truncate => {
                c.kind = Kind::Truncate;
                c.size = a[1];
                c.follow = true;
            }
            libc::SYS_unlink => c.kind = Kind::Delete,
            libc::SYS_rmdir => c.kind = Kind::Rmdir,
            libc::SYS_unlinkat => {
                c.dir = a[0] as i32;
                c.address = a[1];
                c.flags = a[2];
                c.kind = if a[2] & libc::AT_REMOVEDIR as u64 != 0 {
                    Kind::Rmdir
                } else {
                    Kind::Delete
                };
            }
            libc::SYS_rename => {
                c.kind = Kind::Rename;
                c.address2 = Some(a[1]);
            }
            libc::SYS_renameat | libc::SYS_renameat2 => {
                c.kind = Kind::Rename;
                c.dir = a[0] as i32;
                c.address = a[1];
                c.dir2 = a[2] as i32;
                c.address2 = Some(a[3]);
                if n.data.nr as i64 == libc::SYS_renameat2 {
                    c.flags = a[4];
                    if c.flags & libc::RENAME_EXCHANGE as u64 != 0 {
                        c.kind = Kind::Exchange;
                    }
                }
            }
            libc::SYS_mkdir | libc::SYS_mknod => {}
            libc::SYS_mkdirat | libc::SYS_mknodat => {
                c.dir = a[0] as i32;
                c.address = a[1];
            }
            libc::SYS_symlink => c.address = a[1],
            libc::SYS_symlinkat => {
                c.dir = a[1] as i32;
                c.address = a[2];
            }
            libc::SYS_link => {
                c.kind = Kind::Link;
                c.address2 = Some(a[1]);
            }
            libc::SYS_linkat => {
                c.kind = Kind::Link;
                c.dir = a[0] as i32;
                c.address = a[1];
                c.dir2 = a[2] as i32;
                c.address2 = Some(a[3]);
                c.flags = a[4];
            }
            _ => return Err(io::Error::other("unexpected notification")),
        }
        if c.kind == Kind::Open {
            c.follow = c.flags & (libc::O_NOFOLLOW | libc::O_EXCL) as u64 == 0;
        }
        Ok(c)
    }
}
struct Pending {
    seqs: Vec<u64>,
    kind: Kind,
    path: PathBuf,
    path2: Option<PathBuf>,
    before: Option<State>,
    before2: Option<State>,
}
pub struct Capture {
    pub journal: Journal,
    qfd: OwnedFd,
    home: PathBuf,
    user_home: PathBuf,
    max_copy: u64,
    force_proc: bool,
    page: usize,
    copied: HashMap<(u64, u64), (String, State)>,
    created: HashSet<PathBuf>,
    pending: HashMap<u32, Pending>,
    linked: BTreeMap<String, State>,
}
impl Capture {
    pub fn new(journal: Journal) -> io::Result<Self> {
        let qfd = sys::open_at(
            libc::AT_FDCWD,
            &cstr(journal.quarantine.as_os_str())?,
            libc::O_PATH | libc::O_DIRECTORY,
            0,
        )?;
        let home = fs::canonicalize(&journal.home)?;
        let user_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|p| fs::canonicalize(p).ok())
            .unwrap_or_default();
        let max_copy = match std::env::var("SAQTA_MAX_COPY") {
            Ok(s) => s.parse().map_err(io::Error::other)?,
            Err(_) => DEFAULT_MAX_COPY,
        };
        Ok(Self {
            journal,
            qfd,
            home,
            user_home,
            max_copy,
            force_proc: std::env::var_os("SAQTA_FORCE_PROC_MEM").is_some_and(|s| s == "1"),
            page: sys::page_size()?,
            copied: HashMap::new(),
            created: HashSet::new(),
            pending: HashMap::new(),
            linked: BTreeMap::new(),
        })
    }
    pub fn handle(&mut self, listener: i32, n: &sys::Notification) {
        self.complete(n.pid);
        let observed: Vec<_> = self
            .pending
            .iter()
            .filter(|(pid, p)| Self::applied(p) || !sys::alive(**pid as i32))
            .map(|(pid, _)| *pid)
            .collect();
        for pid in observed {
            self.complete(pid);
        }
        let result = self.read_call(listener, n);
        match result {
            Ok(Some((call, p, p2))) => {
                let deny = self.denied(&p, call.kind)
                    || p2.as_ref().is_some_and(|p| self.denied(p, call.kind));
                if deny {
                    let mut e = self.effect(n, "blocked", &p, p2.as_ref(), call.flags);
                    e.note.message = "deny floor: protected path".into();
                    if let Err(err) = self.journal.append(e) {
                        self.journal.error(&err);
                    }
                    sys::answer(listener, n.id, true);
                    return;
                }
                if let Err(e) = self.capture(n, &call, &p, p2.as_ref()) {
                    self.failure(n, Some(&p), &e);
                }
                sys::answer(listener, n.id, false);
            }
            Ok(None) => {} // ID is stale. Never act on its arguments.
            Err(e) => {
                if sys::valid(listener, n.id) {
                    self.failure(n, None, &e);
                    sys::answer(listener, n.id, false);
                }
            }
        }
    }
    fn read_call(
        &self,
        listener: i32,
        n: &sys::Notification,
    ) -> io::Result<Option<(Call, Resolved, Option<Resolved>)>> {
        let call = Call::decode(n)?;
        let p = sys::read_path(n.pid as i32, call.address, self.force_proc, self.page)?;
        let p2 = call
            .address2
            .map(|a| sys::read_path(n.pid as i32, a, self.force_proc, self.page))
            .transpose()?;
        if !sys::valid(listener, n.id) {
            return Ok(None);
        }
        let p = resolve(n.pid, call.dir, &p, call.follow)?;
        let p2 = p2
            .map(|p| resolve(n.pid, call.dir2, &p, false))
            .transpose()?;
        if !sys::valid(listener, n.id) {
            return Ok(None);
        }
        Ok(Some((call, p, p2)))
    }
    fn denied(&self, p: &Resolved, kind: Kind) -> bool {
        std::iter::once(&p.path)
            .chain(p.requests.iter())
            .any(|path| self.denied_path(path, kind, p.state().is_some_and(|s| s.directory())))
    }
    fn denied_path(&self, path: &Path, kind: Kind, directory: bool) -> bool {
        if path.starts_with(&self.home) {
            return true;
        }
        if matches!(kind, Kind::Rmdir | Kind::Rename | Kind::Exchange)
            && directory
            && self.home.starts_with(path)
        {
            return true;
        }
        let name = path.file_name().and_then(OsStr::to_str);
        if matches!(name, Some("settings.json" | "settings.local.json"))
            && path.parent().and_then(Path::file_name) == Some(OsStr::new(".claude"))
        {
            return true;
        }
        path == self.user_home.join(".codex/config.toml")
            || path == self.user_home.join(".codex/hooks.json")
    }
    fn effect(
        &self,
        n: &sys::Notification,
        op: &str,
        p: &Resolved,
        p2: Option<&Resolved>,
        flags: u64,
    ) -> Effect {
        Effect {
            seq: 0,
            time_ns: journal::now(),
            pid: n.pid,
            op: op.into(),
            path: journal::display(&p.path),
            path2: p2.map(|p| journal::display(&p.path)).unwrap_or_default(),
            quarantine: String::new(),
            flags,
            note: Detail {
                before: p.state(),
                before2: p2.and_then(Resolved::state),
                ..Detail::default()
            },
        }
    }
    fn failure(&mut self, n: &sys::Notification, p: Option<&Resolved>, err: &io::Error) {
        let e = Effect {
            seq: 0,
            time_ns: journal::now(),
            pid: n.pid,
            op: "not protected".into(),
            path: p.map(|p| journal::display(&p.path)).unwrap_or_default(),
            path2: String::new(),
            quarantine: String::new(),
            flags: 0,
            note: Detail {
                message: err.to_string(),
                ..Detail::default()
            },
        };
        if let Err(e) = self.journal.append(e) {
            self.journal.error(&e);
        }
    }
    fn capture(
        &mut self,
        n: &sys::Notification,
        c: &Call,
        p: &Resolved,
        p2: Option<&Resolved>,
    ) -> io::Result<()> {
        if p.skipped()? || p2.map(Resolved::skipped).transpose()?.unwrap_or(false) {
            return Ok(());
        }
        let mut effects = Vec::new();
        match c.kind {
            Kind::Open => {
                if let Some(st) = p.stat.as_ref() {
                    if c.flags & libc::O_EXCL as u64 != 0 && c.flags & libc::O_CREAT as u64 != 0 {
                        return Ok(());
                    }
                    if c.flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_TRUNC) as u64 != 0
                        && st.st_mode & libc::S_IFMT == libc::S_IFREG
                    {
                        let mut e = self.effect(n, "modify", p, None, c.flags);
                        self.keep_copy(p, &mut e, true)?;
                        effects.push(e);
                    }
                } else if c.flags & libc::O_CREAT as u64 != 0 {
                    effects.push(self.effect(n, "create", p, None, c.flags));
                }
            }
            Kind::Truncate => {
                if let Some(st) = p
                    .stat
                    .as_ref()
                    .filter(|s| s.st_mode & libc::S_IFMT == libc::S_IFREG)
                {
                    let mut e = self.effect(
                        n,
                        if c.size >= st.st_size as u64 {
                            "size"
                        } else {
                            "modify"
                        },
                        p,
                        None,
                        c.flags,
                    );
                    if c.size < st.st_size as u64 {
                        self.keep_copy(p, &mut e, true)?;
                    }
                    effects.push(e);
                }
            }
            Kind::Delete => {
                if p.stat.is_some() {
                    let mut e = self.effect(n, "delete", p, None, c.flags);
                    self.keep_link(p, &mut e)?;
                    effects.push(e);
                }
            }
            Kind::Rmdir => {
                if p.state().is_some_and(|s| s.directory()) {
                    effects.push(self.effect(n, "rmdir", p, None, c.flags));
                }
            }
            Kind::Create => {
                if p.stat.is_none() {
                    effects.push(self.effect(n, "create", p, None, c.flags));
                }
            }
            Kind::Link => {
                if let Some(dest) = p2.filter(|d| d.stat.is_none()) {
                    effects.push(self.effect(n, "create", dest, None, c.flags));
                }
            }
            Kind::Rename | Kind::Exchange => {
                if let Some(dest) = p2.filter(|_| p.stat.is_some()) {
                    if c.kind == Kind::Rename
                        && c.flags & libc::RENAME_NOREPLACE as u64 != 0
                        && dest.stat.is_some()
                    {
                        return Ok(());
                    }
                    if p.state()
                        .zip(dest.state())
                        .is_some_and(|(a, b)| a.same_inode(&b))
                    {
                        return Ok(());
                    }
                    if c.kind == Kind::Rename
                        && let Some(st) = &dest.stat
                    {
                        let mut e = self.effect(
                            n,
                            if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                                "rmdir"
                            } else {
                                "delete"
                            },
                            dest,
                            None,
                            c.flags,
                        );
                        if !e.note.before.as_ref().is_some_and(State::directory) {
                            self.keep_link(dest, &mut e)?;
                        }
                        effects.push(e);
                    }
                    effects.push(self.effect(
                        n,
                        if c.kind == Kind::Exchange {
                            "exchange"
                        } else {
                            "rename"
                        },
                        p,
                        Some(dest),
                        c.flags,
                    ));
                }
            }
        }
        let pending_kind = if effects.iter().any(|e| e.op == "create") {
            Kind::Create
        } else {
            c.kind
        };
        let mut seqs = Vec::new();
        for e in effects {
            seqs.push(self.journal.append(e)?);
        }
        if !seqs.is_empty() {
            self.pending.insert(
                n.pid,
                Pending {
                    seqs,
                    kind: pending_kind,
                    path: if c.kind == Kind::Link {
                        p2.map(|p| p.path.clone()).unwrap_or_else(|| p.path.clone())
                    } else {
                        p.path.clone()
                    },
                    path2: if c.kind == Kind::Link {
                        None
                    } else {
                        p2.map(|p| p.path.clone())
                    },
                    before: if c.kind == Kind::Link {
                        None
                    } else {
                        p.state()
                    },
                    before2: p2.and_then(Resolved::state),
                },
            );
        }
        Ok(())
    }
    fn keep_link(&mut self, p: &Resolved, e: &mut Effect) -> io::Result<()> {
        if self.created.contains(&p.path) {
            e.note.kept = "created earlier".into();
            return Ok(());
        }
        let q = format!("{}-{}", self.journal.next_seq(), journal::now());
        match sys::link_at(
            p.parent.as_raw_fd(),
            &p.name,
            self.qfd.as_raw_fd(),
            &CString::new(q.as_bytes()).map_err(io::Error::other)?,
        ) {
            Ok(()) => {
                e.quarantine = q.clone();
                e.note.kept = "hard link".into();
                if let Some(st) = p.state() {
                    self.linked.insert(q, st);
                }
                Ok(())
            }
            Err(_) => self.keep_copy(p, e, false),
        }
    }
    fn copy_to(&self, p: &Resolved, q: &str) -> io::Result<()> {
        let st = p
            .stat
            .as_ref()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io::Error::other("only regular files can be copied"));
        }
        if st.st_size as u64 > self.max_copy {
            return Err(io::Error::other("too large"));
        }
        let src = sys::open_at(
            p.parent.as_raw_fd(),
            &p.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        let dst = sys::open_at(
            self.qfd.as_raw_fd(),
            &CString::new(q).map_err(io::Error::other)?,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )?;
        sys::copy_regular(File::from(src), File::from(dst), st)
    }
    fn keep_copy(&mut self, p: &Resolved, e: &mut Effect, deduplicate: bool) -> io::Result<()> {
        let st = p
            .state()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        // A delete's hard link still shares the live inode. Freeze those links before a write.
        let links: Vec<_> = self
            .linked
            .iter()
            .filter(|(_, s)| s.same_inode(&st))
            .map(|(q, _)| q.clone())
            .collect();
        for q in links {
            let temp = format!("freeze-{}-{}", q, journal::now());
            self.copy_to(p, &temp)?;
            sys::rename_at(
                self.qfd.as_raw_fd(),
                &CString::new(temp).map_err(io::Error::other)?,
                self.qfd.as_raw_fd(),
                &CString::new(q.as_bytes()).map_err(io::Error::other)?,
                0,
            )?;
            self.linked.remove(&q);
        }
        let key = (st.dev, st.ino);
        if self.created.contains(&p.path) {
            e.note.kept = "created earlier".into();
            return Ok(());
        }
        if deduplicate && let Some((q, original)) = self.copied.get(&key) {
            e.quarantine = q.clone();
            e.note.before = Some(original.clone());
            e.note.kept = "copy (same inode)".into();
            return Ok(());
        }
        let q = format!("{}-{}", self.journal.next_seq(), journal::now());
        match self.copy_to(p, &q) {
            Ok(()) => {
                e.quarantine = q.clone();
                e.note.kept = "copy".into();
                if deduplicate {
                    self.copied.insert(key, (q, st));
                }
                Ok(())
            }
            Err(err) => {
                e.op = "not protected".into();
                e.note.message = err.to_string();
                Ok(())
            }
        }
    }
    fn applied(p: &Pending) -> bool {
        let current = journal::state(&p.path).ok().flatten();
        let second = p
            .path2
            .as_ref()
            .and_then(|p| journal::state(p).ok().flatten());
        match p.kind {
            Kind::Delete | Kind::Rmdir => {
                current.is_none()
                    || current
                        .as_ref()
                        .zip(p.before.as_ref())
                        .is_some_and(|(a, b)| !a.same_inode(b))
            }
            Kind::Create => current.is_some(),
            Kind::Rename => second
                .as_ref()
                .zip(p.before.as_ref())
                .is_some_and(|(a, b)| a.same_inode(b)),
            Kind::Exchange => {
                second
                    .as_ref()
                    .zip(p.before.as_ref())
                    .is_some_and(|(a, b)| a.same_inode(b))
                    && current
                        .as_ref()
                        .zip(p.before2.as_ref())
                        .is_some_and(|(a, b)| a.same_inode(b))
            }
            _ => true,
        }
    }
    pub fn complete(&mut self, pid: u32) {
        let Some(p) = self.pending.remove(&pid) else {
            return;
        };
        let applied = Self::applied(&p);
        if applied {
            if p.kind == Kind::Create {
                self.created.insert(p.path.clone());
            }
            if matches!(p.kind, Kind::Delete | Kind::Rmdir) {
                self.created.retain(|old| !old.starts_with(&p.path));
            }
            if matches!(p.kind, Kind::Rename | Kind::Exchange)
                && let Some(dest) = &p.path2
            {
                let previous = std::mem::take(&mut self.created);
                for old in previous {
                    if let Ok(tail) = old.strip_prefix(&p.path) {
                        self.created.insert(dest.join(tail));
                    } else if let Ok(tail) = old.strip_prefix(dest) {
                        if p.kind == Kind::Exchange {
                            self.created.insert(p.path.join(tail));
                        }
                    } else {
                        self.created.insert(old);
                    }
                }
            }
        }
        for seq in p.seqs {
            if let Err(e) = self.journal.outcome(seq, applied) {
                self.journal.error(&e);
            }
        }
    }
    pub fn snapshot(&mut self) {
        if let Err(e) = self.journal.snapshot() {
            self.journal.error(&e);
        }
    }
    pub fn finish(&mut self, status: i32) {
        let pids: Vec<_> = self.pending.keys().copied().collect();
        for pid in pids {
            self.complete(pid);
        }
        if let Err(e) = self.journal.finish(status) {
            self.journal.error(&e);
        }
    }
}
