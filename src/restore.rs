//! Undo checks all conflicts first, then walks canonical parents without symlinks.
use crate::{
    journal::{self, Effect, State},
    output::Progress,
    paths, sys,
};
use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet},
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
    time::Instant,
};

fn cstr(p: &OsStr) -> io::Result<CString> {
    CString::new(p.as_bytes()).map_err(io::Error::other)
}
struct Parent {
    fd: OwnedFd,
    name: CString,
}
struct Undo {
    root: OwnedFd,
    q: OwnedFd,
    backup: OwnedFd,
    log: File,
    serial: u64,
    copies: HashMap<String, usize>,
    conflicts: HashSet<String>,
    git_indexes: HashSet<String>,
    backup_path: PathBuf,
    output: Progress<io::Stdout>,
}
impl Undo {
    fn parent(&self, p: &Path) -> io::Result<Parent> {
        let parent = p
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent"))?;
        let parent = parent.strip_prefix("/").map_err(io::Error::other)?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let name = p
            .file_name()
            .ok_or_else(|| io::Error::other("cannot restore root"))?;
        Ok(Parent {
            fd: sys::open_restore_parent(self.root.as_raw_fd(), &cstr(parent.as_os_str())?)?,
            name: cstr(name)?,
        })
    }
    fn save_current(&mut self, p: &Path, at: &Parent) -> io::Result<()> {
        let Some(st) = sys::stat_at(at.fd.as_raw_fd(), &at.name)? else {
            return Ok(());
        };
        self.serial += 1;
        let q = CString::new(self.serial.to_string()).map_err(io::Error::other)?;
        // The recovery map is durable before the name can move.
        writeln!(
            self.log,
            "{}\t{}\t{}",
            self.serial,
            journal::display(p),
            serde_json::to_string(&State::from_stat(&st)).map_err(io::Error::other)?
        )?;
        self.log.sync_data()?;
        match sys::rename_at(
            at.fd.as_raw_fd(),
            &at.name,
            self.backup.as_raw_fd(),
            &q,
            libc::RENAME_NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                self.copy_entry(
                    at.fd.as_raw_fd(),
                    &at.name,
                    self.backup.as_raw_fd(),
                    &q,
                    &st,
                )?;
                sys::unlink_at(
                    at.fd.as_raw_fd(),
                    &at.name,
                    st.st_mode & libc::S_IFMT == libc::S_IFDIR,
                )
            }
            Err(e) => Err(e),
        }?;
        if self.git_indexes.remove(&journal::display(p)) {
            self.output.line(format_args!(
                "kept the newer git index in {} (git rewrites it on commands like git status)",
                journal::display(&self.backup_path.join(self.serial.to_string()))
            ));
        }
        Ok(())
    }
    // Reverse moves still need their source. Keep the conflicting entry first,
    // then make an independent working copy so the retained version never moves.
    fn keep_move_input(&mut self, p: &Path, at: &Parent) -> io::Result<()> {
        if !self.conflicts.remove(&journal::display(p))
            || sys::stat_at(at.fd.as_raw_fd(), &at.name)?.is_none()
        {
            return Ok(());
        }
        self.save_current(p, at)?;
        let saved = CString::new(self.serial.to_string()).map_err(io::Error::other)?;
        self.copy_tree(self.backup.as_raw_fd(), &saved, at.fd.as_raw_fd(), &at.name)
    }
    fn copy_tree(
        &self,
        from: i32,
        name: &std::ffi::CStr,
        to: i32,
        dest: &std::ffi::CStr,
    ) -> io::Result<()> {
        let st = sys::stat_at(from, name)?
            .ok_or_else(|| io::Error::other("missing displaced move input"))?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return self.copy_entry(from, name, to, dest, &st);
        }
        let source = sys::open_at(
            from,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        sys::mkdir_at(to, dest, 0o700)?;
        let target = sys::open_at(
            to,
            dest,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", source.as_raw_fd()))? {
            let child = cstr(&entry?.file_name())?;
            self.copy_tree(source.as_raw_fd(), &child, target.as_raw_fd(), &child)?;
        }
        sys::metadata_fd(
            target.as_raw_fd(),
            st.st_mode,
            st.st_mtime,
            st.st_mtime_nsec,
        )
    }
    fn copy_entry(
        &self,
        from: i32,
        name: &std::ffi::CStr,
        to: i32,
        dest: &std::ffi::CStr,
        st: &libc::stat,
    ) -> io::Result<()> {
        match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                let src = sys::open_at(
                    from,
                    name,
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                    0,
                )?;
                let dst = sys::open_at(
                    to,
                    dest,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o600,
                )?;
                sys::copy_regular(File::from(src), File::from(dst), st)
            }
            libc::S_IFLNK => {
                let target = fs::read_link(
                    PathBuf::from(format!("/proc/self/fd/{from}"))
                        .join(OsStr::from_bytes(name.to_bytes())),
                )?;
                sys::symlink_at(&cstr(target.as_os_str())?, to, dest)?;
                sys::symlink_mtime(to, dest, st.st_mtime, st.st_mtime_nsec)
            }
            libc::S_IFDIR => {
                // Cross-filesystem directory backups are safe only when empty.
                let source = sys::open_at(
                    from,
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                    0,
                )?;
                if fs::read_dir(format!("/proc/self/fd/{}", source.as_raw_fd()))?
                    .next()
                    .is_some()
                {
                    return Err(io::Error::other(
                        "cannot move a nonempty directory across filesystems; kept it in place",
                    ));
                }
                sys::mkdir_at(to, dest, st.st_mode & 0o7777)?;
                let fd = sys::open_at(
                    to,
                    dest,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )?;
                sys::metadata_fd(fd.as_raw_fd(), st.st_mode, st.st_mtime, st.st_mtime_nsec)
            }
            _ => Err(io::Error::other(
                "cannot copy a special file across filesystems; kept it in place",
            )),
        }
    }
    fn restore_kept(&mut self, e: &Effect, p: &Path) -> io::Result<()> {
        if e.quarantine.is_empty() {
            return Ok(());
        }
        let name = CString::new(e.quarantine.as_bytes()).map_err(io::Error::other)?;
        let st = sys::stat_at(self.q.as_raw_fd(), &name)?
            .ok_or_else(|| io::Error::other("missing quarantine pre-image"))?;
        let at = self.parent(p)?;
        self.save_current(p, &at)?;
        let remaining = self
            .copies
            .get_mut(&e.quarantine)
            .ok_or_else(|| io::Error::other("missing quarantine reference count"))?;
        *remaining -= 1;
        let last = *remaining == 0;
        if last {
            match sys::rename_at(
                self.q.as_raw_fd(),
                &name,
                at.fd.as_raw_fd(),
                &at.name,
                libc::RENAME_NOREPLACE,
            ) {
                Ok(()) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                Err(e) => return Err(e),
            }
        }
        // Reused pre-images must remain available for the earlier effects.
        self.copy_entry(self.q.as_raw_fd(), &name, at.fd.as_raw_fd(), &at.name, &st)
    }
    fn restore_modified(&mut self, e: &Effect, p: &Path) -> io::Result<()> {
        if e.quarantine.is_empty() {
            return Ok(());
        }
        let at = self.parent(p)?;
        let current = sys::stat_at(at.fd.as_raw_fd(), &at.name)?;
        let Some(current) =
            current.filter(|s| s.st_mode & libc::S_IFMT == libc::S_IFREG && s.st_nlink > 1)
        else {
            return self.restore_kept(e, p);
        };
        // Restore through a surviving shared inode so untouched aliases regain
        // their bytes too. First detach an independent post-command backup.
        self.save_current(p, &at)?;
        let live = CString::new(self.serial.to_string()).map_err(io::Error::other)?;
        let post = CString::new(format!("{}-post", self.serial)).map_err(io::Error::other)?;
        self.copy_entry(
            self.backup.as_raw_fd(),
            &live,
            self.backup.as_raw_fd(),
            &post,
            &current,
        )?;
        let name = CString::new(e.quarantine.as_bytes()).map_err(io::Error::other)?;
        let src = sys::open_at(
            self.q.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        let dst = sys::open_at(
            self.backup.as_raw_fd(),
            &live,
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        let actual = sys::stat_fd(dst.as_raw_fd())?;
        if actual.st_dev != current.st_dev
            || actual.st_ino != current.st_ino
            || actual.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(io::Error::other("shared inode changed during undo"));
        }
        let before = e
            .note
            .before
            .as_ref()
            .ok_or_else(|| io::Error::other("missing pre-image metadata"))?;
        let mut source = File::from(src);
        let mut destination = File::from(dst);
        destination.set_len(0)?;
        let copied = io::copy(&mut source, &mut destination)?;
        if copied != before.size as u64 {
            return Err(io::Error::other(
                "short shared-inode restore; both versions remain in quarantine",
            ));
        }
        sys::metadata_fd(
            destination.as_raw_fd(),
            before.mode,
            before.mtime,
            before.mtime_ns,
        )?;
        destination.sync_all()?;
        sys::rename_at(
            self.backup.as_raw_fd(),
            &live,
            at.fd.as_raw_fd(),
            &at.name,
            libc::RENAME_NOREPLACE,
        )?;
        sys::rename_at(
            self.backup.as_raw_fd(),
            &post,
            self.backup.as_raw_fd(),
            &live,
            libc::RENAME_NOREPLACE,
        )?;
        let references = self
            .copies
            .get_mut(&e.quarantine)
            .ok_or_else(|| io::Error::other("missing pre-image reference count"))?;
        *references -= 1;
        Ok(())
    }
    fn restore_size(&mut self, e: &Effect, p: &Path) -> io::Result<()> {
        let at = self.parent(p)?;
        let before = e
            .note
            .before
            .as_ref()
            .ok_or_else(|| io::Error::other("missing original size"))?;
        let mut current = sys::stat_at(at.fd.as_raw_fd(), &at.name)?
            .ok_or_else(|| io::Error::other("missing grown file"))?;
        self.save_current(p, &at)?;
        let live = CString::new(self.serial.to_string()).map_err(io::Error::other)?;
        let post = CString::new(format!("{}-post", self.serial)).map_err(io::Error::other)?;
        let shared = current.st_nlink > 1;
        let destination = if shared {
            self.copy_entry(
                self.backup.as_raw_fd(),
                &live,
                self.backup.as_raw_fd(),
                &post,
                &current,
            )?;
            sys::open_at(
                self.backup.as_raw_fd(),
                &live,
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )?
        } else {
            // Preserve the full grown file in the backup, but copy only the old
            // prefix back. A large sparse extension must not force a huge copy.
            current.st_size = before.size;
            self.copy_entry(
                self.backup.as_raw_fd(),
                &live,
                at.fd.as_raw_fd(),
                &at.name,
                &current,
            )?;
            sys::open_at(
                at.fd.as_raw_fd(),
                &at.name,
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )?
        };
        let destination = File::from(destination);
        destination.set_len(before.size as u64)?;
        sys::metadata_fd(
            destination.as_raw_fd(),
            before.mode,
            before.mtime,
            before.mtime_ns,
        )?;
        destination.sync_all()?;
        if shared {
            sys::rename_at(
                self.backup.as_raw_fd(),
                &live,
                at.fd.as_raw_fd(),
                &at.name,
                libc::RENAME_NOREPLACE,
            )?;
            sys::rename_at(
                self.backup.as_raw_fd(),
                &post,
                self.backup.as_raw_fd(),
                &live,
                libc::RENAME_NOREPLACE,
            )?;
        }
        Ok(())
    }
    fn apply(&mut self, e: &Effect) -> io::Result<()> {
        let p = journal::path(&e.path)?;
        match e.op.as_str() {
            "delete" => self.restore_kept(e, &p),
            "modify" => self.restore_modified(e, &p),
            "create" => {
                let at = match self.parent(&p) {
                    Ok(a) => a,
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
                    Err(e) => return Err(e),
                };
                self.save_current(&p, &at)
            }
            "rmdir" => {
                let at = self.parent(&p)?;
                if self.conflicts.remove(&e.path) {
                    self.save_current(&p, &at)?;
                }
                if sys::stat_at(at.fd.as_raw_fd(), &at.name)?.is_none() {
                    let st = e
                        .note
                        .before
                        .as_ref()
                        .ok_or_else(|| io::Error::other("missing directory metadata"))?;
                    sys::mkdir_at(at.fd.as_raw_fd(), &at.name, st.mode & 0o7777)?;
                    let fd = sys::open_at(
                        at.fd.as_raw_fd(),
                        &at.name,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                        0,
                    )?;
                    sys::metadata_fd(fd.as_raw_fd(), st.mode, st.mtime, st.mtime_ns)?;
                }
                Ok(())
            }
            "size" => self.restore_size(e, &p),
            "rename" => {
                let dest = journal::path(&e.path2)?;
                let to = self.parent(&p)?;
                let from = match self.parent(&dest) {
                    Ok(a) => a,
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
                    Err(e) => return Err(e),
                };
                if sys::stat_at(from.fd.as_raw_fd(), &from.name)?.is_none() {
                    return Ok(());
                }
                self.keep_move_input(&dest, &from)?;
                // An unacknowledged rename on a killed supervisor may never have run.
                if sys::stat_at(to.fd.as_raw_fd(), &to.name)?
                    .as_ref()
                    .map(State::from_stat)
                    .zip(e.note.before.as_ref())
                    .is_some_and(|(a, b)| a.same_inode(b))
                {
                    return Ok(());
                }
                self.save_current(&p, &to)?;
                writeln!(
                    self.log,
                    "rename\t{}\t{}",
                    journal::display(&dest),
                    journal::display(&p)
                )?;
                self.log.sync_data()?;
                sys::rename_at(
                    from.fd.as_raw_fd(),
                    &from.name,
                    to.fd.as_raw_fd(),
                    &to.name,
                    libc::RENAME_NOREPLACE,
                )
            }
            "exchange" => {
                let dest = journal::path(&e.path2)?;
                let a = self.parent(&p)?;
                let b = self.parent(&dest)?;
                self.keep_move_input(&p, &a)?;
                self.keep_move_input(&dest, &b)?;
                writeln!(
                    self.log,
                    "exchange\t{}\t{}",
                    journal::display(&p),
                    journal::display(&dest)
                )?;
                self.log.sync_data()?;
                sys::rename_at(
                    a.fd.as_raw_fd(),
                    &a.name,
                    b.fd.as_raw_fd(),
                    &b.name,
                    libc::RENAME_EXCHANGE,
                )
            }
            _ => Ok(()),
        }
    }
}
fn current_secure(root: i32, p: &Path) -> io::Result<Option<State>> {
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
            if !path.is_empty() && self.examples.len() < 10 && !self.examples.contains(path) {
                self.examples.push(path.clone());
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
            output.line(format_args!("{label}: {count}"));
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
            || matches!(e.op.as_str(), "blocked" | "not protected")
        {
            continue;
        }
        undo.apply(e).with_context(|| {
            format!(
                "could not undo {} {}; recovery files: {}",
                e.op,
                e.path,
                journal::display(&undo.backup_path)
            )
        })?;
        summary.record(e);
        if verbose {
            undo.output
                .line(format_args!("restored {} {}", e.op, e.path));
        }
    }
    Ok(())
}

pub fn run(id: u64, dry_run: bool, force: bool, verbose: bool) -> Result<i32> {
    let started = Instant::now();
    let mut output = Progress::new(io::stdout());
    if std::env::var_os("SAQTA_SUPERVISED").is_some() {
        bail!("run undo in your own terminal");
    }
    let home = paths::home()?;
    journal::import(&home)?;
    let r = journal::load(&home, id)?;
    if journal::running(&home, &r.meta)
        || (r.status == "incomplete"
            && (journal::process_live(r.meta.child)
                || r.effects
                    .iter()
                    .any(|e| journal::process_live(e.pid as i32))))
    {
        bail!("command {id}'s tree is still running");
    }
    if r.undone {
        bail!("command {id} already undone");
    }
    let folder = home.join("cmd").join(id.to_string());
    let lock = journal::private_file(&folder.join("undo.lock"))?;
    sys::flock_exclusive(lock.as_raw_fd(), true).context("another undo is in progress")?;
    if folder.join("undone").exists() {
        bail!("command {id} already undone");
    }
    if let Some(warning) = r.warning {
        bail!("journal writes failed: {warning}; inspect quarantine before restoring by hand");
    }
    if folder.join("undo-in-progress").exists() {
        bail!(
            "a previous undo stopped partway; inspect {} and its recovery map",
            folder.display()
        );
    }
    let root = sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
    let mut posts = journal::post_states(&folder)?;
    let mut conflicts = Vec::new();
    let mut inaccessible = HashSet::new();
    for e in &r.effects {
        if e.op == "blocked" {
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
                        if expected != &current {
                            conflicts.push(name.clone());
                        }
                    } else if r.status == "incomplete" {
                        posts.insert(name.clone(), current);
                    } else {
                        bail!("missing post-state for {name}; refusing undo");
                    }
                }
                Err(_) => {
                    inaccessible.insert(name.clone());
                    conflicts.push(name.clone());
                }
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    let (git_indexes, conflicts): (Vec<_>, Vec<_>) = conflicts.into_iter().partition(|name| {
        !inaccessible.contains(name) && journal::path(name).is_ok_and(|p| is_git_index(&p))
    });
    let backup_path = home
        .join("q/main")
        .join(format!("undo-{id}-{}", r.meta.start_ns));
    if !conflicts.is_empty() && !force && !dry_run {
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
    let qpath = home.join("q/main").join(id.to_string());
    let q = sys::open_at(
        libc::AT_FDCWD,
        &cstr(qpath.as_os_str())?,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    let mut copies = HashMap::new();
    for e in &r.effects {
        if r.outcomes.get(&e.seq) == Some(&false) {
            continue;
        }
        if !e.quarantine.is_empty() {
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
            *copies.entry(e.quarantine.clone()).or_insert(0) += 1;
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
            if verbose && !matches!(e.op.as_str(), "blocked" | "not protected") {
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
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&backup_path)
        .with_context(|| {
            format!(
                "cannot create undo folder {}; inspect it if it already exists",
                journal::display(&backup_path)
            )
        })?;
    let backup = sys::open_at(
        libc::AT_FDCWD,
        &cstr(backup_path.as_os_str())?,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    let log = OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .open(backup_path.join("recovery.log"))?;
    fs::write(
        folder.join("undo-in-progress"),
        journal::display(&backup_path),
    )?;
    let mut undo = Undo {
        root,
        q,
        backup,
        log,
        serial: 0,
        copies,
        conflicts: conflicts
            .into_iter()
            .chain(git_indexes.iter().cloned())
            .collect(),
        git_indexes: git_indexes.into_iter().collect(),
        backup_path: backup_path.clone(),
        output,
    };
    report_unrestored(&mut undo.output, &r.effects);
    let mut summary = Summary::default();
    let result = restore_effects(&mut undo, &r, verbose, &mut summary).and_then(|()| {
        journal::mark_undone(&folder)?;
        fs::remove_file(folder.join("undo-in-progress"))?;
        journal::import(&home)?;
        Ok(())
    });
    summary.print(&mut undo.output, false, verbose, started);
    if undo.serial > 0 {
        undo.output.line(format_args!(
            "displaced versions kept in {} (see recovery.log)",
            journal::display(&backup_path)
        ));
    } else {
        undo.output.line(format_args!("displaced versions: none"));
    }
    result?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
