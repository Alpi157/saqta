#![cfg(debug_assertions)]
mod common;
use common::*;
use std::{ffi::OsStr, fs, path::PathBuf};

fn fixture() -> Sandbox {
    let (s, root) = capture_fixture("json-error-path");
    fs::write(root.join("f"), b"original").unwrap();
    assert_success(&script(&s, &root, "printf changed > f"));
    s
}
fn home(s: &Sandbox) -> PathBuf {
    s.env[OsStr::new("SAQTA_HOME")].clone().into()
}
fn check(s: &Sandbox, args: &[&str], file: &std::path::Path) {
    let result = run(s.saqta(args));
    assert_eq!(
        result.status.code(),
        Some(125),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(&encoded_path(file)),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn inspection_and_undo_json_errors_identify_the_source_file() {
    let cases: &[(&[&str], &str)] = &[
        (&["undo", "1"], "cmd/1/meta"),
        (&["log", "--json"], "cmd/1/meta"),
        (&["show", "1", "--json"], "cmd/1/meta"),
        (&["status"], "cmd/1/meta"),
        (&["gc", "--dry-run"], "cmd/1/meta"),
        (&["show", "1"], "cmd/1/child"),
        (&["show", "1"], "cmd/1/end"),
        (&["show", "1"], "cmd/1/outcomes-positive.json"),
        (&["show", "1"], "cmd/1/expired.json"),
        (&["show", "1"], "cmd/1/tail-recovery.json"),
        (&["undo", "1"], "cmd/1/post.log"),
        (&["undo", "1"], "cmd/1/recovered-post.json"),
        (&["undo", "1"], "undo-posts.json"),
        (&["undo", "1"], "undo-aliases.json"),
        (&["status"], "codex-installs.json"),
        (&["status"], "last-gc"),
        (&["status"], "last-gc-error"),
    ];
    for (args, relative) in cases {
        let s = fixture();
        let file = home(&s).join(relative);
        fs::write(&file, b"not json\n").unwrap();
        check(&s, args, &file);
    }
}

#[test]
fn framed_effect_json_errors_identify_effects_log() {
    let s = fixture();
    let file = home(&s).join("cmd/1/effects.log");
    let text = fs::read_to_string(&file).unwrap();
    let mut fields: Vec<_> = text.trim_end().split('\t').collect();
    fields[8] = "invalid-json";
    fs::write(&file, format!("{}\n", fields.join("\t"))).unwrap();
    check(&s, &["show", "1"], &file);
}

#[test]
fn resume_json_errors_identify_state_json_and_state_log() {
    for name in [
        "state.json",
        "state.log",
        "directory-observations.json",
        "copy-1-1.json",
    ] {
        let s = fixture();
        let mut command = s.saqta(&["undo", "1"]);
        command.env("SAQTA_TEST_ABORT_AFTER", "1");
        assert_eq!(run(command).status.code(), Some(86));
        let backup = fs::read_dir(home(&s).join("q/main"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b"undo-1-")
            })
            .unwrap();
        let file = backup.join(name);
        fs::write(&file, b"not-json\n").unwrap();
        check(&s, &["undo", "1", "--resume"], &file);
    }
}

#[test]
fn release_json_errors_identify_the_hold_plan() {
    let s = fixture();
    let config = PathBuf::from(&s.env[OsStr::new("HOME")]).join(".config/saqta");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        b"release_without_terminal = true\n",
    )
    .unwrap();
    let hold = home(&s).join("holds/1");
    fs::create_dir_all(&hold).unwrap();
    fs::write(hold.join("ready"), b"0").unwrap();
    let file = hold.join("plan");
    fs::write(&file, b"not-json\n").unwrap();
    check(&s, &["release", "1"], &file);
}
