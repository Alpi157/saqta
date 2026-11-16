//! Project-local Codex installation and inspection.
use crate::{hook, journal, output::Progress, paths};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Serialize, Deserialize)]
struct Change {
    path: String,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct Install {
    project: String,
    changes: Vec<Change>,
    command: String,
}
fn installs(home: &Path) -> Result<Vec<Install>> {
    match fs::read(home.join("codex-installs.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}
fn save(home: &Path, rows: &[Install]) -> Result<()> {
    journal::json_write(&home.join("codex-installs.json"), &rows)?;
    let mut projects = Vec::new();
    for row in rows {
        projects.extend(journal::decode(&row.project)?);
        projects.push(b'\n');
    }
    let temporary = home.join(format!("projects.tmp-{}", std::process::id()));
    let mut file = journal::private_file(&temporary)?;
    use std::io::Write;
    file.set_len(0)?;
    file.write_all(&projects)?;
    fs::rename(temporary, home.join("projects"))?;
    Ok(())
}
fn git(project: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let o = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()?;
    if !o.status.success() {
        bail!("PROJECT must be inside a git repository");
    }
    Ok(o.stdout.strip_suffix(b"\n").unwrap_or(&o.stdout).to_vec())
}
fn root(project: Option<&PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let dir = project.map_or(cwd.as_path(), PathBuf::as_path);
    let p = PathBuf::from(std::ffi::OsString::from_vec(git(
        dir,
        &["rev-parse", "--show-toplevel"],
    )?));
    Ok(fs::canonicalize(p)?)
}
fn command() -> Result<String> {
    Ok(format!(
        "{} hook codex",
        String::from_utf8(hook::quote(std::env::current_exe()?.as_os_str().as_bytes()))?
    ))
}
fn entries(cmd: &str) -> Vec<(&'static str, Value)> {
    [("PreToolUse", "^Bash$"), ("PreToolUse", "^apply_patch$"), ("PostToolUse", "^apply_patch$")].into_iter().map(|(event, matcher)| (event, json!({"matcher":matcher,"hooks":[{"type":"command","command":cmd,"statusMessage":"saqta"}]}))).collect()
}
fn hooks_edit(bytes: &[u8], cmd: &str, remove: bool) -> Result<Vec<u8>> {
    let mut v: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(bytes)?
    };
    let obj = v.as_object_mut().context("hooks.json must be an object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert(json!({}))
        .as_object_mut()
        .context("hooks must be an object")?;
    for (event, entry) in entries(cmd) {
        let array = hooks
            .entry(event)
            .or_insert(json!([]))
            .as_array_mut()
            .context("event hooks must be arrays")?;
        if remove {
            array.retain(|e| e != &entry);
        } else if !array.contains(&entry) {
            array.push(entry);
        }
    }
    let mut out = serde_json::to_vec_pretty(&v)?;
    out.push(b'\n');
    Ok(out)
}
fn config_edit(bytes: &[u8], home: &Path, remove: bool) -> Result<Vec<u8>> {
    use toml_edit::{DocumentMut, Item, Table, value};
    let mut doc = std::str::from_utf8(bytes)?.parse::<DocumentMut>()?;
    let home = home
        .to_str()
        .context("Codex writable_roots needs a UTF-8 SAQTA_HOME")?;
    if doc.get("sandbox_workspace_write").is_none() {
        doc["sandbox_workspace_write"] = Item::Table(Table::new());
    }
    let table = doc
        .get_mut("sandbox_workspace_write")
        .and_then(Item::as_table_like_mut)
        .context("sandbox_workspace_write must be a table")?;
    if table.get("writable_roots").is_none() {
        table.insert("writable_roots", value(toml_edit::Array::new()));
    }
    let array = table
        .get_mut("writable_roots")
        .and_then(Item::as_array_mut)
        .context("writable_roots must be an array")?;
    if remove {
        for i in (0..array.len()).rev() {
            if array.get(i).and_then(|v| v.as_str()) == Some(home) {
                array.remove(i);
            }
        }
    } else if !array.iter().any(|v| v.as_str() == Some(home)) {
        array.push(home);
    }
    Ok(doc.to_string().into_bytes())
}
fn change(
    path: PathBuf,
    after: Vec<u8>,
    changes: &mut Vec<Change>,
    out: &mut Progress<std::io::Stdout>,
) -> Result<()> {
    let before = match fs::read(&path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if before.as_ref() == Some(&after) {
        return Ok(());
    }
    if let Some(ref b) = before {
        let backup = path.with_file_name(format!(
            "{}.saqta-backup",
            path.file_name().context("filename")?.to_string_lossy()
        ));
        // Preserve the first backup across repeated installs.
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut f) => {
                f.write_all(b)?;
                out.line(format_args!("Backed up {}", journal::display(&backup)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let row = Change {
        path: journal::display(&path),
        before,
        after,
    };
    changes.push(row);
    Ok(())
}
pub fn configure(project: Option<&PathBuf>, remove: bool) -> Result<i32> {
    let project = root(project)?;
    let home = paths::home()?;
    use std::os::fd::AsRawFd;
    let lock = journal::private_file(&home.join("codex-install.lock"))?;
    crate::sys::flock_exclusive(lock.as_raw_fd(), false)?;
    let mut rows = installs(&home)?;
    let key = journal::display(&project);
    let mut out = Progress::new(std::io::stdout());
    if remove {
        if let Some(index) = rows.iter().position(|r| r.project == key) {
            let row = &rows[index];
            let mut restored = Vec::new();
            for c in &row.changes {
                let p = journal::path(&c.path)?;
                let current = fs::read(&p)?;
                let bytes = if current == c.after {
                    c.before.clone()
                } else {
                    Some(remove_owned(c, &current, &row.command, &home)?)
                };
                restored.push((p, bytes));
            }
            for (p, bytes) in restored {
                if let Some(b) = bytes {
                    fs::write(&p, b)?;
                } else {
                    fs::remove_file(&p)?;
                }
                out.line(format_args!(
                    "Removed Saqta changes from {}",
                    journal::display(&p)
                ));
            }
            rows.remove(index);
            save(&home, &rows)?;
            out.line(format_args!("Removed project {}", key));
        } else {
            out.line(format_args!("No Saqta installation in {}", key));
        }
        return Ok(0);
    }
    if let Ok(cargo) = fs::read_to_string(project.join("Cargo.toml")) {
        let doc = cargo.parse::<toml_edit::DocumentMut>()?;
        if doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml_edit::Item::as_str)
            == Some("saqta")
        {
            bail!(
                "cannot init the Saqta repository: its test suite runs its own supervisors, and the kernel allows one per process chain"
            );
        }
    }
    if rows.iter().any(|r| r.project == key) {
        out.line(format_args!("Already configured {}", key));
        next_steps(&mut out);
        return Ok(0);
    }
    let cmd = command()?;
    let codex = project.join(".codex");
    for p in [
        &codex,
        &codex.join("hooks.json"),
        &codex.join("config.toml"),
    ] {
        if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("refusing symlinked Codex settings: {}", journal::display(p));
        }
    }
    fs::create_dir_all(&codex)?;
    let hooks = codex.join("hooks.json");
    let config = codex.join("config.toml");
    let read = |p: &Path| -> Result<Vec<u8>> {
        match fs::read(p) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    };
    let mut changes = Vec::new();
    change(
        hooks.clone(),
        hooks_edit(&read(&hooks)?, &cmd, false)?,
        &mut changes,
        &mut out,
    )?;
    change(
        config.clone(),
        config_edit(&read(&config)?, &home, false)?,
        &mut changes,
        &mut out,
    )?;
    let exclude = PathBuf::from(std::ffi::OsString::from_vec(git(
        &project,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    )?));
    let mut bytes = read(&exclude)?;
    for name in [".codex/hooks.json", ".codex/config.toml"] {
        if !git(&project, &["ls-files", "--", name])?.is_empty() {
            out.line(format_args!("Warning: {name} is tracked; not excluded"));
            continue;
        }
        if !bytes
            .split(|b| *b == b'\n')
            .any(|line| line == name.as_bytes())
        {
            if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'\n');
        }
    }
    change(exclude, bytes, &mut changes, &mut out)?;
    // Persist ownership before changing settings so partial failures remain reviewable.
    rows.push(Install {
        project: key.clone(),
        changes,
        command: cmd,
    });
    save(&home, &rows)?;
    if let Some(row) = rows.last() {
        for c in &row.changes {
            fs::write(journal::path(&c.path)?, &c.after)?;
            out.line(format_args!("Updated {}", c.path));
        }
    }
    out.line(format_args!("Recorded project {}", key));
    next_steps(&mut out);
    Ok(0)
}
fn next_steps(out: &mut Progress<std::io::Stdout>) {
    out.line(format_args!("Open Codex in this project, type /hooks, and trust the three saqta hooks. Then run `saqta doctor` through Codex to check that protection works inside the sandbox."));
}
pub fn status() -> Result<i32> {
    let home = paths::home()?;
    let cmd = command()?;
    for row in installs(&home)? {
        let project = journal::path(&row.project)?;
        let v: Value = fs::read(project.join(".codex/hooks.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let present = entries(&cmd)
            .iter()
            .filter(|(event, e)| v["hooks"][event].as_array().is_some_and(|a| a.contains(e)))
            .count();
        for (event, expected) in entries(&cmd) {
            let matching = v["hooks"][event]
                .as_array()
                .and_then(|a| a.iter().find(|e| e["matcher"] == expected["matcher"]));
            println!(
                "  {event} {}: present {}; current binary {}",
                expected["matcher"],
                matching.is_some(),
                v["hooks"][event]
                    .as_array()
                    .is_some_and(|a| a.contains(&expected))
            );
        }
        let config = fs::read(project.join(".codex/config.toml")).unwrap_or_default();
        let writable = config_edit(&config, &home, false).is_ok_and(|b| b == config);
        println!(
            "{}: hooks {present}/3 point at current binary; writable root {}",
            row.project,
            if writable { "set" } else { "missing" }
        );
    }
    journal::log(5, false)
}

// Undo the exact changed span when surrounding bytes have been edited. Refuse an
// ambiguous match rather than discard newer settings. JSON removal is structural.
fn remove_owned(c: &Change, current: &[u8], cmd: &str, home: &Path) -> Result<Vec<u8>> {
    let old = c.before.as_deref().unwrap_or_default();
    let prefix = old.iter().zip(&c.after).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(c.after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let added = &c.after[prefix..c.after.len() - suffix];
    if !added.is_empty() {
        let matches: Vec<_> = current
            .windows(added.len())
            .enumerate()
            .filter(|(_, b)| *b == added)
            .map(|(i, _)| i)
            .collect();
        if matches.len() == 1 {
            let i = matches[0];
            let mut out = current[..i].to_vec();
            out.extend_from_slice(&old[prefix..old.len() - suffix]);
            out.extend_from_slice(&current[i + added.len()..]);
            return Ok(out);
        }
    }
    if c.path.ends_with("hooks.json") {
        return remove_hooks(current, cmd, old);
    }
    if c.path.ends_with("config.toml") {
        // toml_edit retains user comments and formatting for the other values.
        return config_edit(current, home, true);
    }
    bail!(
        "{} changed ambiguously; retained all versions; review before uninstall",
        c.path
    )
}
fn json_end(bytes: &[u8], start: usize) -> Result<usize> {
    let mut stream =
        serde_json::Deserializer::from_slice(&bytes[start..]).into_iter::<serde::de::IgnoredAny>();
    stream.next().context("missing JSON value")??;
    Ok(start + stream.byte_offset())
}
fn skip(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}
fn members(bytes: &[u8], start: usize) -> Result<Vec<(String, usize, usize)>> {
    let mut i = skip(bytes, start + 1);
    let mut result = Vec::new();
    while bytes.get(i) != Some(&b'}') {
        let end = json_end(bytes, i)?;
        let key: String = serde_json::from_slice(&bytes[i..end])?;
        i = skip(bytes, end);
        if bytes.get(i) != Some(&b':') {
            bail!("invalid JSON object");
        }
        i = skip(bytes, i + 1);
        let end = json_end(bytes, i)?;
        result.push((key, i, end));
        i = skip(bytes, end);
        if bytes.get(i) == Some(&b',') {
            i = skip(bytes, i + 1);
        } else {
            break;
        }
    }
    Ok(result)
}
fn remove_hooks(bytes: &[u8], cmd: &str, original: &[u8]) -> Result<Vec<u8>> {
    let _: Value = serde_json::from_slice(bytes)?;
    let before: Value = if original.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(original)?
    };
    let mut out = bytes.to_vec();
    loop {
        let bytes = &out;
        let hooks = members(bytes, skip(bytes, 0))?
            .into_iter()
            .find(|(k, _, _)| k == "hooks")
            .context("missing hooks")?
            .1;
        let mut remove = None;
        'events: for (event, start, _) in members(bytes, hooks)? {
            if !matches!(event.as_str(), "PreToolUse" | "PostToolUse") {
                continue;
            }
            let mut i = skip(bytes, start + 1);
            let mut previous = None;
            while bytes.get(i) != Some(&b']') {
                let end = json_end(bytes, i)?;
                let v: Value = serde_json::from_slice(&bytes[i..end])?;
                let next = skip(bytes, end);
                if entries(cmd).iter().any(|(e, entry)| {
                    *e == event
                        && *entry == v
                        && !before["hooks"][&event]
                            .as_array()
                            .is_some_and(|a| a.contains(entry))
                }) {
                    remove = Some(if bytes.get(next) == Some(&b',') {
                        (i, next + 1)
                    } else {
                        (previous.unwrap_or(i), end)
                    });
                    break 'events;
                }
                if bytes.get(next) == Some(&b',') {
                    previous = Some(next);
                    i = skip(bytes, next + 1);
                } else {
                    break;
                }
            }
        }
        if let Some((a, b)) = remove {
            out.drain(a..b);
        } else {
            break;
        }
    }
    let _: Value = serde_json::from_slice(&out)?;
    Ok(out)
}
