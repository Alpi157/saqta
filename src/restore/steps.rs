//! Replayable filesystem actions. Each handler recognizes an already completed step.
use super::*;

impl Undo {
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

    fn copy_atomic(&self, from: &Path, to: &Path) -> io::Result<()> {
        let marker = self.backup_path.join(format!(
            "copy-{}-{}.json",
            self.recovery.effect, self.recovery.cursor
        ));
        let mut progress: (String, Option<State>) = if marker.exists() {
            serde_json::from_slice(&fs::read(&marker)?).map_err(io::Error::other)?
        } else {
            let temp =
                to.with_file_name(format!(".saqta-restore-{}-{}", sys::pid(), journal::now()));
            let progress = (journal::display(&temp), None);
            journal::json_write(&marker, &progress)?;
            progress
        };
        let temp = journal::path(&progress.0)?;
        if progress.1.is_none() {
            if self.state(&temp)?.is_some() {
                // A killed copy may be partial. Retain it before rebuilding from its immutable source.
                let partial = self.backup_path.join(format!(
                    "partial-{}-{}-{}",
                    self.recovery.effect,
                    self.recovery.cursor,
                    journal::now()
                ));
                match self.rename(&temp, &partial) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        let a = self.parent(&temp)?;
                        let b = self.parent(&partial)?;
                        let st = sys::stat_at(a.fd.as_raw_fd(), &a.name)?
                            .ok_or_else(|| io::Error::other("missing partial copy"))?;
                        self.copy_entry(a.fd.as_raw_fd(), &a.name, b.fd.as_raw_fd(), &b.name, &st)?;
                        sys::unlink_at(
                            a.fd.as_raw_fd(),
                            &a.name,
                            st.st_mode & libc::S_IFMT == libc::S_IFDIR,
                        )?;
                    }
                    Err(e) => return Err(e),
                }
            }
            let a = self.parent(from)?;
            let b = self.parent(&temp)?;
            self.copy_tree(a.fd.as_raw_fd(), &a.name, b.fd.as_raw_fd(), &b.name)?;
            progress.1 = self.state(&temp)?;
            journal::json_write(&marker, &progress)?;
        }
        let st = progress
            .1
            .ok_or_else(|| io::Error::other("missing staged copy"))?;
        if self.state(&temp)?.is_none() && self.state(to)?.is_some_and(|s| s.same_inode(&st)) {
            return Ok(());
        }
        self.rename(&temp, to)
    }
    pub(super) fn execute(&mut self) -> io::Result<()> {
        while let Some(action) = self.recovery.pending.get(self.recovery.cursor).cloned() {
            match action {
                Action::Move { from, to } => self.execute_move(from, to)?,
                Action::Copy { from, to } => {
                    self.copy_atomic(&journal::path(&from)?, &journal::path(&to)?)?
                }
                Action::Directory { path, before } => self.execute_directory(path, before)?,
                Action::Write { from, to, before } => self.execute_write(from, to, before)?,
                Action::Exchange {
                    a,
                    b,
                    first,
                    second,
                } => self.execute_exchange(a, b, first, second)?,
            }
            // Abort after the mutation but before its completion record. Resume must identify it.
            #[cfg(debug_assertions)]
            if std::env::var("SAQTA_TEST_ABORT_AFTER")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                == Some(self.recovery.steps + 1)
            {
                sys::exit(86);
            }
            self.recovery.cursor += 1;
            self.recovery.steps += 1;
            self.recovery.before = None;
            if self.recovery.cursor < self.recovery.pending.len() {
                self.save_state()?;
            }
        }
        Ok(())
    }
    fn execute_move(&mut self, from: String, to: String) -> io::Result<()> {
        let from = journal::path(&from)?;
        let to = journal::path(&to)?;
        if self.recovery.before.is_none() {
            self.recovery.before = self.state(&from)?;
            if self.recovery.before.is_none() {
                return Err(io::Error::other("missing recovery move source"));
            }
            self.save_state()?;
        }
        let expected = self
            .recovery
            .before
            .as_ref()
            .ok_or_else(|| io::Error::other("missing move identity"))?;
        if self.state(&from)?.is_none() {
            if !self.state(&to)?.is_some_and(|s| s.same_inode(expected)) {
                // Cross-filesystem copy has its own durable publication identity.
                self.copy_atomic(&from, &to)?;
            }
        } else {
            if !self.state(&from)?.is_some_and(|s| s.same_inode(expected)) {
                return Err(io::Error::other(
                    "recovery move input changed; kept both versions",
                ));
            }
            match self.rename(&from, &to) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                    self.copy_atomic(&from, &to)?;
                    let a = self.parent(&from)?;
                    sys::unlink_at(a.fd.as_raw_fd(), &a.name, expected.directory())?;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    fn execute_directory(&mut self, path: String, before: State) -> io::Result<()> {
        let p = journal::path(&path)?;
        let at = self.parent(&p)?;
        if sys::stat_at(at.fd.as_raw_fd(), &at.name)?.is_none() {
            sys::mkdir_at(at.fd.as_raw_fd(), &at.name, before.mode & 0o7777)?;
        }
        let fd = sys::open_at(
            at.fd.as_raw_fd(),
            &at.name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        sys::metadata_fd(fd.as_raw_fd(), before.mode, before.mtime, before.mtime_ns)?;

        Ok(())
    }

    fn execute_write(&mut self, from: String, to: String, before: State) -> io::Result<()> {
        let to = journal::path(&to)?;
        let at = self.parent(&to)?;
        let fd = sys::open_at(
            at.fd.as_raw_fd(),
            &at.name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0o600,
        )?;
        let mut dst = File::from(fd);
        if !from.is_empty() {
            let source = self.parent(&journal::path(&from)?)?;
            let mut src = File::from(sys::open_at(
                source.fd.as_raw_fd(),
                &source.name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )?);
            dst.set_len(0)?;
            use std::io::Read;
            if io::copy(&mut (&mut src).take(before.size as u64), &mut dst)? != before.size as u64 {
                return Err(io::Error::other("short recovery write; source retained"));
            }
        }
        dst.set_len(before.size as u64)?;
        sys::metadata_fd(dst.as_raw_fd(), before.mode, before.mtime, before.mtime_ns)?;
        dst.sync_all()?;

        Ok(())
    }

    fn execute_exchange(
        &mut self,
        a: String,
        b: String,
        first: State,
        second: State,
    ) -> io::Result<()> {
        let mut first = first;
        let mut second = second;
        let path_a = journal::path(&a)?;
        let path_b = journal::path(&b)?;
        if self.recovery.before.is_none() {
            first = self
                .state(&path_a)?
                .ok_or_else(|| io::Error::other("missing exchange input"))?;
            second = self
                .state(&path_b)?
                .ok_or_else(|| io::Error::other("missing exchange input"))?;
            self.recovery.before = Some(first.clone());
            self.recovery.pending[self.recovery.cursor] = Action::Exchange {
                a: a.clone(),
                b: b.clone(),
                first: first.clone(),
                second: second.clone(),
            };
            self.save_state()?;
        }
        let a = path_a;
        let b = path_b;
        if !(self.state(&a)?.is_some_and(|s| s.same_inode(&second))
            && self.state(&b)?.is_some_and(|s| s.same_inode(&first)))
        {
            let x = self.parent(&a)?;
            let y = self.parent(&b)?;
            sys::rename_at(
                x.fd.as_raw_fd(),
                &x.name,
                y.fd.as_raw_fd(),
                &y.name,
                libc::RENAME_EXCHANGE,
            )?;
        }

        Ok(())
    }
}
