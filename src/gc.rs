//! Retention only removes explicitly eligible command content, under the undo lock.
use crate::{
    journal::{self, Meta},
    output::Progress,
    paths, sys,
};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs, io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
const DAY: u64 = 86_400_000_000_000;
const COMPONENTS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    ".pytest_cache",
];
pub fn regenerable(p: &Path) -> bool {
    p.components()
        .any(|c| COMPONENTS.iter().any(|name| c.as_os_str() == *name))
}
fn cached(p: &Path) -> bool {
    if regenerable(p) {
        return true;
    }
    p.ancestors().skip(1).any(cache_marker)
}
pub fn cache_marker(dir: &Path) -> bool {
    use std::io::Read;
    let signature = b"Signature: 8a477f597d28d172789f06886806bc55";
    let Ok(name) = std::ffi::CString::new(dir.join("CACHEDIR.TAG").as_os_str().as_encoded_bytes())
    else {
        return false;
    };
    let Ok(fd) = sys::open_at(
        libc::AT_FDCWD,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    ) else {
        return false;
    };
    let mut bytes = [0; 43];
    std::fs::File::from(fd).read_exact(&mut bytes).is_ok() && bytes == *signature
}
pub fn quantity(s: &str, age: bool) -> Result<u64> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..split].parse::<u64>()?;
    let suffix = s[split..].trim().to_ascii_lowercase();
    let factor = if age {
        match suffix.as_str() {
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            "d" | "" => DAY,
            "w" => 7 * DAY,
            _ => bail!("use s, m, h, d or w for duration"),
        }
    } else {
        match suffix.as_str() {
            "" | "b" => 1,
            "kb" | "k" => 1000,
            "mb" | "m" => 1_000_000,
            "gb" | "g" => 1_000_000_000,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            _ => bail!("use B, KB, MB, GB, KiB, MiB or GiB for size"),
        }
    };
    n.checked_mul(factor)
        .ok_or_else(|| anyhow::anyhow!("quantity is too large"))
}
pub fn store_size(home: &Path) -> io::Result<u64> {
    fn walk(p: &Path, seen: &mut HashSet<(u64, u64)>) -> io::Result<u64> {
        let m = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        if m.is_dir() {
            let mut size = 0u64;
            for e in fs::read_dir(p)? {
                size = size.saturating_add(walk(&e?.path(), seen)?);
            }
            Ok(size)
        } else if seen.insert((m.dev(), m.ino())) {
            Ok(m.len())
        } else {
            Ok(0)
        }
    }
    let mut seen = HashSet::new();
    Ok(walk(&home.join("cmd"), &mut seen)?
        .saturating_add(walk(&home.join("q"), &mut seen)?)
        .saturating_add(walk(&home.join("holds"), &mut seen)?))
}
#[derive(Serialize)]
struct Collection {
    id: u64,
    kind: &'static str,
    paths: Vec<String>,
    bytes: u64,
}
#[derive(Serialize)]
struct Report {
    dry_run: bool,
    before_bytes: u64,
    after_bytes: u64,
    collections: Vec<Collection>,
    deferred: Option<String>,
}
struct Candidate {
    meta: Meta,
    folder: PathBuf,
}
fn candidates(home: &Path) -> Result<Vec<Candidate>> {
    let mut rows = Vec::new();
    if !home.join("cmd").exists() {
        return Ok(rows);
    }
    for entry in fs::read_dir(home.join("cmd"))? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let folder = entry.path();
        if folder.is_symlink() {
            continue;
        }
        if let Ok(bytes) = fs::read(folder.join("meta")) {
            let meta: Meta = journal::parse_json(&folder.join("meta"), &bytes)?;
            if meta.id != id {
                bail!("command id disagrees with folder");
            }
            rows.push(Candidate { meta, folder });
        }
    }
    rows.sort_by_key(|c| c.meta.id);
    Ok(rows)
}
fn protected(home: &Path, c: &Candidate, newest: &BTreeSet<u64>) -> bool {
    newest.contains(&c.meta.id)
        || crate::hold::protected(home, c.meta.id)
        || c.folder.join("undo-in-progress").exists()
        || journal::running(home, &c.meta)
        || !c.folder.join("end").exists()
}
fn qpath(home: &Path, id: u64) -> PathBuf {
    home.join("q/main").join(id.to_string())
}
pub fn expired(folder: &Path) -> Result<BTreeSet<String>> {
    Ok(journal::read_json_optional(&folder.join("expired.json"))?.unwrap_or_default())
}
fn collect_content(home: &Path, c: &Candidate, dry: bool) -> Result<Collection> {
    let effects = journal::read_effects(&c.folder)?;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in effects.iter().filter(|e| !e.quarantine.is_empty()) {
        groups
            .entry(e.quarantine.clone())
            .or_default()
            .push(e.path.clone());
    }
    let tagged: Vec<_> = effects
        .iter()
        .filter(|e| !e.quarantine.is_empty())
        .filter_map(|e| {
            let p = journal::path(&e.path).ok()?;
            (p.file_name() == Some(std::ffi::OsStr::new("CACHEDIR.TAG"))
                && fs::read(qpath(home, c.meta.id).join(&e.quarantine))
                    .is_ok_and(|b| b.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55")))
            .then(|| p.parent().map(Path::to_owned))
            .flatten()
        })
        .collect();
    let mut expired = expired(&c.folder)?;
    let mut paths = Vec::new();
    let mut bytes = 0;
    let mut remove = Vec::new();
    for (q, names) in groups {
        if expired.contains(&q)
            || !names.iter().all(|s| {
                journal::path(s)
                    .is_ok_and(|p| cached(&p) || tagged.iter().any(|dir| p.starts_with(dir)))
            })
        {
            continue;
        }
        // Quarantine names are single entries produced by capture. Refuse damaged logs.
        if Path::new(&q).components().count() != 1 || q == "." || q == ".." {
            bail!("invalid quarantine name");
        }
        let path = qpath(home, c.meta.id).join(&q);
        if let Ok(m) = fs::symlink_metadata(&path) {
            if m.is_dir() {
                continue;
            }
            bytes += m.len();
            paths.extend(names);
            expired.insert(q);
            remove.push(path);
        }
    }
    if !dry && !remove.is_empty() {
        // Expiry is durable before unlink. A crash can leave extra content, never an unlabelled gap.
        journal::synced_json_write(&c.folder.join("expired.json"), &expired)?;
        for p in remove {
            fs::remove_file(p)?;
        }
    }
    Ok(Collection {
        id: c.meta.id,
        kind: "regenerable",
        paths,
        bytes,
    })
}
fn whole_paths(home: &Path, c: &Candidate) -> Result<Vec<PathBuf>> {
    let mut paths = vec![c.folder.clone(), qpath(home, c.meta.id)];
    if home.join("q/main").exists() {
        for e in fs::read_dir(home.join("q/main"))? {
            let e = e?;
            if e.file_name()
                .to_str()
                .is_some_and(|s| s.starts_with(&format!("undo-{}-", c.meta.id)))
            {
                paths.push(e.path());
            }
        }
    }
    Ok(paths)
}
fn path_size(p: &Path) -> io::Result<u64> {
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if !m.is_dir() {
        return Ok(m.len());
    }
    let mut n = 0;
    for e in fs::read_dir(p)? {
        n += path_size(&e?.path())?;
    }
    Ok(n)
}
fn collect_whole(home: &Path, c: &Candidate, dry: bool, kind: &'static str) -> Result<Collection> {
    let mut paths = whole_paths(home, c)?;
    paths.sort_by_key(|p| p == &c.folder);
    if !dry {
        let names: BTreeSet<_> = journal::read_effects(&c.folder)?
            .into_iter()
            .filter_map(|e| (!e.quarantine.is_empty()).then_some(e.quarantine))
            .collect();
        journal::synced_json_write(&c.folder.join("expired.json"), &names)?;
    }
    let mut bytes = 0;
    for p in paths {
        bytes += path_size(&p)?;
        if !dry && p.exists() {
            fs::remove_dir_all(p)?;
        }
    }
    Ok(Collection {
        id: c.meta.id,
        kind,
        paths: vec![],
        bytes,
    })
}
fn validate_preimages(home: &Path, rows: &[Candidate]) -> Result<()> {
    for c in rows {
        if !c.folder.join("end").exists()
            || c.folder.join("undone").exists()
            || c.folder.join("undo-in-progress").exists()
        {
            continue;
        }
        let expired = expired(&c.folder)?;
        let restored = crate::hold::restored_sequences(home, c.meta.id)?;
        for e in journal::read_effects(&c.folder)? {
            if e.quarantine.is_empty()
                || expired.contains(&e.quarantine)
                || restored.contains(&e.seq)
            {
                continue;
            }
            let saved = journal::state(&qpath(home, c.meta.id).join(&e.quarantine))?;
            let saved = saved.ok_or_else(|| {
                anyhow::anyhow!(
                    "missing pre-image in command {}; kept the store unchanged for recovery",
                    c.meta.id
                )
            })?;
            if let Some(expected) = e.note.before
                && (saved.mode != expected.mode
                    || saved.size != expected.size
                    || saved.mtime != expected.mtime
                    || saved.mtime_ns != expected.mtime_ns)
            {
                bail!(
                    "pre-image changed in command {}; kept the store unchanged so another command's retained copy is not collected; undo newer commands first",
                    c.meta.id
                );
            }
        }
    }
    Ok(())
}
fn collect(home: &Path, dry: bool, age: u64, max: u64) -> Result<Report> {
    let rows = candidates(home)?;
    if rows.iter().any(|c| {
        c.folder.join("undo-in-progress").exists() || crate::hold::pending(home, c.meta.id)
    }) {
        let bytes = store_size(home)?;
        return Ok(Report {
            dry_run: dry,
            before_bytes: bytes,
            after_bytes: bytes,
            collections: vec![],
            deferred: Some(
                "undo in progress; collection deferred to keep shared inode handles".into(),
            ),
        });
    }
    validate_preimages(home, &rows)?;
    let newest = rows.iter().rev().take(3).map(|c| c.meta.id).collect();
    let before = store_size(home)?;
    let now = journal::now();
    let mut collections = Vec::new();
    // Hold every eligible command lock throughout all three passes. Undo uses the same lock.
    let mut locks = Vec::new();
    let mut eligible = Vec::new();
    for c in &rows {
        if protected(home, c, &newest) {
            continue;
        }
        if !dry {
            let f = journal::private_file(&c.folder.join("undo.lock"))?;
            if sys::flock_exclusive(f.as_raw_fd(), true).is_err() {
                continue;
            }
            if protected(home, c, &newest) {
                continue;
            }
            locks.push(f);
        }
        eligible.push(c);
    }
    for c in &eligible {
        if now.saturating_sub(c.meta.start_ns) > DAY {
            let item = collect_content(home, c, dry)?;
            if !item.paths.is_empty() {
                collections.push(item);
            }
        }
    }
    let content_bytes: BTreeMap<_, _> = collections
        .iter()
        .map(|item| (item.id, item.bytes))
        .collect();
    let mut removed = BTreeSet::new();
    for c in &eligible {
        if now.saturating_sub(c.meta.start_ns) > age {
            let mut item = collect_whole(home, c, dry, "age")?;
            if dry {
                item.bytes = item
                    .bytes
                    .saturating_sub(*content_bytes.get(&c.meta.id).unwrap_or(&0));
            }
            collections.push(item);
            removed.insert(c.meta.id);
        }
    }
    let mut after = if dry {
        before.saturating_sub(collections.iter().map(|c| c.bytes).sum())
    } else {
        store_size(home)?
    };
    for c in &eligible {
        if after <= max {
            break;
        }
        if removed.contains(&c.meta.id) {
            continue;
        }
        let mut item = collect_whole(home, c, dry, "size")?;
        if dry {
            item.bytes = item
                .bytes
                .saturating_sub(*content_bytes.get(&c.meta.id).unwrap_or(&0));
        }
        after = if dry {
            after.saturating_sub(item.bytes)
        } else {
            store_size(home)?
        };
        collections.push(item);
    }
    drop(locks);
    Ok(Report {
        dry_run: dry,
        before_bytes: before,
        after_bytes: after,
        collections,
        deferred: None,
    })
}
pub fn run(dry: bool, age: &str, max: &str, json: bool) -> Result<i32> {
    if crate::exec::is_supervised() {
        bail!("run gc in your own terminal");
    }
    let age = quantity(age, true)?;
    let max = quantity(max, false)?;
    let home = paths::home()?;
    let _lock = if dry {
        None
    } else {
        let f = journal::private_file(&home.join("gc.lock"))?;
        sys::flock_exclusive(f.as_raw_fd(), true)?;
        Some(f)
    };
    let report = match collect(&home, dry, age, max) {
        Ok(report) => report,
        Err(e) => {
            if !dry {
                let _ = journal::json_write(&home.join("last-gc-error"), &format!("{e:#}"));
            }
            return Err(e);
        }
    };
    if !dry {
        let _ = fs::remove_file(home.join("last-gc-error"));
    }
    if !dry {
        journal::json_write(&home.join("last-gc"), &journal::now())?;
        journal::json_write(&home.join("gc-attempt"), &journal::now())?;
    }
    let mut out = Progress::new(io::stdout());
    if json {
        out.line(format_args!("{}", serde_json::to_string_pretty(&report)?));
    } else {
        if let Some(reason) = &report.deferred {
            out.line(format_args!("{reason}"));
        }
        for c in &report.collections {
            out.line(format_args!(
                "{}command {}: {} ({} bytes)",
                if dry { "would collect " } else { "collected " },
                c.id,
                c.kind,
                c.bytes
            ));
        }
        out.line(format_args!(
            "store size: {} bytes -> {} bytes{}",
            report.before_bytes,
            report.after_bytes,
            if dry { " (dry run)" } else { "" }
        ));
    }
    Ok(0)
}
pub fn maybe_gc() {
    if crate::exec::is_supervised() {
        return;
    }
    let Ok(home) = paths::home() else {
        return;
    };
    let last = fs::read(home.join("gc-attempt"))
        .ok()
        .and_then(|b| journal::parse_json::<u64>(&home.join("gc-attempt"), &b).ok())
        .unwrap_or(0);
    if journal::now().saturating_sub(last) < DAY {
        return;
    }
    let Ok(lock) = journal::private_file(&home.join("gc.lock")) else {
        return;
    };
    if sys::flock_exclusive(lock.as_raw_fd(), true).is_err() {
        return;
    }
    let last = fs::read(home.join("gc-attempt"))
        .ok()
        .and_then(|b| journal::parse_json::<u64>(&home.join("gc-attempt"), &b).ok())
        .unwrap_or(0);
    if journal::now().saturating_sub(last) < DAY {
        return;
    }
    // Reserve the day before spawning. This is bounded work on the interactive path.
    if journal::json_write(&home.join("gc-attempt"), &journal::now()).is_err() {
        return;
    }
    drop(lock);
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .arg("gc")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}
pub fn status(home: &Path) -> Result<()> {
    let last = journal::read_json_optional::<u64>(&home.join("last-gc"))?
        .map(sys::local_time)
        .transpose()?
        .unwrap_or_else(|| "never".into());
    for id in crate::hold::open_holds(home)? {
        println!("open hold: {id} (saqta show {id})");
    }
    println!("store size: {} bytes; last gc: {last}", store_size(home)?);
    if let Some(error) = journal::read_json_optional::<String>(&home.join("last-gc-error"))? {
        println!("last gc kept retained versions: {error}");
    }
    Ok(())
}
