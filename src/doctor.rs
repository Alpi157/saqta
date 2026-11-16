use crate::{filter, paths, sys};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
};
pub fn run(json_output: bool) -> Result<i32> {
    let mut checks = Vec::<Value>::new();
    let mut add = |name: &str, status: &str, advice: String| {
        checks.push(json!({"name":name,"status":status,"advice":advice}));
    };
    let release = fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let mut parts = release
        .trim()
        .split('.')
        .filter_map(|p| p.parse::<u32>().ok());
    let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    add(
        "kernel",
        if version < (5, 6) {
            "fail"
        } else if version < (6, 6) {
            "warn"
        } else {
            "ok"
        },
        format!(
            "{}{}",
            release.trim(),
            if version < (5, 6) {
                ": use Linux 5.6 or newer"
            } else if version < (6, 6) {
                ": interception is slower without synchronous wake-up"
            } else {
                ": supported"
            }
        ),
    );
    add(
        "architecture",
        if cfg!(target_arch = "x86_64") {
            "ok"
        } else {
            "fail"
        },
        format!("{}; v0.1 requires x86_64", std::env::consts::ARCH),
    );
    match sys::probe(&filter::program()) {
        Ok(sync) => {
            add(
                "functional probe",
                "ok",
                "listener and CONTINUE worked".into(),
            );
            add(
                "synchronous wake-up",
                if sync { "ok" } else { "warn" },
                if sync {
                    "accepted"
                } else {
                    "interception is slower without synchronous wake-up"
                }
                .into(),
            );
        }
        Err(e) => {
            add(
                "functional probe",
                "fail",
                if e.raw_os_error() == Some(libc::EBUSY) {
                    "already running under a seccomp supervisor".into()
                } else {
                    format!("cannot install and service a listener: {e}")
                },
            );
            add(
                "synchronous wake-up",
                "warn",
                "unavailable because listener probe failed".into(),
            );
        }
    }
    let yama =
        fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_else(|_| "0".into());
    add(
        "Yama ptrace_scope",
        match yama.trim() {
            "0" | "1" => "ok",
            "3" => "fail",
            _ => "warn",
        },
        format!(
            "{}: {}",
            yama.trim(),
            match yama.trim() {
                "3" => "Saqta can't read command arguments",
                "2" => "argument reads may require privileges",
                _ => "argument reads available",
            }
        ),
    );
    let cwd = std::env::current_dir()?;
    if fs::read_to_string("/proc/version")
        .unwrap_or_default()
        .to_lowercase()
        .contains("microsoft")
    {
        add(
            "WSL",
            if cwd.starts_with("/mnt") {
                "warn"
            } else {
                "ok"
            },
            if cwd.starts_with("/mnt") {
                "keep projects in the Linux filesystem (~/...), not under /mnt/c"
            } else {
                "project is in the Linux filesystem"
            }
            .into(),
        );
    }
    match paths::home() {
        Ok(home) => {
            let (kind, free) = sys::filesystem(&CString::new(home.as_os_str().as_bytes())?)?;
            let metadata = fs::metadata(&home)?;
            let same = metadata.dev() == fs::metadata(&cwd)?.dev();
            let shared = metadata.permissions().mode() & 0o066 != 0;
            let writable = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(home.join(format!(".doctor-{}", std::process::id())));
            let can_write = writable.is_ok();
            if can_write {
                drop(writable);
                fs::remove_file(home.join(format!(".doctor-{}", std::process::id())))?;
            }
            add(
                "SAQTA_HOME",
                if !can_write || !same || shared {
                    "warn"
                } else {
                    "ok"
                },
                format!(
                    "{}; filesystem {}; {:.2} GiB free; same filesystem: {same}{}{}{}",
                    home.display(),
                    filesystem_name(kind),
                    free as f64 / (1u64 << 30) as f64,
                    if shared {
                        "; readable or writable by group or others; restrict access to this directory"
                    } else {
                        ""
                    },
                    if same {
                        ""
                    } else {
                        "; deletes there will be copied instead of hard-linked"
                    },
                    if can_write {
                        ""
                    } else {
                        "; cannot write run files"
                    }
                ),
            );
        }
        Err(e) => add(
            "SAQTA_HOME",
            "warn",
            format!("cannot create home: {e}; commands can still run"),
        ),
    }
    let failed = checks.iter().any(|c| c["status"] == "fail");
    let note = "sudo and other setuid programs don't work inside protected commands (no_new_privs)";
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"checks":checks,"note":note}))?
        );
    } else {
        for c in checks {
            println!(
                "{}: {}: {}",
                c["status"].as_str().unwrap_or("fail"),
                c["name"].as_str().unwrap_or("check"),
                c["advice"].as_str().unwrap_or("")
            );
        }
        println!("note: {note}");
    }
    Ok(if failed { 1 } else { 0 })
}

// Linux linux/magic.h; libc does not expose the 9p magic.
const V9FS_MAGIC: i64 = 0x01021997;

fn filesystem_name(kind: i64) -> String {
    match kind {
        libc::EXT4_SUPER_MAGIC => "ext4".into(),
        libc::BTRFS_SUPER_MAGIC => "btrfs".into(),
        libc::XFS_SUPER_MAGIC => "xfs".into(),
        libc::TMPFS_MAGIC => "tmpfs".into(),
        V9FS_MAGIC => "9p".into(),
        libc::OVERLAYFS_SUPER_MAGIC => "overlayfs".into(),
        _ => format!("0x{kind:x}"),
    }
}
#[cfg(test)]
mod tests {
    use super::{V9FS_MAGIC, filesystem_name};
    #[test]
    fn filesystem_names_and_unknown_magic() {
        for (magic, name) in [
            (libc::EXT4_SUPER_MAGIC, "ext4"),
            (libc::BTRFS_SUPER_MAGIC, "btrfs"),
            (libc::XFS_SUPER_MAGIC, "xfs"),
            (libc::TMPFS_MAGIC, "tmpfs"),
            (V9FS_MAGIC, "9p"),
            (libc::OVERLAYFS_SUPER_MAGIC, "overlayfs"),
        ] {
            assert_eq!(filesystem_name(magic), name);
        }
        assert_eq!(filesystem_name(0x1234), "0x1234");
    }
}
