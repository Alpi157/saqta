//! Codex hooks are deliberately silent and fail open.
use crate::{
    capture::Capture,
    journal::{self, Journal},
    paths, sys,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

pub fn quote(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![b'\''];
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}
pub fn rewrite(binary: &Path, shell: &Path, command: &str) -> Result<String> {
    let mut out = quote(binary.as_os_str().as_bytes());
    out.extend_from_slice(b" exec --shell ");
    out.extend(quote(shell.as_os_str().as_bytes()));
    out.extend_from_slice(b" -c ");
    out.extend(quote(command.as_bytes()));
    Ok(String::from_utf8(out)?)
}
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            _ => out.push(c),
        }
    }
    out
}
pub fn log(reason: &str) {
    if let Ok(home) = paths::home() {
        let _ = journal::private_dir(&home.join("logs"));
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.join("logs/hook.log"))
        {
            let _ = writeln!(f, "{} {reason}", journal::now());
        }
    }
}
pub fn run() {
    if std::env::var_os("SAQTA_OFF").is_some_and(|v| v == "1") {
        return;
    }
    if let Err(e) = handle() {
        log(&format!("{e:#}"));
    }
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing {key}"))
}
fn handle() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024)
        .read_to_end(&mut input)?;
    let v: Value = serde_json::from_slice(&input)?;
    match (field(&v, "tool_name")?, field(&v, "hook_event_name")?) {
        ("Bash", "PreToolUse") => {
            let command = field(&v["tool_input"], "command")?;
            if command.contains('\0') {
                bail!("NUL in command");
            }
            let binary = std::env::current_exe()?;
            let prefix = rewrite(&binary, &sys::login_shell(), "")?;
            // Only recognize our exact invocation prefix, never a substring in user data.
            let prefix = prefix.strip_suffix("''").unwrap_or(&prefix);
            let plain_prefix = binary.to_str().map(|p| format!("{p} exec "));
            if command.starts_with(prefix)
                || plain_prefix
                    .as_ref()
                    .is_some_and(|p| command.starts_with(p))
            {
                return Ok(());
            }
            let updated = rewrite(&binary, &sys::login_shell(), command)?;
            let output = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":updated}}});
            std::io::stdout().write_all(serde_json::to_string(&output)?.as_bytes())?;
        }
        ("apply_patch", "PreToolUse") => {
            let cwd = Path::new(field(&v, "cwd")?);
            if !cwd.is_absolute() {
                bail!("relative cwd");
            }
            let patch = field(&v["tool_input"], "command")?;
            if !patch.starts_with("*** Begin Patch\n")
                || !patch.trim_end().ends_with("*** End Patch")
            {
                bail!("invalid patch envelope");
            }
            let mut names = std::collections::BTreeMap::new();
            for line in patch.lines() {
                for prefix in [
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ] {
                    if let Some(p) = line.strip_prefix(prefix) {
                        if p.is_empty() || p.contains('\0') {
                            bail!("invalid patch path");
                        }
                        names.insert(cwd.join(p), prefix == "*** Update File: ");
                    }
                }
            }
            if names.is_empty() {
                bail!("patch has no paths");
            }
            let reference = json!({"session_id":field(&v,"session_id")?,"tool_use_id":field(&v,"tool_use_id")?});
            let home = paths::home()?;
            let j = Journal::start(home, &["apply_patch".into()], cwd, -1)?;
            fs::write(
                j.folder.join("hook-reference"),
                serde_json::to_vec(&reference)?,
            )?;
            let mut capture = Capture::new(j)?;
            for (p, update) in names {
                capture.patch_path(&p, update)?;
            }
        }
        ("apply_patch", "PostToolUse") => {
            let reference = json!({"session_id":field(&v,"session_id")?,"tool_use_id":field(&v,"tool_use_id")?});
            let home = paths::home()?;
            for entry in fs::read_dir(home.join("cmd"))? {
                let folder = entry?.path();
                if folder.join("end").exists() {
                    continue;
                }
                if let Ok(bytes) = fs::read(folder.join("hook-reference")) {
                    let stored: Value = serde_json::from_slice(&bytes)?;
                    if stored == reference {
                        let id = folder
                            .file_name()
                            .and_then(|p| p.to_str())
                            .ok_or_else(|| anyhow::anyhow!("invalid command id"))?
                            .parse()?;
                        Journal::reopen(home.clone(), id)?.finish(0)?;
                        return Ok(());
                    }
                }
            }
            bail!("no matching patch PreToolUse");
        }
        _ => bail!("unhandled tool or hook event"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::PermissionsExt},
        process::Command,
    };

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn quoting_matches_bash(bytes in prop::collection::vec(1u8..=255, 0..512), variant in 0usize..4) {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/saqta-tests").join(format!("quoting-{}-{}", std::process::id(), journal::now()));
            fs::create_dir_all(&dir).unwrap();
            let fake = dir.join("fake saqta ' binary");
            fs::write(&fake, b"#!/bin/sh\nshift\nshift\nshell=$1\nshift\nexec \"$shell\" \"$@\"\n").unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
            let mut original = b"printf '%s' ".to_vec(); original.extend(quote(&bytes));
            original.extend_from_slice(match variant {
                0 => b"; printf 'stderr\\n' >&2; exit 9",
                1 => b"\ncat <<'EOF_SAQTA'\n$hello `literal` \\ ' \" & ; unicode: \xce\xbb\nEOF_SAQTA\n",
                2 => b"; x='a$b'; printf '%s' \"$x\"; true & wait",
                _ => b"; printf '%s' 'quote '\\'' backslash \\'; exit 0",
            });
            let mut wrapped = quote(fake.as_os_str().as_bytes());
            wrapped.extend_from_slice(b" exec --shell /bin/bash -c "); wrapped.extend(quote(&original));
            let run = |script| {
                let mut c = Command::new("/bin/bash");
                c.args(["-lc"]).arg(OsString::from_vec(script)).current_dir(&dir);
                for key in ["HOME","XDG_DATA_HOME","XDG_CONFIG_HOME","XDG_STATE_HOME","SAQTA_HOME"] { c.env(key,dir.join(key)); }
                c.output().unwrap()
            };
            let a=run(original); let b=run(wrapped);
            fs::remove_dir_all(&dir).unwrap();
            prop_assert_eq!((a.stdout,a.stderr,a.status.code()),(b.stdout,b.stderr,b.status.code()));
        }
    }
}
