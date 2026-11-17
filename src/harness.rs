//! Debug-only syscall driver. Its fixed namespace and pinned parents cannot escape root.
use crate::{journal, sys};
use anyhow::{Context, Result, bail};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

#[path = "harness_ops.rs"]
mod ops;
use ops::{Batch, Op};
pub fn name(slot: u8) -> PathBuf {
    let slot = slot % 18;
    if slot < 6 {
        PathBuf::from(format!("d{slot}"))
    } else {
        PathBuf::from(format!("d{}/f{}", (slot - 6) / 2, (slot - 6) % 2))
    }
}
fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)
}
fn relative(p: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::ParentDir if out.pop() => {}
            _ => return Err(io::Error::other("path escapes fixture")),
        }
    }
    Ok(out)
}
fn parent(root: i32, p: &Path) -> io::Result<(std::os::fd::OwnedFd, CString)> {
    let dir = p
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut fd = sys::open_at(
        root,
        c".",
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    for part in dir.components() {
        if let Component::Normal(n) = part {
            fd = sys::open_at(
                fd.as_raw_fd(),
                &cstr(Path::new(n))?,
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )?;
        } else if part != Component::CurDir {
            return Err(io::Error::other("unsafe parent"));
        }
    }
    Ok((
        fd,
        cstr(Path::new(
            p.file_name()
                .ok_or_else(|| io::Error::other("missing name"))?,
        ))?,
    ))
}
fn follow(root: i32, mut p: PathBuf) -> io::Result<PathBuf> {
    for _ in 0..40 {
        let (at, name) = parent(root, &p)?;
        if !sys::stat_at(at.as_raw_fd(), &name)?
            .is_some_and(|s| s.st_mode & libc::S_IFMT == libc::S_IFLNK)
        {
            return Ok(p);
        }
        let target = fs::read_link(
            PathBuf::from(format!("/proc/self/fd/{}", at.as_raw_fd()))
                .join(std::ffi::OsStr::from_bytes(name.as_bytes())),
        )?;
        p = relative(&p.parent().unwrap_or(Path::new("")).join(target))?;
    }
    Err(io::Error::from_raw_os_error(libc::ELOOP))
}
fn apply(root: i32, op: &Op) -> io::Result<()> {
    let p = name(op.a);
    if matches!(op.kind, 1..=6 | 14) {
        follow(root, p.clone())?;
    }
    let (at, name) = parent(root, &p)?;
    match op.kind {
        0..=3 | 14 => {
            let flags = match op.kind {
                0 => libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                1 | 14 => libc::O_WRONLY | libc::O_TRUNC | libc::O_CREAT,
                2 => libc::O_RDWR,
                _ => libc::O_WRONLY | libc::O_APPEND,
            };
            let mut f = File::from(sys::open_at(
                at.as_raw_fd(),
                &name,
                flags | libc::O_NONBLOCK | if op.kind == 0 { libc::O_NOFOLLOW } else { 0 },
                0o640,
            )?);
            if op.kind == 2 {
                f.seek(SeekFrom::Start(u64::from(op.n % 16)))?;
            }
            f.write_all(&op.data)
        }
        4..=6 => {
            let path = fs::read_link(format!("/proc/self/fd/{root}"))?.join(&p);
            sys::truncate_path(
                &cstr(&path)?,
                if op.kind == 4 {
                    u64::from(op.n % 8)
                } else {
                    64 + u64::from(op.n)
                },
            )
        }
        7 | 8 => {
            let (to, dest) = parent(root, &name_path(op.b))?;
            sys::rename_at(
                at.as_raw_fd(),
                &name,
                to.as_raw_fd(),
                &dest,
                if op.kind == 8 {
                    libc::RENAME_EXCHANGE
                } else {
                    0
                },
            )
        }
        9 => sys::unlink_at(at.as_raw_fd(), &name, false),
        10 => sys::unlink_at(at.as_raw_fd(), &name, true),
        11 => sys::mkdir_at(at.as_raw_fd(), &name, 0o750),
        12 => {
            let (to, dest) = parent(root, &name_path(op.b))?;
            sys::link_at(at.as_raw_fd(), &name, to.as_raw_fd(), &dest)
        }
        13 => {
            let target = if p.parent().is_some_and(|d| !d.as_os_str().is_empty()) {
                Path::new("..").join(name_path(op.b))
            } else {
                name_path(op.b)
            };
            sys::symlink_at(&cstr(&target)?, at.as_raw_fd(), &name)
        }
        _ => Err(io::Error::other("unknown operation")),
    }
}
fn name_path(slot: u8) -> PathBuf {
    name(slot)
}
pub fn run() -> Result<()> {
    let root = std::env::args_os()
        .nth(2)
        .context("fixture root required")?;
    let root = fs::canonicalize(root)?;
    if root == Path::new("/") {
        bail!("fixture root cannot be /");
    }
    let mut bytes = Vec::new();
    io::stdin().read_to_end(&mut bytes)?;
    let batch: Batch = serde_json::from_slice(&bytes)?;
    if batch.processes.len() > 1 {
        let mut children = Vec::new();
        for (i, ops) in batch.processes.into_iter().enumerate() {
            let sub = root.join(format!("p{i}"));
            if !fs::symlink_metadata(&sub)?.is_dir() {
                bail!("worker root must be a real directory");
            }
            let mut child = Command::new(std::env::current_exe()?)
                .arg("__harness")
                .arg(sub)
                .stdin(Stdio::piped())
                .spawn()?;
            serde_json::to_writer(
                child.stdin.take().context("child stdin")?,
                &Batch {
                    processes: vec![ops],
                },
            )?;
            children.push(child);
        }
        for mut child in children {
            if !child.wait()?.success() {
                bail!("worker failed");
            }
        }
    } else {
        let fd = sys::open_at(
            libc::AT_FDCWD,
            &cstr(&root)?,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        for (i, op) in batch.processes.into_iter().flatten().enumerate() {
            let result = apply(fd.as_raw_fd(), &op);
            println!(
                "pid {} [{}] op {i}: {op:?}: {}",
                sys::pid(),
                journal::display(&root),
                result.map_or_else(|e| e.to_string(), |_| "ok".into())
            );
        }
    }
    Ok(())
}
