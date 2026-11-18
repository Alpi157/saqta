//! Undo checks conflicts, plans reverse effects and keeps alias lineage.
mod checkpoints;
mod steps;
use crate::{
    journal::{self, Effect, State},
    output::Progress,
    paths, sys,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};

fn cstr(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes()).map_err(io::Error::other)
}
struct Parent {
    fd: Rc<OwnedFd>,
    name: CString,
}
#[derive(Clone, Serialize, Deserialize)]
enum Action {
    Move {
        from: String,
        to: String,
    },
    Copy {
        from: String,
        to: String,
    },
    Write {
        from: String,
        to: String,
        before: State,
    },
    Directory {
        path: String,
        before: State,
    },
    Exchange {
        a: String,
        b: String,
        first: State,
        second: State,
    },
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Recovery {
    completed_through: Option<u64>,
    origins: BTreeMap<String, (State, State)>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    alias_parents: BTreeMap<String, State>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    repaired_aliases: BTreeMap<String, (State, State)>,
    serial: u64,
    conflicts: HashSet<String>,
    git_indexes: HashSet<String>,
    pending: Vec<Action>,
    effect: u64,
    cursor: usize,
    before: Option<State>,
    steps: u64,
}
struct Undo {
    root: OwnedFd,
    qfd: Rc<OwnedFd>,
    backup_fd: Rc<OwnedFd>,
    checkpoints: checkpoints::Checkpoints,
    absent: HashSet<PathBuf>,
    log: File,
    recovery: Recovery,
    backup_path: PathBuf,
    qpath: PathBuf,
    aliases: Vec<String>,
    alias_keys: BTreeSet<(u64, u64)>,
    output: Progress<io::Stdout>,
}
impl Undo {
    fn parent(&self, p: &Path) -> io::Result<Parent> {
        let parent = p
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent"))?;
        let pinned = if parent.as_os_str() == self.qpath.as_os_str() {
            Some(&self.qfd)
        } else if parent.as_os_str() == self.backup_path.as_os_str() {
            Some(&self.backup_fd)
        } else {
            None
        };
        if let Some(fd) = pinned {
            return Ok(Parent {
                fd: Rc::clone(fd),
                name: cstr(
                    p.file_name()
                        .ok_or_else(|| io::Error::other("cannot restore root"))?,
                )?,
            });
        }
        let parent = parent.strip_prefix("/").map_err(io::Error::other)?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        Ok(Parent {
            fd: Rc::new(sys::open_restore_parent(
                self.root.as_raw_fd(),
                &cstr(parent.as_os_str())?,
            )?),
            name: cstr(
                p.file_name()
                    .ok_or_else(|| io::Error::other("cannot restore root"))?,
            )?,
        })
    }
    fn state(&self, p: &Path) -> io::Result<Option<State>> {
        if p.parent().is_some_and(|parent| {
            parent.as_os_str() == self.qpath.as_os_str()
                || parent.as_os_str() == self.backup_path.as_os_str()
        }) {
            let at = self.parent(p)?;
            return Ok(sys::stat_at(at.fd.as_raw_fd(), &at.name)?
                .as_ref()
                .map(State::from_stat));
        }
        current_secure(self.root.as_raw_fd(), p)
    }
    fn save_state(&mut self) -> io::Result<()> {
        self.checkpoints.record(&self.recovery)
    }
    fn push_move(&mut self, from: &Path, to: &Path) {
        self.recovery.pending.push(Action::Move {
            from: journal::display(from),
            to: journal::display(to),
        });
    }
    fn push_copy(&mut self, from: &Path, to: &Path) {
        self.recovery.pending.push(Action::Copy {
            from: journal::display(from),
            to: journal::display(to),
        });
    }
    fn save_current(&mut self, p: &Path) -> io::Result<Option<(PathBuf, State)>> {
        let at = match self.parent(p) {
            Ok(at) => at,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let Some(raw) = sys::stat_at(at.fd.as_raw_fd(), &at.name)? else {
            return Ok(None);
        };
        let shared = raw.st_nlink > 1;
        let st = State::from_stat(&raw);
        self.recovery.serial += 1;
        let saved = self.backup_path.join(self.recovery.serial.to_string());
        writeln!(
            self.log,
            "{}\t{}\t{}",
            self.recovery.serial,
            journal::display(p),
            serde_json::to_string(&st).map_err(io::Error::other)?
        )?;
        self.log.sync_data()?;
        self.push_move(p, &saved);
        let retained = if st.directory() || (st.mode & libc::S_IFMT == libc::S_IFREG && shared) {
            // Keep an inode handle for older alias repairs, and an independent
            // numeric snapshot for the user. Later writes cannot change that snapshot.
            let held = self
                .backup_path
                .join(format!("inode-{}", self.recovery.serial));
            self.push_move(&saved, &held);
            self.push_copy(&held, &saved);
            held
        } else {
            saved.clone()
        };
        if self.recovery.git_indexes.remove(&journal::display(p)) {
            self.output.line(format_args!(
                "kept the newer git index in {} (git rewrites it on commands like git status)",
                journal::display(&saved)
            ));
        }
        Ok(Some((retained, st)))
    }
    fn keep_move_input(&mut self, p: &Path) -> io::Result<()> {
        if self.recovery.conflicts.remove(&journal::display(p))
            && let Some((saved, _)) = self.save_current(p)?
        {
            self.push_copy(&saved, p);
        }
        Ok(())
    }
    fn prepare_sizes(&self, record: &journal::Record) -> io::Result<()> {
        for e in record
            .effects
            .iter()
            .filter(|e| e.op == "size" && record.outcomes.get(&e.seq) != Some(&false))
        {
            let target = self.backup_path.join(format!("size-{}", e.seq));
            if target.exists() {
                continue;
            }
            let before = e
                .note
                .before
                .as_ref()
                .ok_or_else(|| io::Error::other("missing size metadata"))?;
            let source = record
                .effects
                .iter()
                .filter(|other| other.seq >= e.seq && !other.quarantine.is_empty())
                .find(|other| {
                    other
                        .note
                        .before
                        .as_ref()
                        .is_some_and(|s| s.same_inode(before))
                })
                .map(|other| self.qpath.join(&other.quarantine))
                .or_else(|| {
                    let mut p = journal::path(&e.path).ok()?;
                    for later in record
                        .effects
                        .iter()
                        .filter(|v| v.seq > e.seq && record.outcomes.get(&v.seq) != Some(&false))
                    {
                        if matches!(later.op.as_str(), "rename" | "exchange") {
                            let a = journal::path(&later.path).ok()?;
                            let b = journal::path(&later.path2).ok()?;
                            if let Ok(tail) = p.strip_prefix(&a) {
                                p = b.join(tail);
                            } else if later.op == "exchange"
                                && let Ok(tail) = p.strip_prefix(&b)
                            {
                                p = a.join(tail);
                            }
                        }
                    }
                    self.state(&p)
                        .ok()
                        .flatten()
                        .filter(|s| s.mode & libc::S_IFMT == libc::S_IFREG && s.size >= before.size)
                        .map(|_| p)
                });
            let source =
                source.ok_or_else(|| io::Error::other("missing original prefix for grown file"))?;
            let at = self.parent(&source)?;
            let mut source = File::from(sys::open_at(
                at.fd.as_raw_fd(),
                &at.name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )?);
            let staging = self.backup_path.join(format!("size-{}-staging", e.seq));
            if staging.exists() {
                self.rename(
                    &staging,
                    &self
                        .backup_path
                        .join(format!("size-{}-partial-{}", e.seq, journal::now())),
                )?;
            }
            let at = self.parent(&staging)?;
            let mut dst = File::from(sys::open_at(
                at.fd.as_raw_fd(),
                &at.name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                0o600,
            )?);
            use std::io::Read;
            if io::copy(&mut (&mut source).take(before.size as u64), &mut dst)?
                != before.size as u64
            {
                return Err(io::Error::other("short original prefix; originals kept"));
            }
            sys::metadata_fd(dst.as_raw_fd(), before.mode, before.mtime, before.mtime_ns)?;
            dst.sync_all()?;
            self.rename(&staging, &target)?;
        }
        Ok(())
    }
    fn alias_paths(&self, p: &Path, before: &State) -> io::Result<BTreeSet<PathBuf>> {
        let mut aliases = BTreeSet::from([p.to_owned()]);
        let mut live_original = false;
        for name in self.aliases.iter().chain(self.recovery.origins.keys()) {
            let candidate = journal::path(name)?;
            if let Ok(Some(current)) = self.state(&candidate) {
                let restored = self
                    .recovery
                    .origins
                    .get(name)
                    .is_some_and(|(origin, actual)| {
                        origin.same_inode(before) && current.same_inode(actual)
                    });
                live_original |= current.same_inode(before);
                if current.same_inode(before) || restored {
                    aliases.insert(candidate);
                }
            }
        }
        if !live_original {
            let mut dirs = vec![self.backup_path.clone()];
            while let Some(dir) = dirs.pop() {
                for entry in fs::read_dir(dir)? {
                    let entry = entry?;
                    let st = fs::symlink_metadata(entry.path())?;
                    if st.is_dir() {
                        dirs.push(entry.path());
                    } else if st.is_file()
                        && self
                            .state(&entry.path())?
                            .is_some_and(|s| s.same_inode(before))
                    {
                        aliases.insert(entry.path());
                    }
                }
            }
        }
        Ok(aliases)
    }
    fn remember_origins(&mut self, e: &Effect) -> io::Result<()> {
        if matches!(e.op.as_str(), "create" | "rename" | "exchange") {
            reverse_repair_names(&mut self.recovery.alias_parents, e)?;
            reverse_repair_names(&mut self.recovery.repaired_aliases, e)?;
        }
        if matches!(e.op.as_str(), "rename" | "exchange") {
            let a = journal::path(&e.path)?;
            let b = journal::path(&e.path2)?;
            let old = std::mem::take(&mut self.recovery.origins);
            for (name, value) in old {
                let p = journal::path(&name)?;
                let p = if let Ok(tail) = p.strip_prefix(&b) {
                    a.join(tail)
                } else if e.op == "exchange"
                    && let Ok(tail) = p.strip_prefix(&a)
                {
                    b.join(tail)
                } else {
                    p
                };
                self.recovery.origins.insert(journal::display(&p), value);
            }
        }
        for (name, before) in [
            (&e.path, e.note.before.as_ref()),
            (
                &e.path2,
                if e.op == "exchange" {
                    e.note.before2.as_ref()
                } else {
                    None
                },
            ),
        ] {
            if let Some(before) = before
                && self.alias_keys.contains(&(before.dev, before.ino))
            {
                let p = journal::path(name)?;
                if let Some(actual) = self.state(&p)? {
                    self.recovery
                        .origins
                        .insert(name.clone(), (before.clone(), actual));
                }
            }
        }
        Ok(())
    }
    fn plan_repair(&mut self, e: &Effect, p: &Path, source: &Path) -> io::Result<()> {
        if let Some((live, st)) = self.save_current(p)?
            && st.mode & libc::S_IFMT == libc::S_IFREG
        {
            // Keep the inode even when its other alias is temporarily absent. Older
            // effects still identify it when reversing a later deletion or growth.
            let post = self
                .backup_path
                .join(format!("{}-post", self.recovery.serial));
            let before = e
                .note
                .before
                .clone()
                .ok_or_else(|| io::Error::other("missing pre-image metadata"))?;
            let name = journal::display(p);
            if name != e.path && !p.starts_with(&self.backup_path) {
                // The effect log cannot refresh lineage or directory state for
                // an alias it never named.
                self.recovery
                    .repaired_aliases
                    .insert(name.clone(), (before.clone(), st));
                if let Some(parent) = p.parent()
                    && let Ok(Some(state)) = self.state(parent)
                {
                    self.recovery
                        .alias_parents
                        .insert(journal::display(parent), state);
                }
            }
            self.push_copy(&live, &post);
            self.recovery.pending.push(Action::Write {
                from: journal::display(source),
                to: journal::display(&live),
                before,
            });
            self.push_move(&live, p);
            self.push_move(&post, &live);
        } else {
            self.push_copy(source, p);
        }
        Ok(())
    }
    fn plan(&mut self, e: &Effect) -> io::Result<()> {
        let p = journal::path(&e.path)?;
        match e.op.as_str() {
            "modify" if !e.quarantine.is_empty() => {
                let source = self.qpath.join(&e.quarantine);
                let before = e
                    .note
                    .before
                    .as_ref()
                    .ok_or_else(|| io::Error::other("missing pre-image metadata"))?;
                for p in self.alias_paths(&p, before)? {
                    self.plan_repair(e, &p, &source)?;
                }
            }
            "delete" if !e.quarantine.is_empty() => {
                if !self.absent.contains(&p) {
                    self.save_current(&p)?;
                }
                let source = self.qpath.join(&e.quarantine);
                self.push_move(&source, &p);
            }
            "create" => {
                self.save_current(&p)?;
            }
            "rmdir" => {
                if self.recovery.conflicts.remove(&e.path) {
                    self.save_current(&p)?;
                }
                if self.state(&p)?.is_none() || !self.recovery.pending.is_empty() {
                    self.recovery.pending.push(Action::Directory {
                        path: e.path.clone(),
                        before: e
                            .note
                            .before
                            .clone()
                            .ok_or_else(|| io::Error::other("missing directory metadata"))?,
                    });
                }
            }
            "size" => {
                let before = e
                    .note
                    .before
                    .as_ref()
                    .ok_or_else(|| io::Error::other("missing size metadata"))?;
                let aliases = self.alias_paths(&p, before)?;
                let source = self.backup_path.join(format!("size-{}", e.seq));
                for p in aliases {
                    self.plan_repair(e, &p, &source)?;
                }
            }
            "rename" => {
                let dest = journal::path(&e.path2)?;
                if self.state(&dest)?.is_none() {
                    return Ok(());
                }
                if self
                    .state(&p)?
                    .zip(e.note.before.as_ref())
                    .is_some_and(|(a, b)| a.same_inode(b))
                {
                    return Ok(());
                }
                self.keep_move_input(&dest)?;
                self.save_current(&p)?;
                self.push_move(&dest, &p);
            }
            "exchange" => {
                let dest = journal::path(&e.path2)?;
                let first = self.state(&p)?;
                let second = self.state(&dest)?;
                let (first, second) = match (first, second) {
                    (Some(a), Some(b)) => (a, b),
                    (None, Some(_)) => {
                        self.push_move(&dest, &p);
                        return Ok(());
                    }
                    (Some(_), None) => {
                        self.push_move(&p, &dest);
                        return Ok(());
                    }
                    (None, None) => return Ok(()),
                };
                self.keep_move_input(&p)?;
                self.keep_move_input(&dest)?;
                // Input copies receive fresh identities. The executor records those just before exchanging.
                self.recovery.pending.push(Action::Exchange {
                    a: e.path.clone(),
                    b: e.path2.clone(),
                    first,
                    second,
                });
            }
            _ => {}
        }
        Ok(())
    }
    fn save_directory_observations(
        &self,
        home: &Path,
        r: &journal::Record,
        restored: &BTreeMap<String, State>,
        observations: BTreeMap<String, Option<State>>,
    ) -> io::Result<()> {
        let mut observations: BTreeMap<PathBuf, Option<State>> = observations
            .into_iter()
            .map(|(name, state)| Ok((journal::path(&name)?, state)))
            .collect::<io::Result<_>>()?;
        for e in applied_effects(r).rev() {
            reverse_names(&mut observations, e)?;
        }
        let mut observations: BTreeMap<String, Option<State>> = observations
            .into_iter()
            .map(|(name, state)| (journal::display(&name), state))
            .collect();
        let mut parents = HashSet::new();
        for e in &r.effects {
            for name in [&e.path, &e.path2].into_iter().filter(|s| !s.is_empty()) {
                let p = journal::path(name)?;
                parents.extend(p.ancestors().skip(1).map(Path::to_owned));
            }
        }
        // Reverse exchanges can move a changed directory below an ancestor
        // whose name also moved. Observe its final name, not just effect paths.
        for (name, before) in restored {
            let p = journal::path(name)?;
            parents.extend(
                p.ancestors()
                    .skip(usize::from(!before.directory()))
                    .map(Path::to_owned),
            );
        }
        for p in parents {
            if let Ok(st) = self.state(&p)
                && st.as_ref().is_some_and(State::directory)
            {
                observations.insert(journal::display(&p), st);
            }
        }
        for (name, before) in &self.recovery.alias_parents {
            let p = journal::path(name)?;
            if let Ok(Some(current)) = self.state(&p)
                && current.directory()
                && current.same_inode(before)
            {
                observations.insert(name.clone(), Some(current));
            }
        }
        journal::synced_json_write(&home.join("undo-posts.json"), &observations)?;
        Ok(())
    }
    fn save_alias_observations(
        &self,
        home: &Path,
        restored: BTreeMap<String, State>,
        mut alias_posts: BTreeMap<String, (State, State)>,
    ) -> io::Result<()> {
        for (name, (origin, repaired)) in &self.recovery.repaired_aliases {
            if let Some(actual) = self.state(&journal::path(name)?)?
                && actual.same_inode(repaired)
            {
                alias_posts.insert(name.clone(), (origin.clone(), actual));
            }
        }
        for (name, origin) in restored {
            if origin.mode & libc::S_IFMT == libc::S_IFREG
                && let Some(actual) = self.state(&journal::path(&name)?)?
            {
                alias_posts.insert(name, (origin, actual));
            }
        }
        let mut dirs = vec![self.backup_path.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let st = fs::symlink_metadata(entry.path())?;
                if st.is_dir() {
                    dirs.push(entry.path());
                } else if st.is_file()
                    && let Some(actual) = self.state(&entry.path())?
                {
                    alias_posts.insert(journal::display(&entry.path()), (actual.clone(), actual));
                }
            }
        }
        journal::synced_json_write(&home.join("undo-aliases.json"), &alias_posts)?;
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let a = self.parent(from)?;
        let b = self.parent(to)?;
        sys::rename_at(
            a.fd.as_raw_fd(),
            &a.name,
            b.fd.as_raw_fd(),
            &b.name,
            libc::RENAME_NOREPLACE,
        )
    }
}

pub fn snapshot_entry(root: i32, path: &Path, target: i32, name: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .and_then(|p| p.strip_prefix("/").ok())
        .ok_or_else(|| io::Error::other("invalid snapshot path"))?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let source = sys::open_restore_parent(root, &cstr(parent.as_os_str())?)?;
    let source_name = cstr(
        path.file_name()
            .ok_or_else(|| io::Error::other("missing snapshot name"))?,
    )?;
    Undo::copy_tree(
        source.as_raw_fd(),
        &source_name,
        target,
        &CString::new(name)?,
    )
}

pub(crate) fn current_secure(root: i32, p: &Path) -> io::Result<Option<State>> {
    let parent = p
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?
        .strip_prefix("/")
        .map_err(io::Error::other)?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let fd = match sys::open_restore_parent(root, &cstr(parent.as_os_str())?) {
        Ok(fd) => fd,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let name = p
        .file_name()
        .ok_or_else(|| io::Error::other("cannot inspect root"))?;
    Ok(sys::stat_at(fd.as_raw_fd(), &cstr(name)?)?
        .as_ref()
        .map(State::from_stat))
}
fn same_post(a: Option<&State>, b: Option<&State>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.mode == b.mode && a.size == b.size && a.mtime == b.mtime && a.mtime_ns == b.mtime_ns
        }
        _ => false,
    }
}
fn restored_origins(record: &journal::Record) -> io::Result<BTreeMap<String, State>> {
    let mut origins: BTreeMap<PathBuf, State> = BTreeMap::new();
    for e in applied_effects(record).rev() {
        let p = journal::path(&e.path)?;
        reverse_names(&mut origins, e)?;
        if e.op != "create"
            && let Some(before) = &e.note.before
        {
            origins.insert(p, before.clone());
        }
        if e.op == "exchange"
            && let Some(before) = &e.note.before2
        {
            origins.insert(journal::path(&e.path2)?, before.clone());
        }
    }
    Ok(origins
        .into_iter()
        .map(|(p, s)| (journal::display(&p), s))
        .collect())
}
fn applied_effects(record: &journal::Record) -> impl DoubleEndedIterator<Item = &Effect> {
    record.effects.iter().filter(|e| {
        record.outcomes.get(&e.seq) != Some(&false)
            && !matches!(e.op.as_str(), "blocked" | "not protected" | "expired")
    })
}
fn reverse_repair_names<T>(names: &mut BTreeMap<String, T>, e: &Effect) -> io::Result<()> {
    if !names.is_empty() {
        let mut paths = std::mem::take(names)
            .into_iter()
            .map(|(name, value)| Ok((journal::path(&name)?, value)))
            .collect::<io::Result<_>>()?;
        reverse_names(&mut paths, e)?;
        names.extend(
            paths
                .into_iter()
                .map(|(path, value)| (journal::display(&path), value)),
        );
    }
    Ok(())
}
fn take_subtree<T>(names: &mut BTreeMap<PathBuf, T>, source: &Path) -> Vec<(PathBuf, T)> {
    let keys: Vec<_> = names
        .range(source.to_owned()..)
        .take_while(|(name, _)| name.starts_with(source))
        .map(|(name, _)| name.clone())
        .collect();
    keys.into_iter()
        .filter_map(|name| names.remove(&name).map(|value| (name, value)))
        .collect()
}
fn reverse_names<T>(names: &mut BTreeMap<PathBuf, T>, e: &Effect) -> io::Result<()> {
    if !matches!(e.op.as_str(), "create" | "rename" | "exchange") {
        return Ok(());
    }
    let source = journal::path(&e.path)?;
    if e.op == "create" {
        take_subtree(names, &source);
    } else {
        let dest = journal::path(&e.path2)?;
        let to_source = take_subtree(names, &dest);
        let to_dest = take_subtree(names, &source);
        for (name, value) in to_source {
            let tail = name.strip_prefix(&dest).map_err(io::Error::other)?;
            names.insert(
                if tail.as_os_str().is_empty() {
                    source.clone()
                } else {
                    source.join(tail)
                },
                value,
            );
        }
        if e.op == "exchange" {
            for (name, value) in to_dest {
                let tail = name.strip_prefix(&source).map_err(io::Error::other)?;
                names.insert(
                    if tail.as_os_str().is_empty() {
                        dest.clone()
                    } else {
                        dest.join(tail)
                    },
                    value,
                );
            }
        }
    }
    Ok(())
}
fn is_git_index(p: &Path) -> bool {
    if p.file_name() != Some(OsStr::new("index")) {
        return false;
    }
    let Some(parent) = p.parent() else {
        return false;
    };
    parent.file_name() == Some(OsStr::new(".git"))
        || parent.parent().is_some_and(|worktrees| {
            worktrees.file_name() == Some(OsStr::new("worktrees"))
                && worktrees.parent().and_then(Path::file_name) == Some(OsStr::new(".git"))
        })
}

fn conflict_message(id: u64, conflicts: &[String], backup: &Path) -> String {
    let mut message = format!(
        "paths changed after command {id}; refusing to overwrite newer work:\n{}",
        conflicts
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    if conflicts.len() > 10 {
        message.push_str(&format!("\n+{} more", conflicts.len() - 10));
    }
    message.push_str(&format!(
        "\nRun `saqta undo {id} --force` to restore anyway. Every newer version is moved to {} first, so nothing is lost.",
        journal::display(backup)
    ));
    message
}

#[derive(Default)]
struct Summary {
    counts: [usize; 5],
    examples: Vec<String>,
}
impl Summary {
    fn record(&mut self, e: &Effect) {
        let category = match e.op.as_str() {
            "delete" => 0,
            "modify" | "size" => 1,
            "create" => 2,
            "rmdir" => 3,
            "rename" | "exchange" => 4,
            _ => return,
        };
        self.counts[category] += 1;
        for path in [&e.path, &e.path2] {
            if !path.is_empty() && !self.examples.contains(path) {
                self.examples.push(path.clone());
                self.examples
                    .sort_by_key(|s| journal::path(s).is_ok_and(|p| crate::gc::regenerable(&p)));
                self.examples.truncate(10);
            }
        }
    }

    fn print(
        &self,
        output: &mut Progress<io::Stdout>,
        dry_run: bool,
        verbose: bool,
        started: Instant,
    ) {
        let labels = if dry_run {
            [
                "would restore deleted files",
                "would restore modified files",
                "would remove created paths",
                "would recreate directories",
                "would reverse renames",
            ]
        } else {
            [
                "restored deleted files",
                "restored modified files",
                "removed created paths",
                "recreated directories",
                "reversed renames",
            ]
        };
        for (label, count) in labels.iter().zip(self.counts) {
            if count > 0 {
                output.line(format_args!("{label}: {count}"));
            }
        }
        output.line(format_args!(
            "elapsed: {:.3} s",
            started.elapsed().as_secs_f64()
        ));
        if !verbose {
            for path in &self.examples {
                output.line(format_args!("example path: {path}"));
            }
        }
    }
}

fn report_unrestored(output: &mut Progress<io::Stdout>, effects: &[Effect]) {
    // Report every unprotected item before starting, even if a later restore
    // fails. These diagnostics are never limited by the example-path budget.
    for e in effects {
        match e.op.as_str() {
            "not protected" => output.line(format_args!(
                "could not restore {}: not protected: {}",
                e.path, e.note.message
            )),
            "expired" => output.line(format_args!("{}: expired (collected by gc)", e.path)),
            "blocked" => output.line(format_args!("blocked {}: {}", e.path, e.note.message)),
            _ => {}
        }
    }
}

fn restore_effects(
    undo: &mut Undo,
    record: &journal::Record,
    verbose: bool,
    summary: &mut Summary,
) -> Result<()> {
    for e in record.effects.iter().rev() {
        if record.outcomes.get(&e.seq) == Some(&false)
            || matches!(e.op.as_str(), "blocked" | "not protected" | "expired")
        {
            continue;
        }
        if undo
            .recovery
            .completed_through
            .is_some_and(|seq| e.seq >= seq)
        {
            continue;
        }
        if undo.recovery.effect != e.seq {
            undo.recovery.pending.clear();
            undo.recovery.cursor = 0;
            undo.recovery.before = None;
            undo.recovery.effect = e.seq;
            undo.plan(e)?;
            if let Some(Action::Move { from, .. }) = undo.recovery.pending.first() {
                undo.recovery.before = undo.state(&journal::path(from)?)?;
            }
            undo.save_state()?;
        }
        undo.execute().with_context(|| {
            format!(
                "could not undo {} {}; recovery files: {}",
                e.op,
                e.path,
                journal::display(&undo.backup_path)
            )
        })?;
        undo.remember_origins(e)?;
        undo.recovery.completed_through = Some(e.seq);
        // The next plan checkpoints this completion before its first mutation.
        // If killed first, replay recognizes the last action from its identities.
        summary.record(e);
        if verbose {
            undo.output
                .line(format_args!("restored {} {}", e.op, e.path));
        }
    }
    // Flush the final completion too, including an empty command.
    undo.save_state()?;
    undo.checkpoints.publish(&undo.recovery)?;
    Ok(())
}

pub fn run(id: u64, dry_run: bool, force: bool, verbose: bool, resume: bool) -> Result<i32> {
    // Fault-injection handshake: stop before undo can change anything. A parent
    // can observe T in /proc without racing a fast one-file resume.
    #[cfg(debug_assertions)]
    if std::env::var_os("SAQTA_TEST_STOP_UNDO").is_some_and(|s| s == "1") {
        sys::kill(sys::pid(), libc::SIGSTOP);
    }
    let mut output = Progress::new(io::stdout());
    if std::env::var_os("SAQTA_SUPERVISED").is_some() {
        bail!("run undo in your own terminal");
    }
    let home = paths::home()?;
    let r = journal::refresh(&home, id)?;
    if journal::running(&home, &r.meta)
        || (r.status == "incomplete"
            && (journal::process_live(r.meta.child)
                || r.effects
                    .iter()
                    .any(|e| journal::process_live(e.pid as i32))))
    {
        bail!("command {id}'s tree is still running");
    }
    if r.undone && !resume {
        bail!("command {id} already undone");
    }
    let folder = home.join("cmd").join(id.to_string());
    let release_from = folder.join("release-from");
    if release_from.exists() {
        let held: u64 = serde_json::from_slice(&fs::read(release_from)?)?;
        if !crate::hold::directory(&home, held)
            .join("released")
            .exists()
        {
            bail!("release for command {held} is incomplete. Run `saqta release {held}` to resume");
        }
    }
    let lock = journal::private_file(&folder.join("undo.lock"))?;
    sys::flock_exclusive(lock.as_raw_fd(), true).context("another undo is in progress")?;
    if folder.join("undone").exists() {
        if resume && folder.join("undo-in-progress").exists() {
            fs::remove_file(folder.join("undo-in-progress"))?;
            output.line(format_args!("finished undoing command {id}"));
            return Ok(0);
        }
        bail!("command {id} already undone");
    }
    if resume && crate::hold::pending(&home, id) {
        crate::hold::resume(&home, r)?;
        if !folder.join("end").exists() {
            journal::Journal::reopen(home, id)?.end(77 << 8)?;
        }
        output.line(format_args!("finished automatic restore for command {id}"));
        return Ok(0);
    }
    if crate::hold::pending(&home, id) {
        bail!("an automatic restore stopped partway. Run `saqta undo {id} --resume`");
    }
    let mut r = r;
    let mut posts = journal::post_states(&folder)?;
    crate::hold::exclude_restored(&home, &mut r, &mut posts)?;
    let backup_path = home
        .join("q/main")
        .join(format!("undo-{id}-{}", r.meta.start_ns));
    let qpath = home.join("q/main").join(id.to_string());
    apply(
        r,
        Apply {
            home: &home,
            folder: &folder,
            backup_path,
            qpath,
            posts,
            dry_run,
            force,
            verbose,
            resume,
            automatic: false,
            seal: None,
        },
    )
}

pub struct Apply<'a> {
    pub home: &'a Path,
    pub folder: &'a Path,
    pub backup_path: PathBuf,
    pub qpath: PathBuf,
    pub posts: BTreeMap<String, Option<State>>,
    pub dry_run: bool,
    pub force: bool,
    pub verbose: bool,
    pub resume: bool,
    pub automatic: bool,
    pub seal: Option<&'a Path>,
}

pub fn apply(r: journal::Record, options: Apply<'_>) -> Result<i32> {
    let Apply {
        home,
        folder,
        backup_path,
        qpath,
        mut posts,
        dry_run,
        force,
        verbose,
        resume,
        automatic,
        seal,
    } = options;
    let id = r.meta.id;
    let started = Instant::now();
    let mut output = if automatic {
        Progress::silent(io::stdout())
    } else {
        Progress::new(io::stdout())
    };
    if folder.join("undone").exists() {
        if resume {
            let _ = fs::remove_file(folder.join("undo-in-progress"));
            return Ok(0);
        }
        bail!("command {id} already undone");
    }
    if let Some(warning) = &r.warning {
        bail!("journal writes failed: {warning}; inspect quarantine before restoring by hand");
    }
    let resuming = folder.join("undo-in-progress").exists()
        || (resume && backup_path.join("state.json").exists());
    if resuming && !resume {
        bail!(
            "a previous undo stopped partway; run `saqta undo {id} --resume`; recovery files: {}",
            folder.display()
        );
    }
    let root = sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
    let observations: std::collections::BTreeMap<String, Option<State>> =
        fs::read(home.join("undo-posts.json"))
            .ok()
            .map(|b| serde_json::from_slice(&b))
            .transpose()?
            .unwrap_or_default();
    let mut conflicts = Vec::new();
    let mut inaccessible = HashSet::new();
    for e in r.effects.iter().filter(|_| !resuming) {
        if matches!(e.op.as_str(), "blocked" | "expired") || r.outcomes.get(&e.seq) == Some(&false)
        {
            continue;
        }
        for name in [&e.path, &e.path2] {
            if name.is_empty() {
                continue;
            }
            let p = journal::path(name)?;
            let current = current_secure(root.as_raw_fd(), &p);
            match current {
                Ok(current) => {
                    if let Some(expected) = posts.get(name) {
                        let matches = if automatic {
                            expected == &current
                        } else {
                            same_post(expected.as_ref(), current.as_ref())
                                || (expected.as_ref().is_some_and(State::directory)
                                    && observations.get(name) == Some(&current))
                        };
                        if !matches {
                            conflicts.push(name.clone());
                        }
                    } else if r.status == "incomplete" {
                        posts.insert(name.clone(), current);
                    } else {
                        bail!("missing post-state for {name}; refusing undo");
                    }
                }
                Err(_)
                    if posts.get(name) == Some(&None)
                        && p.ancestors().skip(1).any(|parent| {
                            posts
                                .get(&journal::display(parent))
                                .is_some_and(|expected| {
                                    expected.is_some()
                                        && current_secure(root.as_raw_fd(), parent).is_ok_and(
                                            |current| {
                                                same_post(expected.as_ref(), current.as_ref())
                                            },
                                        )
                                })
                        }) => {}
                Err(_) => {
                    inaccessible.insert(name.clone());
                    conflicts.push(name.clone());
                }
            }
        }
    }
    if automatic && !resuming {
        for (name, expected) in &posts {
            if current_secure(root.as_raw_fd(), &journal::path(name)?)
                .as_ref()
                .ok()
                != Some(expected)
            {
                conflicts.push(name.clone());
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    let (git_indexes, conflicts): (Vec<_>, Vec<_>) = conflicts.into_iter().partition(|name| {
        !automatic
            && !inaccessible.contains(name)
            && journal::path(name).is_ok_and(|p| is_git_index(&p))
    });
    if !conflicts.is_empty() && !force && !dry_run {
        if automatic {
            let visible = conflicts
                .iter()
                .take(10)
                .map(String::as_str)
                .collect::<Vec<_>>();
            bail!(
                "checked post-state changed for {} path(s): {}. Retained versions kept.",
                conflicts.len(),
                visible.join(", ")
            );
        }
        bail!("{}", conflict_message(id, &conflicts, &backup_path));
    }
    if !inaccessible.is_empty() && !dry_run {
        bail!(
            "cannot safely open parents for changed paths; --force cannot bypass directory traversal checks; nothing restored:\n{}",
            conflicts
                .iter()
                .filter(|name| inaccessible.contains(*name))
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let q = sys::open_at(
        libc::AT_FDCWD,
        &cstr(qpath.as_os_str())?,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    for e in r.effects.iter().filter(|_| !resuming) {
        if r.outcomes.get(&e.seq) == Some(&false) {
            continue;
        }
        if !e.quarantine.is_empty() && e.op != "expired" {
            let name = CString::new(e.quarantine.as_bytes())?;
            let saved = sys::stat_at(q.as_raw_fd(), &name)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "missing pre-image {} for {}; nothing restored",
                    e.quarantine,
                    e.path
                )
            })?;
            if let Some(expected) = &e.note.before
                && (saved.st_mode != expected.mode
                    || saved.st_size != expected.size
                    || saved.st_mtime != expected.mtime
                    || saved.st_mtime_nsec != expected.mtime_ns)
            {
                bail!("pre-image changed for {}; nothing restored", e.path);
            }
        }
    }
    if dry_run {
        output.line(format_args!("would undo command {id} (dry run)"));
        if !conflicts.is_empty() {
            output.line(format_args!(
                "{}",
                conflict_message(id, &conflicts, &backup_path)
            ));
            output.line(format_args!(
                "with --force, would keep newer entries there before restoring"
            ));
        }
        if !inaccessible.is_empty() {
            output.line(format_args!(
                "--force cannot bypass directory traversal checks; repair the changed parents first"
            ));
        }
        for name in &git_indexes {
            output.line(format_args!(
                "would keep the newer git index {name} in {} before restoring",
                journal::display(&backup_path)
            ));
        }
        report_unrestored(&mut output, &r.effects);
        let mut summary = Summary::default();
        for e in r
            .effects
            .iter()
            .rev()
            .filter(|e| r.outcomes.get(&e.seq) != Some(&false))
        {
            summary.record(e);
            if verbose && !matches!(e.op.as_str(), "blocked" | "not protected" | "expired") {
                output.line(format_args!("would undo {} {}", e.op, e.path));
            }
        }
        summary.print(&mut output, true, verbose, started);
        output.line(format_args!(
            "would keep displaced versions in {} (see recovery.log after undo)",
            journal::display(&backup_path)
        ));
        return Ok(0);
    }
    output.line(format_args!("undoing command {id}"));
    let alias_keys: BTreeSet<_> = r
        .effects
        .iter()
        .filter(|e| matches!(e.op.as_str(), "modify" | "size"))
        .filter_map(|e| e.note.before.as_ref().map(|s| (s.dev, s.ino)))
        .collect();
    let alias_posts: BTreeMap<String, (State, State)> = fs::read(home.join("undo-aliases.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b))
        .transpose()?
        .unwrap_or_default();
    let recovery = if resuming {
        checkpoints::load(&backup_path)?
    } else {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).recursive(automatic && resume);
        builder.create(&backup_path)?;
        let mut origins = BTreeMap::new();
        for (name, (origin, actual)) in &alias_posts {
            if alias_keys.contains(&(origin.dev, origin.ino))
                && current_secure(root.as_raw_fd(), &journal::path(name)?)
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(actual)
            {
                origins.insert(name.clone(), (origin.clone(), actual.clone()));
            }
        }
        Recovery {
            origins,
            conflicts: conflicts
                .into_iter()
                .chain(git_indexes.iter().cloned())
                .collect(),
            git_indexes: git_indexes.into_iter().collect(),
            ..Recovery::default()
        }
    };
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(backup_path.join("recovery.log"))?;
    let mut undo = Undo {
        root,
        qfd: Rc::new(q),
        backup_fd: Rc::new(sys::open_at(
            libc::AT_FDCWD,
            &cstr(backup_path.as_os_str())?,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?),
        absent: if resuming {
            HashSet::new()
        } else {
            posts
                .iter()
                .filter(|(name, state)| state.is_none() && !recovery.conflicts.contains(*name))
                .map(|(name, _)| journal::path(name))
                .collect::<io::Result<_>>()?
        },
        checkpoints: checkpoints::Checkpoints::open(&backup_path, &recovery)?,
        log,
        recovery,
        backup_path: backup_path.clone(),
        qpath,
        alias_keys,
        aliases: r
            .effects
            .iter()
            .flat_map(|e| [e.path.clone(), e.path2.clone()])
            .filter(|s| !s.is_empty())
            .collect(),
        output,
    };
    if !resuming {
        undo.save_state()?;
        journal::synced_json_write(
            &folder.join("undo-in-progress"),
            &journal::display(&backup_path),
        )?;
    }
    // Reuse the same input map after interruption, even if publishing the final
    // home observations succeeded before undo was marked complete.
    let observed_before = backup_path.join("directory-observations.json");
    let observations = if observed_before.exists() {
        serde_json::from_slice(&fs::read(&observed_before)?)?
    } else {
        journal::synced_json_write(&observed_before, &observations)?;
        observations
    };
    if let Some(note) = &r.recovery_note {
        undo.output.line(format_args!("{note}"));
    }
    undo.prepare_sizes(&r)?;
    report_unrestored(&mut undo.output, &r.effects);
    let mut summary = Summary::default();
    let result = restore_effects(&mut undo, &r, verbose, &mut summary).and_then(|()| {
        let restored = restored_origins(&r)?;
        undo.save_directory_observations(home, &r, &restored, observations)?;
        undo.save_alias_observations(home, restored, alias_posts)?;
        if let Some(path) = seal {
            crate::hold::seal_posts(undo.root.as_raw_fd(), &r.effects, path)?;
        }
        #[cfg(debug_assertions)]
        if !automatic
            && std::env::var_os("SAQTA_TEST_ABORT_UNDO_FINALIZE").is_some_and(|s| s == "1")
        {
            sys::exit(86);
        }
        journal::mark_undone(folder)?;
        fs::remove_file(folder.join("undo-in-progress"))?;
        Ok(())
    });
    summary.print(&mut undo.output, false, verbose, started);
    if undo.recovery.serial > 0 {
        undo.output.line(format_args!(
            "displaced versions kept in {} (see recovery.log)",
            journal::display(&backup_path)
        ));
    }
    result?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_examples_prefer_source_paths() {
        let mut summary = Summary::default();
        let mut effect = Effect {
            seq: 0,
            time_ns: 0,
            pid: 0,
            op: "delete".into(),
            path: String::new(),
            path2: String::new(),
            quarantine: String::new(),
            flags: 0,
            note: crate::journal::Detail::default(),
        };
        for i in 0..12 {
            effect.path = format!("/repo/node_modules/{i}");
            summary.record(&effect);
        }
        effect.path = "/repo/src/keep.rs".into();
        summary.record(&effect);
        assert_eq!(summary.examples.len(), 10);
        assert_eq!(summary.examples[0], "/repo/src/keep.rs");
        assert_eq!(summary.counts, [13, 0, 0, 0, 0]);
    }
    #[test]
    fn git_index_exception_has_exact_component_boundaries() {
        for path in ["/repo/.git/index", "/repo/.git/worktrees/topic/index"] {
            assert!(is_git_index(Path::new(path)), "{path}");
        }
        for path in [
            "/repo/index",
            "/repo/not.git/index",
            "/repo/.git/objects/index",
            "/repo/.git/worktrees/index",
            "/repo/.git/worktrees/topic/nested/index",
            "/repo/.git/index.lock",
            "/repo/.git/worktrees/topic/index.lock",
        ] {
            assert!(!is_git_index(Path::new(path)), "{path}");
        }
    }
}
