//! Undo checks all conflicts first, then walks canonical parents without symlinks.
use crate::{
    journal::{self, Effect, State},
    paths, sys,
};
use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
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
        }
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
        // Keep the grown version in quarantine and restore only its old prefix.
        current.st_size = before.size;
        self.copy_entry(
            self.backup.as_raw_fd(),
            &live,
            at.fd.as_raw_fd(),
            &at.name,
            &current,
        )?;
        let destination = sys::open_at(
            at.fd.as_raw_fd(),
            &at.name,
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        let destination = File::from(destination);
        destination.set_len(before.size as u64)?;
        sys::metadata_fd(
            destination.as_raw_fd(),
            before.mode,
            before.mtime,
            before.mtime_ns,
        )?;
        destination.sync_all()?;
        Ok(())
    }
    fn apply(&mut self, e: &Effect) -> io::Result<()> {
        let p = journal::path(&e.path)?;
        match e.op.as_str() {
            "delete" => self.restore_kept(e, &p),
            "modify" => self.restore_kept(e, &p),
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
pub fn run(id: u64, dry_run: bool) -> Result<i32> {
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
                Err(_) => conflicts.push(name.clone()),
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    if !conflicts.is_empty() {
        bail!(
            "paths changed after command {id}:\n{}",
            conflicts.join("\n")
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
        for e in r
            .effects
            .iter()
            .rev()
            .filter(|e| r.outcomes.get(&e.seq) != Some(&false))
        {
            println!("would undo {} {}", e.op, e.path);
        }
        return Ok(0);
    }
    let backup_path = home
        .join("q/main")
        .join(format!("undo-{id}-{}", journal::now()));
    journal::private_dir(&backup_path)?;
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
    };
    for e in r.effects.iter().rev() {
        if r.outcomes.get(&e.seq) == Some(&false) {
            continue;
        }
        if e.op == "blocked" {
            println!("blocked {}: {}", e.path, e.note.message);
            continue;
        }
        if e.op == "not protected" {
            println!(
                "could not restore {}: not protected: {}",
                e.path, e.note.message
            );
            continue;
        }
        undo.apply(e).with_context(|| {
            format!(
                "could not undo {} {}; recovery files: {}",
                e.op,
                e.path,
                backup_path.display()
            )
        })?;
        println!("restored {} {}", e.op, e.path);
    }
    journal::mark_undone(&folder)?;
    fs::remove_file(folder.join("undo-in-progress"))?;
    journal::import(&home)?;
    Ok(0)
}
