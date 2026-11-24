//! Decode a stopped syscall, pin its parent directories, and journal its pre-images.
mod context;
use crate::{
    classify::{self, GitDeletion, Indexes},
    journal::{self, Detail, Effect, Journal, State},
    rules::{Breaker, Policy},
    sys,
};
use context::{NotificationResolver, ResolutionRoot, TaskCache, resolve};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    rc::Rc,
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
fn relocated(base: &Path, tail: &Path) -> PathBuf {
    if tail.as_os_str().is_empty() {
        base.to_owned()
    } else {
        base.join(tail)
    }
}
pub struct Resolved {
    pub parent: Rc<OwnedFd>,
    pub name: CString,
    pub path: PathBuf,
    pub stat: Option<libc::stat>,
    requests: Vec<PathBuf>,
    parent_dev: u64,
    mount: Option<u64>,
    birth: Option<(i64, u32)>,
    incarnation: std::cell::Cell<Option<sys::Incarnation>>,
}
impl Resolved {
    fn pinned_state(&self) -> Option<State> {
        self.stat.as_ref().map(|stat| {
            let mut state = State::from_stat(stat);
            state.incarnation.birth = self.birth;
            state
        })
    }
    pub fn state(&self) -> Option<State> {
        self.stat.as_ref().map(|s| {
            let mut state = State::from_stat(s);
            let marker = self.incarnation.get().unwrap_or_else(|| {
                sys::Incarnation::captured(self.parent.as_raw_fd(), &self.name, s, self.birth)
            });
            self.incarnation.set(Some(marker));
            state.incarnation = marker;
            state
        })
    }
    fn proc_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.parent.as_raw_fd()))
            .join(OsStr::from_bytes(self.name.to_bytes()))
    }
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
struct Entry {
    parent: Option<Rc<OwnedFd>>,
    name: CString,
}
impl Entry {
    fn pin(p: &Resolved) -> Self {
        Self {
            parent: Some(Rc::clone(&p.parent)),
            name: p.name.clone(),
        }
    }
    fn state(&self, path: &Path) -> Option<State> {
        let Some(parent) = &self.parent else {
            return journal::capture_state(path).ok().flatten();
        };
        journal::capture_state_at(parent.as_raw_fd(), &self.name)
            .ok()
            .flatten()
    }
    fn exists(&self, path: &Path) -> bool {
        match &self.parent {
            Some(parent) => sys::stat_at(parent.as_raw_fd(), &self.name)
                .ok()
                .flatten()
                .is_some(),
            None => fs::symlink_metadata(path).is_ok(),
        }
    }
}
enum Destruction {
    Observed(PathBuf, State),
    CheckGit(GitDeletion),
}

struct Pending {
    inode_pinned: bool,
    at: Entry,
    at2: Option<Entry>,
    seqs: Vec<u64>,
    kind: Kind,
    path: PathBuf,
    path2: Option<PathBuf>,
    before: Option<State>,
    before2: Option<State>,
    destruction: Vec<Destruction>,
    git_lock: Option<u64>,
}
pub struct Capture {
    pub journal: Journal,
    qfd: OwnedFd,
    deny_roots: Vec<PathBuf>,
    pub policy: Option<Policy>,
    pub prior_created: std::collections::HashSet<crate::classify::Birth>,
    pub(crate) breaker: Breaker,
    pub indexes: Indexes,
    pending_git_deletions: Vec<GitDeletion>,
    checked_git_batch: bool,
    git_locks: BTreeMap<PathBuf, (u64, State)>,
    last_rule_candidate: u64,
    cache_roots: BTreeSet<PathBuf>,
    max_copy: u64,
    vm_unavailable: bool,
    tasks: TaskCache,
    page: usize,
    copied: HashMap<(u64, u64), (String, State)>,
    original_links: HashMap<(u64, u64), PathBuf>,
    created: BTreeSet<OsString>,
    filesystems: HashMap<u64, (bool, OwnedFd)>,
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
        let max_copy = match std::env::var("SAQTA_MAX_COPY") {
            Ok(s) => s.parse().map_err(io::Error::other)?,
            Err(_) => DEFAULT_MAX_COPY,
        };
        Ok(Self {
            journal,
            qfd,
            deny_roots: std::iter::once(home)
                .chain(crate::paths::protected_configs())
                .collect(),
            policy: None,
            prior_created: Default::default(),
            breaker: Breaker::default(),
            indexes: Indexes::default(),
            pending_git_deletions: Vec::new(),
            checked_git_batch: false,
            git_locks: BTreeMap::new(),
            last_rule_candidate: 0,
            cache_roots: BTreeSet::new(),
            max_copy,
            vm_unavailable: std::env::var_os("SAQTA_FORCE_PROC_MEM").is_some_and(|s| s == "1"),
            page: sys::page_size()?,
            tasks: TaskCache {
                eligible: None,
                task: None,
            },
            copied: HashMap::new(),
            original_links: HashMap::new(),
            created: BTreeSet::new(),
            filesystems: HashMap::new(),
            pending: HashMap::new(),
            linked: BTreeMap::new(),
        })
    }
    pub fn needs_rule_evaluation(&self, after: u64) -> bool {
        self.last_rule_candidate > after || self.tripped()
    }

    pub fn tripped(&self) -> bool {
        self.policy.as_ref().is_some_and(|p| self.breaker.fired(p))
    }

    /// Capture exactly one patch-named entry. Patches replace entries, so retain copies.
    pub fn patch_path(&mut self, path: &Path, update: bool) -> io::Result<()> {
        let mut ancestor = path.parent().unwrap_or(Path::new("/"));
        let mut tail = path.file_name().map(PathBuf::from).unwrap_or_default();
        let normalized = loop {
            match fs::canonicalize(ancestor) {
                Ok(base) => break crate::hook::normalize(&base.join(&tail)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if let Some(name) = ancestor.file_name() {
                        tail = PathBuf::from(name).join(tail);
                    }
                    if let Some(parent) = ancestor.parent() {
                        ancestor = parent;
                    } else {
                        break crate::hook::normalize(path);
                    }
                }
                Err(_) => break crate::hook::normalize(path),
            }
        };
        let resolved = resolve(sys::pid() as u32, libc::AT_FDCWD, path.as_os_str(), false);
        let denied = self.denied_path(&normalized, Kind::Delete, false)
            || resolved
                .as_ref()
                .is_ok_and(|p| self.denied(p, Kind::Delete));
        let mut e = Effect {
            seq: 0,
            time_ns: journal::now(),
            pid: sys::pid() as u32,
            op: "create".into(),
            path: journal::display(&normalized),
            path2: String::new(),
            quarantine: String::new(),
            flags: 0,
            note: Detail::default(),
        };
        if denied {
            e.op = "not protected".into();
            e.note.message = "deny floor: patch path".into();
        } else {
            match resolved {
                Ok(p) => {
                    e.path = journal::display(&p.path);
                    e.note.before = p.state();
                    if p.stat.is_some() {
                        e.op = if update { "modify" } else { "delete" }.into();
                        if let Err(err) = self.keep_copy(&p, &mut e, false) {
                            e.op = "not protected".into();
                            e.note.message = err.to_string();
                        }
                    }
                }
                Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {}
                Err(err) => {
                    e.op = "not protected".into();
                    e.note.message = err.to_string();
                }
            }
        }
        if e.op == "not protected" {
            crate::hook::log(&e.note.message);
        }
        self.journal.append(e)?;
        Ok(())
    }
    pub fn observe_pending(&mut self) {
        let observed: Vec<_> = self
            .pending
            .iter()
            .filter(|(pid, p)| {
                !sys::alive(**pid as i32)
                    || if matches!(p.kind, Kind::Open | Kind::Truncate) {
                        self.policy.is_none()
                            || (!p.destruction.is_empty() && p.at.state(&p.path) != p.before)
                    } else {
                        Self::applied(p) == Some(true)
                    }
            })
            .map(|(pid, _)| *pid)
            .collect();
        for pid in observed {
            self.complete(pid);
        }
    }

    pub fn handle(&mut self, listener: i32, n: &sys::Notification) -> bool {
        self.complete(n.pid);
        self.observe_pending();
        if self.tripped() {
            // Leave this notification stopped until the supervisor freezes and kills the tree.
            return false;
        }
        if crate::filter::CONTEXT_CALLS.contains(&(n.data.nr as i64)) {
            // Other tids cannot repopulate a cache while this context call is
            // in flight. Only its issuer's next notification permits reuse.
            self.tasks.invalidate(n.pid);
            if matches!(
                n.data.nr as i64,
                libc::SYS_mount
                    | libc::SYS_umount2
                    | libc::SYS_move_mount
                    | libc::SYS_pivot_root
                    | libc::SYS_setns
                    | libc::SYS_unshare
            ) {
                self.filesystems.clear();
                // Release observation pins too. Mount changes use M4's pathname
                // observation rather than making an otherwise idle mount busy.
                for pending in self.pending.values_mut() {
                    pending.at.parent = None;
                    if let Some(second) = pending.at2.as_mut() {
                        second.parent = None;
                    }
                }
            }
            return sys::answer(listener, n.id, false);
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
                    return sys::answer(listener, n.id, true);
                }
                if let Err(e) = self.capture(n, &call, &p, p2.as_ref()) {
                    self.failure(n, Some(&p), &e);
                }
                sys::answer(listener, n.id, false)
            }
            Ok(None) => false, // ID is stale. Never act on its arguments.
            Err(e) => {
                if sys::valid(listener, n.id) {
                    self.failure(n, None, &e);
                    return sys::answer(listener, n.id, false);
                }
                false
            }
        }
    }
    fn read_call(
        &mut self,
        listener: i32,
        n: &sys::Notification,
    ) -> io::Result<Option<(Call, Resolved, Option<Resolved>)>> {
        let call = Call::decode(n)?;
        let mut task = self.tasks.take(n.pid)?;
        let mut reader = sys::PathReader::new(
            n.pid as i32,
            self.page,
            &mut self.vm_unavailable,
            &mut task.mem,
        );
        let p = reader.read(call.address)?;
        let p2 = call.address2.map(|a| reader.read(a)).transpose()?;
        if !sys::valid(listener, n.id) {
            return Ok(None);
        }
        let persistent = self.tasks.eligible == Some(n.pid);
        if persistent && task.root.is_none() {
            task.root = Some(ResolutionRoot::new(n.pid)?);
        }
        let root_display = if let Some(root) = task.root.as_mut() {
            if root.path != Path::new("/") {
                root.path = fs::read_link(format!("/proc/self/fd/{}", root.fd.as_raw_fd()))?;
            }
            Some(root.path.clone())
        } else {
            None
        };
        if let Some(base) = task.cwd.as_mut() {
            base.path = fs::read_link(format!("/proc/self/fd/{}", base.fd.as_raw_fd()))?;
            if base.path.as_os_str().as_bytes().ends_with(b" (deleted)") {
                base.stat = sys::stat_fd(base.fd.as_raw_fd())?;
            }
        }
        let mut resolver = NotificationResolver {
            pid: n.pid,
            root: task.root.take(),
            root_display,
            base: task.cwd.take(),
        };
        let p = resolver.resolve(call.dir, &p, call.follow)?;
        let p2 = p2
            .map(|p| resolver.resolve(call.dir2, &p, false))
            .transpose()?;
        task.root = resolver.root;
        task.cwd = resolver.base.filter(|base| base.dir == libc::AT_FDCWD);
        self.tasks.put(task);
        if !sys::valid(listener, n.id) {
            return Ok(None);
        }
        Ok(Some((call, p, p2)))
    }
    fn denied(&self, p: &Resolved, kind: Kind) -> bool {
        std::iter::once(&p.path)
            .chain(p.requests.iter())
            .any(|path| {
                self.denied_path(
                    path,
                    kind,
                    p.stat
                        .as_ref()
                        .is_some_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFDIR),
                )
            })
    }
    fn denied_path(&self, path: &Path, kind: Kind, directory: bool) -> bool {
        if self.deny_roots.iter().any(|root| {
            path.starts_with(root)
                || (matches!(kind, Kind::Rmdir | Kind::Rename | Kind::Exchange)
                    && directory
                    && root.starts_with(path))
        }) {
            return true;
        }
        let name = path.file_name().and_then(OsStr::to_str);
        if matches!(name, Some("settings.json" | "settings.local.json"))
            && path.parent().and_then(Path::file_name) == Some(OsStr::new(".claude"))
        {
            return true;
        }
        matches!(name, Some("config.toml" | "hooks.json"))
            && path.parent().and_then(Path::file_name) == Some(OsStr::new(".codex"))
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
                before: if op == "delete" {
                    p.pinned_state()
                } else {
                    p.state()
                },
                before2: p2.and_then(|p| {
                    if op == "rename" {
                        p.pinned_state()
                    } else {
                        p.state()
                    }
                }),
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
    fn skipped(&mut self, p: &Resolved) -> io::Result<bool> {
        let dev = p.stat.as_ref().map_or(p.parent_dev, |st| st.st_dev);
        if let Some(mount) = p.mount
            && let Some((skip, _pin)) = self.filesystems.get(&mount)
        {
            return Ok(*skip);
        }
        let fd = if p.stat.is_some() {
            sys::open_at(
                p.parent.as_raw_fd(),
                &p.name,
                libc::O_PATH | libc::O_NOFOLLOW,
                0,
            )?
        } else {
            p.parent.try_clone()?
        };
        let (actual, mount) = sys::stat_mount_fd(fd.as_raw_fd())?;
        if actual.st_dev != dev || p.mount.is_some() && mount != p.mount {
            return Err(io::Error::other("filesystem changed during classification"));
        }
        let skip = SKIPPED_FILESYSTEMS.contains(&sys::fs_type(fd.as_raw_fd())?);
        // Pin the mount, not st_dev: stacked filesystems can report a backing device.
        if let Some(mount) = mount {
            self.filesystems.insert(mount, (skip, fd));
        }
        Ok(skip)
    }
    fn capture(
        &mut self,
        n: &sys::Notification,
        c: &Call,
        p: &Resolved,
        p2: Option<&Resolved>,
    ) -> io::Result<()> {
        if self.skipped(p)? || p2.map(|p| self.skipped(p)).transpose()?.unwrap_or(false) {
            return Ok(());
        }
        if c.kind == Kind::Link
            && !self.created.contains(p.path.as_os_str())
            && let Some(st) = p.state()
        {
            self.original_links.insert((st.dev, st.ino), p.path.clone());
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
                    let key = (st.st_dev, st.st_ino);
                    let created = self.created.contains(p.path.as_os_str());
                    let original = self.original_links.get(&key);
                    let new_name_without_origin = created && original.is_none();
                    // Keep a pre-image through the new name if its recorded origin
                    // disappeared and growth can still reach another link.
                    let copy = c.size < st.st_size as u64
                        || self.copied.contains_key(&key)
                        || (new_name_without_origin && st.st_nlink > 1);
                    let mut e =
                        self.effect(n, if copy { "modify" } else { "size" }, p, None, c.flags);
                    if copy {
                        self.keep_copy(p, &mut e, true)?;
                    } else {
                        if created && let Some(original) = original {
                            e.path = journal::display(original);
                        }
                        self.freeze_links(p)?;
                        if new_name_without_origin {
                            return Ok(());
                        }
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
                    if p.stat
                        .as_ref()
                        .zip(dest.stat.as_ref())
                        .is_some_and(|(a, b)| a.st_dev == b.st_dev && a.st_ino == b.st_ino)
                        && p.state()
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
        let mut destruction = Vec::new();
        let mut last_candidate = None;
        let mut git_lock = None;
        if let Some(policy) = &self.policy {
            for (index, e) in effects.iter_mut().enumerate() {
                if e.op == "create"
                    && e.path.ends_with(".lock")
                    && classify::git_database(&journal::path(&e.path)?)
                    && classify::git_process(n.pid)
                {
                    git_lock = Some(index);
                }
                if !matches!(e.op.as_str(), "delete" | "modify" | "not protected") {
                    continue;
                }
                let path = journal::path(&e.path)?;
                if path.file_name().is_some_and(|n| n == "CACHEDIR.TAG")
                    && let Some(parent) = path.parent().filter(|p| crate::gc::cache_marker(p))
                {
                    self.cache_roots.insert(parent.to_owned());
                }
                e.note.regenerable = self.cache_roots.iter().any(|p| path.starts_with(p))
                    || policy.regenerable(&path);
                if e.note.regenerable {
                    continue;
                }
                e.note.agent_created = self.created.contains(path.as_os_str())
                    && !e
                        .note
                        .before
                        .as_ref()
                        .is_some_and(|st| self.original_links.contains_key(&(st.dev, st.ino)))
                    || (!self.prior_created.is_empty()
                        && crate::classify::Birth::read(&path)
                            .is_some_and(|id| self.prior_created.contains(&id)));
                let database = classify::git_database(&path);
                if database {
                    e.note.git = classify::git_process(n.pid);
                }
                let Some(before) = e.note.before.as_ref().filter(|s| !s.directory()) else {
                    continue;
                };
                if matches!(e.op.as_str(), "delete" | "modify") && !e.quarantine.is_empty() {
                    last_candidate = Some(index);
                }
                // Snapshot the old index before Git can replace it with the result
                // of the same switch, reset, or merge.
                let git = if !database || path.ends_with(".git/index") {
                    self.indexes
                        .observe(&path, &self.journal.folder, policy)
                        .map(|(root, entry)| {
                            (
                                root,
                                entry.is_some(),
                                entry.is_some_and(|e| e.stat_matches(before)),
                            )
                        })
                } else {
                    None
                };
                if e.note.kept == "created earlier" || (database && e.note.git) {
                    continue;
                }
                let deleted = c.kind == Kind::Delete;
                let replaces_precious_or_outside = (policy.precious(&path)
                    || !path.starts_with(&policy.project))
                    && (e.op == "delete"
                        || c.kind == Kind::Truncate
                        || c.flags & libc::O_TRUNC as u64 != 0
                        || c.flags & libc::O_APPEND as u64 == 0);
                if !deleted && !replaces_precious_or_outside {
                    continue;
                }
                match git {
                    Some((_, _, true)) => {}
                    Some((root, tracked, false))
                        if tracked || (deleted && classify::git_process(n.pid)) =>
                    {
                        if !e.quarantine.is_empty()
                            && matches!(before.mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK)
                        {
                            destruction.push(Destruction::CheckGit(GitDeletion {
                                root,
                                path,
                                before: before.clone(),
                                retained: self.journal.quarantine.join(&e.quarantine),
                                tracked,
                            }));
                        }
                    }
                    _ if database || self.indexes.is_known(&path) => {
                        destruction.push(Destruction::Observed(path, before.clone()));
                    }
                    _ => {}
                }
            }
        }
        let pending_kind = if effects.iter().any(|e| e.op == "create") {
            Kind::Create
        } else {
            c.kind
        };
        let inode_pinned = c.kind == Kind::Delete && effects.iter().any(|e| e.note.pinned);
        let seqs = self.journal.append_batch(effects)?;
        if let Some(seq) = last_candidate.and_then(|i| seqs.get(i)) {
            self.last_rule_candidate = *seq;
        }
        let git_lock = git_lock.and_then(|index| seqs.get(index).copied());
        if !seqs.is_empty() {
            self.pending.insert(
                n.pid,
                Pending {
                    inode_pinned,
                    at: Entry::pin(if c.kind == Kind::Link {
                        p2.unwrap_or(p)
                    } else {
                        p
                    }),
                    at2: if c.kind == Kind::Link {
                        None
                    } else {
                        p2.map(Entry::pin)
                    },
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
                        if inode_pinned {
                            p.pinned_state()
                        } else {
                            p.state()
                        }
                    },
                    before2: p2.and_then(|p| {
                        if c.kind == Kind::Rename {
                            p.pinned_state()
                        } else {
                            p.state()
                        }
                    }),
                    destruction,
                    git_lock,
                },
            );
        }
        Ok(())
    }
    fn keep_link(&mut self, p: &Resolved, e: &mut Effect) -> io::Result<()> {
        if self.created.contains(p.path.as_os_str())
            && p.stat.as_ref().is_none_or(|st| st.st_nlink <= 1)
        {
            e.note.kept = "created earlier".into();
            e.note.before = p.state();
            return Ok(());
        }
        // A new alias can be the last recorded name of a pre-existing shared
        // inode. Undo must be able to repair through it before undoing creation.
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
                e.note.pinned = true;
                if let Some(st) = p.pinned_state() {
                    self.linked.insert(q, st);
                }
                Ok(())
            }
            Err(err) => {
                self.keep_copy(p, e, false)?;
                if err.raw_os_error() == Some(libc::EXDEV) {
                    e.note.kept = "copied (cross-mount)".into();
                }
                Ok(())
            }
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
        if let Some(mut marker) = p.state().map(|state| state.incarnation) {
            if let Some(generation) = sys::generation_fd(src.as_raw_fd()) {
                marker.generation = Some(generation);
                marker.unverified = false;
            }
            p.incarnation.set(Some(marker));
        }
        let dst = sys::open_at(
            self.qfd.as_raw_fd(),
            &CString::new(q).map_err(io::Error::other)?,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )?;
        sys::copy_regular(File::from(src), File::from(dst), st)
    }
    fn freeze_links(&mut self, p: &Resolved) -> io::Result<()> {
        let st = p
            .state()
            .ok_or_else(|| io::Error::other("missing freeze source"))?;
        // A delete's hard link still shares the live inode. Freeze those links before a write.
        let links: Vec<_> = self
            .linked
            .iter()
            // These quarantine hard links still pin the inode, so its number
            // cannot have been reused. No birth-time-only discovery occurs here.
            .filter(|(_, s)| s.dev == st.dev && s.ino == st.ino)
            .map(|(q, _)| q.clone())
            .collect();
        for q in links {
            let temp = format!("freeze-{}-{}", q, journal::now());
            self.copy_to(p, &temp)?;
            if let Some(mut original) = self.linked.get(&q).cloned() {
                if let Some(state) = p.state() {
                    original.incarnation.generation = state.incarnation.generation;
                    original.incarnation.unverified = state.incarnation.unverified;
                    if original.incarnation.birth.is_none() {
                        original.incarnation.birth = state.incarnation.birth;
                    }
                }
                self.journal.record_pin(&q, original)?;
            }
            sys::rename_at(
                self.qfd.as_raw_fd(),
                &CString::new(temp).map_err(io::Error::other)?,
                self.qfd.as_raw_fd(),
                &CString::new(q.as_bytes()).map_err(io::Error::other)?,
                0,
            )?;
            self.linked.remove(&q);
        }
        Ok(())
    }
    fn keep_copy(&mut self, p: &Resolved, e: &mut Effect, deduplicate: bool) -> io::Result<()> {
        let st = p
            .state()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        self.freeze_links(p)?;
        let key = (st.dev, st.ino);
        if self.created.contains(p.path.as_os_str()) {
            let original = self.original_links.get(&key).filter(|path| {
                journal::capture_state(path)
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.same_inode(&st))
            });
            if let Some(original) = original {
                e.path = journal::display(original);
            } else if p.stat.as_ref().is_none_or(|stat| stat.st_nlink <= 1) {
                e.note.kept = "created earlier".into();
                return Ok(());
            }
            // A new name can still share a pre-command inode after its recorded
            // origin was removed. Retain and repair it before dropping that name.
        }
        if deduplicate
            && let Some((q, original)) = self.copied.get(&key)
            && original.same_inode(&st)
        {
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
                e.note.before = p.state();
                if deduplicate && let Some(state) = p.state() {
                    self.copied.insert(key, (q, state));
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
    fn outcome_identity(actual: Option<&State>, before: Option<&State>) -> Option<bool> {
        let Some((actual, before)) = actual.zip(before) else {
            return Some(false);
        };
        if actual.dev == before.dev
            && actual.ino == before.ino
            && (!actual.incarnation.available() || !before.incarnation.available())
        {
            // Incomplete identity proves neither success nor failure. Leave the
            // outcome unknown, so undo cannot silently skip an applied rename.
            return None;
        }
        Some(actual.same_inode(before))
    }
    fn applied(p: &Pending) -> Option<bool> {
        if matches!(p.kind, Kind::Open | Kind::Truncate) {
            return Some(true);
        }
        let second = || {
            p.at2
                .as_ref()
                .zip(p.path2.as_ref())
                .and_then(|(at, path)| at.state(path))
        };
        if p.kind == Kind::Rename {
            return Self::outcome_identity(second().as_ref(), p.before.as_ref());
        }
        if p.kind == Kind::Create {
            // Creation outcome only needs existence. The final snapshot records
            // full identity; do not read generation for this discarded check.
            return Some(p.at.exists(&p.path));
        }
        let current = p.at.state(&p.path);
        match p.kind {
            Kind::Delete | Kind::Rmdir => {
                if current.is_none() {
                    Some(true)
                } else if p.inode_pinned {
                    // The retained link prevents reuse while the syscall
                    // completes, including a failed unlink.
                    Some(
                        current
                            .as_ref()
                            .zip(p.before.as_ref())
                            .is_some_and(|(a, b)| a.dev != b.dev || a.ino != b.ino),
                    )
                } else {
                    Self::outcome_identity(current.as_ref(), p.before.as_ref()).map(|same| !same)
                }
            }
            Kind::Exchange => {
                let a = Self::outcome_identity(second().as_ref(), p.before.as_ref());
                let b = Self::outcome_identity(current.as_ref(), p.before2.as_ref());
                match (a, b) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                }
            }
            _ => Some(true),
        }
    }
    pub fn complete(&mut self, pid: u32) {
        let Some(p) = self.pending.remove(&pid) else {
            return;
        };
        let outcome = Self::applied(&p);
        let applied = outcome.unwrap_or(true);
        if applied && let Some(policy) = &self.policy {
            let changed_in_place =
                !matches!(p.kind, Kind::Open | Kind::Truncate) || p.at.state(&p.path) != p.before;
            if changed_in_place {
                for loss in p.destruction {
                    match loss {
                        Destruction::Observed(path, before) => {
                            self.breaker.observe(policy, &path, &before, false);
                        }
                        Destruction::CheckGit(candidate) => {
                            self.pending_git_deletions.push(candidate);
                        }
                    }
                }
                self.check_git_deletions(false);
            }
        }
        if applied {
            if p.kind == Kind::Create {
                self.created.insert(p.path.clone().into_os_string());
                if let Some(seq) = p.git_lock
                    && let Some(state) =
                        p.at.state(&p.path)
                            .filter(|s| s.mode & libc::S_IFMT == libc::S_IFREG)
                {
                    self.git_locks.insert(p.path.clone(), (seq, state));
                }
            }
            if matches!(p.kind, Kind::Delete | Kind::Rename | Kind::Exchange) {
                self.git_locks.remove(&p.path);
            }
            if matches!(p.kind, Kind::Delete | Kind::Rmdir) {
                self.created.remove(p.path.as_os_str());
                let mut prefix = p.path.clone().into_os_string();
                prefix.push("/");
                let removed: Vec<_> = self
                    .created
                    .range(prefix.clone()..)
                    .take_while(|old| old.as_bytes().starts_with(prefix.as_bytes()))
                    .cloned()
                    .collect();
                for path in removed {
                    self.created.remove(&path);
                }
                self.original_links
                    .retain(|_, old| !old.starts_with(&p.path));
            }
            if matches!(p.kind, Kind::Rename | Kind::Exchange)
                && let Some(dest) = &p.path2
            {
                self.original_links.retain(|_, old| {
                    if let Ok(tail) = old.strip_prefix(&p.path) {
                        *old = relocated(dest, tail);
                        true
                    } else if let Ok(tail) = old.strip_prefix(dest) {
                        if p.kind == Kind::Exchange {
                            *old = relocated(&p.path, tail);
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                });
                let previous = std::mem::take(&mut self.created);
                for old in previous {
                    let old = PathBuf::from(old);
                    if let Ok(tail) = old.strip_prefix(&p.path) {
                        self.created.insert(relocated(dest, tail).into_os_string());
                    } else if let Ok(tail) = old.strip_prefix(dest) {
                        if p.kind == Kind::Exchange {
                            self.created
                                .insert(relocated(&p.path, tail).into_os_string());
                        }
                    } else {
                        self.created.insert(old.into_os_string());
                    }
                }
            }
        }
        for seq in p.seqs {
            if let Some(applied) = outcome
                && let Err(e) = self.journal.outcome(seq, applied)
            {
                self.journal.error(&e);
            }
        }
    }
    pub fn interrupted_git_locks(&self) -> anyhow::Result<BTreeSet<u64>> {
        if self.git_locks.is_empty() {
            return Ok(BTreeSet::new());
        }
        let posts = journal::post_states(&self.journal.folder)?;
        Ok(self
            .git_locks
            .iter()
            .filter_map(|(path, (seq, inode))| {
                posts
                    .get(&journal::display(path))
                    .and_then(Option::as_ref)
                    .is_some_and(|current| current.same_inode(inode))
                    .then_some(*seq)
            })
            .collect())
    }

    pub fn check_git_deletions(&mut self, flush: bool) {
        let Some(policy) = &self.policy else { return };
        let pending = &self.pending_git_deletions;
        if pending.is_empty() {
            return;
        }
        // The first potentially decisive batch runs immediately. If its
        // stat misses included clean files, collect another bounded batch
        // before retrying so a near-threshold command cannot spawn Git per file.
        if !flush && self.checked_git_batch && pending.len() < 32 {
            return;
        }
        let mut bytes = 0u64;
        let mut irreplaceable = 0;
        for c in pending {
            bytes = bytes.saturating_add(c.before.size.max(0) as u64);
            irreplaceable += usize::from(policy.irreplaceable(&c.path));
        }
        if !self
            .breaker
            .might_fire(policy, pending.len(), bytes, irreplaceable)
        {
            return;
        }
        self.checked_git_batch = true;
        let candidates = std::mem::take(&mut self.pending_git_deletions);
        for c in classify::confirm_deletions(candidates, &self.indexes, &self.journal.folder) {
            self.breaker.observe(policy, &c.path, &c.before, c.tracked);
        }
    }

    pub fn task_exit_fd(&self) -> i32 {
        self.tasks
            .task
            .as_ref()
            .and_then(|task| task.pidfd.as_ref())
            .map_or(-1, AsRawFd::as_raw_fd)
    }
    pub fn prune_tasks(&mut self) {
        self.tasks.prune();
    }
    pub fn exited(&mut self, pid: u32) {
        self.complete(pid);
        if self.tasks.task.as_ref().is_some_and(|task| task.pid == pid) {
            self.tasks.task = None;
        }
    }
    pub fn background(&self) -> anyhow::Result<Self> {
        let cwd = journal::path(&self.journal.meta.cwd)?;
        let label = format!("background of command {}", self.journal.meta.id);
        let j = Journal::start_session(
            self.journal.home.clone(),
            &[label.into()],
            &cwd,
            -1,
            self.journal.meta.session.clone(),
        )?;
        let mut next = Self::new(j)?;
        next.policy = self.policy.clone();
        next.prior_created = self.prior_created.clone();
        next.prior_created.extend(
            self.created_paths()
                .filter_map(crate::classify::Birth::read),
        );
        Ok(next)
    }

    pub fn created_paths(&self) -> impl Iterator<Item = &Path> {
        self.created.iter().map(Path::new)
    }
    pub fn settle(&mut self) {
        self.tasks.task = None;
        let pids: Vec<_> = self.pending.keys().copied().collect();
        for pid in pids {
            self.complete(pid);
        }
        self.check_git_deletions(true);
    }
    pub fn snapshot(&mut self) {
        if self.policy.is_some() {
            self.observe_pending();
        }
        if let Err(e) = self.journal.snapshot() {
            self.journal.error(&e);
        }
    }
    pub fn finish(&mut self, status: i32) {
        self.settle();
        if let Err(e) = self.journal.finish(status) {
            self.journal.error(&e);
        }
    }
}
