mod common;
use common::*;
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn sample(p: &Path) {
    fs::create_dir_all(p.join("tree/nested/empty")).unwrap();
    fs::create_dir(p.join("tree/empty")).unwrap();
    fs::write(p.join("tree/nested/file"), b"original\0bytes\xff\n").unwrap();
}
fn restore_exact(s: &Sandbox, p: &Path, before: &std::collections::BTreeMap<Vec<u8>, u64>) {
    let id = last_id(s);
    let shown = shown(s, id);
    assert!(
        !shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["op"] == "not protected"),
        "{shown}"
    );
    assert_success(&undo(s, id));
    assert_tree(before, p);
}

#[test]
fn rm_tree_restores_all_byte_paths_and_file_types() {
    let (s, p) = capture_fixture("rm_tree");
    sample(&p);
    for name in [
        OsString::from("space here"),
        OsString::from("line\nbreak"),
        OsString::from_vec(b"non-utf8-\xff".to_vec()),
    ] {
        fs::write(p.join("tree").join(name), b"special").unwrap();
    }
    fs::hard_link(p.join("tree/nested/file"), p.join("tree/hardlink")).unwrap();
    symlink("nested/file", p.join("tree/file-link")).unwrap();
    symlink("nested", p.join("tree/dir-link")).unwrap();
    symlink("missing", p.join("tree/dangling")).unwrap();
    fs::set_permissions(
        p.join("tree/nested/file"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm -rf tree"));
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn dirty_git_checkout_and_clean_restore_entire_repository() {
    let (s, p) = capture_fixture("dirty_git");
    let git = |args: &[&str]| {
        let mut c = s.command("git");
        c.current_dir(&p).arg("--no-optional-locks").args(args);
        run(c)
    };
    if s.command("git").arg("--version").output().is_err() {
        note("SKIP: git is missing");
        return;
    }
    assert_success(&git(&["init", "-q"]));
    assert_success(&git(&["config", "user.email", "fixture@example.invalid"]));
    assert_success(&git(&["config", "user.name", "Fixture"]));
    fs::write(p.join("unstaged"), "committed\n").unwrap();
    fs::write(p.join("staged"), "committed\n").unwrap();
    fs::write(p.join(".gitignore"), ".env\nlocal.db\n").unwrap();
    assert_success(&git(&["add", "."]));
    assert_success(&git(&["commit", "-qm", "fixture"]));
    fs::write(p.join("unstaged"), "unsaved\n").unwrap();
    fs::write(p.join("staged"), "index version\n").unwrap();
    assert_success(&git(&["add", "staged"]));
    fs::write(p.join("staged"), "working version\n").unwrap();
    fs::write(p.join("untracked"), "untracked\n").unwrap();
    fs::write(p.join(".env"), "SECRET=decoy\n").unwrap();
    fs::write(p.join("local.db"), vec![0x5a; 5 * 1024 * 1024]).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "git --no-optional-locks checkout -- . && git --no-optional-locks clean -fdx",
    ));
    assert!(!p.join(".env").exists());
    assert_eq!(fs::read(p.join("unstaged")).unwrap(), b"committed\n");
    restore_exact(&s, &p, &before);
}

// The recovery map is the public way to locate displaced versions. Check bytes
// there rather than accepting a success message as proof that both survived.
fn displaced_for(s: &Sandbox, id: u64, path: &Path) -> Vec<PathBuf> {
    let q = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]).join("q/main");
    let mut kept = Vec::new();
    for entry in fs::read_dir(q).unwrap() {
        let entry = entry.unwrap();
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("undo-{id}-"))
        {
            continue;
        }
        let folder = entry.path();
        let map = fs::read_to_string(folder.join("recovery.log")).unwrap();
        for line in map.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            // These fixtures use ASCII paths without percent escapes.
            if fields.len() == 3
                && fields[1].as_bytes() == bytes(path)
                && fields[0].parse::<u64>().is_ok()
            {
                kept.push(folder.join(fields[0]));
            }
        }
    }
    kept
}

#[test]
fn git_status_after_command_does_not_block_undo() {
    use std::os::unix::fs::MetadataExt;
    let (s, p) = capture_fixture("git_status_undo");
    if s.command("git").arg("--version").output().is_err() {
        note("SKIP: git is missing");
        return;
    }
    let git = |args: &[&str]| {
        let mut c = s.command("git");
        c.current_dir(&p).arg("--no-optional-locks").args(args);
        run(c)
    };
    assert_success(&git(&["init", "-q"]));
    assert_success(&git(&["config", "user.email", "fixture@example.invalid"]));
    assert_success(&git(&["config", "user.name", "Fixture"]));
    fs::write(p.join("tracked"), b"committed\n").unwrap();
    fs::write(p.join("staged"), b"committed\n").unwrap();
    fs::write(p.join(".gitignore"), b".env\n").unwrap();
    assert_success(&git(&["add", "."]));
    assert_success(&git(&["commit", "-qm", "fixture"]));
    fs::write(p.join("tracked"), b"dirty working bytes\0\xff").unwrap();
    fs::write(p.join("staged"), b"staged bytes\n").unwrap();
    assert_success(&git(&["add", "staged"]));
    fs::write(p.join("staged"), b"unstaged bytes\n").unwrap();
    fs::write(p.join("untracked"), b"untracked\n").unwrap();
    fs::write(p.join(".env"), b"decoy secret\n").unwrap();
    let before = tree_hash(&p);
    let original_index = fs::read(p.join(".git/index")).unwrap();
    assert_success(&script(
        &s,
        &p,
        "git --no-optional-locks checkout -- . && git --no-optional-locks clean -fdx",
    ));
    let index_inode = fs::metadata(p.join(".git/index")).unwrap().ino();
    // This deliberately enables index.lock only in this isolated fixture to
    // reproduce a user's routine Git refresh outside Saqta.
    let mut status = s.command("git");
    status
        .current_dir(&p)
        .env("GIT_OPTIONAL_LOCKS", "1")
        .args(["status", "--porcelain"]);
    assert_success(&run(status));
    assert_ne!(
        fs::metadata(p.join(".git/index")).unwrap().ino(),
        index_inode,
        "git status must actually rewrite the index for this regression"
    );
    let refreshed = fs::read(p.join(".git/index")).unwrap();
    let id = last_id(&s);
    let result = undo(&s, id);
    assert_success(&result);
    let output = String::from_utf8_lossy(&result.stdout);
    assert_eq!(output.matches("kept the newer git index in ").count(), 1);
    assert!(output.contains("(git rewrites it on commands like git status)"));
    assert_tree(&before, &p);
    assert_eq!(fs::read(p.join(".git/index")).unwrap(), original_index);
    assert!(
        displaced_for(&s, id, &p.join(".git/index"))
            .iter()
            .any(|path| fs::read(path).unwrap() == refreshed)
    );
}

#[test]
fn user_edit_after_command_refuses_then_force_keeps_both() {
    let (s, p) = capture_fixture("force_keeps_both");
    let path = p.join("file");
    fs::write(&path, b"original\0\xff").unwrap();
    fs::write(p.join("other"), b"also restore").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo command > file; rm other"));
    let id = last_id(&s);
    let newer = b"new user work\0\xfe\n";
    fs::write(&path, newer).unwrap();
    let changed = tree_hash(&p);
    let result = undo(&s, id);
    assert!(!result.status.success());
    let message = String::from_utf8_lossy(&result.stderr);
    assert!(message.contains(&path.display().to_string()));
    assert!(message.contains("refusing to overwrite newer work"));
    assert!(message.contains(&format!("Run `saqta undo {id} --force` to restore anyway.")));
    assert!(message.contains("Every newer version is moved to "));
    assert!(message.trim_end().ends_with("first, so nothing is lost."));
    let suggested_folder = message
        .split("Every newer version is moved to ")
        .nth(1)
        .unwrap()
        .split(" first,")
        .next()
        .unwrap();
    assert_tree(&changed, &p);
    assert!(displaced_for(&s, id, &path).is_empty());
    let result = run(s.saqta(&["undo", &id.to_string(), "--force"]));
    assert_success(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("displaced versions kept in "));
    assert!(String::from_utf8_lossy(&result.stdout).contains(suggested_folder));
    assert_tree(&before, &p);
    assert!(
        displaced_for(&s, id, &path)
            .iter()
            .any(|path| fs::read(path).unwrap() == newer)
    );
}

#[test]
fn worktree_git_index_conflict_keeps_newer_version() {
    let (s, p) = capture_fixture("worktree_index");
    fs::create_dir_all(p.join(".git/worktrees/topic")).unwrap();
    let index = p.join(".git/worktrees/topic/index");
    fs::write(&index, b"original index").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm .git/worktrees/topic/index"));
    let id = last_id(&s);
    fs::write(&index, b"newer index").unwrap();
    let result = undo(&s, id);
    assert_success(&result);
    assert_tree(&before, &p);
    assert!(
        displaced_for(&s, id, &index)
            .iter()
            .any(|p| fs::read(p).unwrap() == b"newer index")
    );
    assert_eq!(
        String::from_utf8_lossy(&result.stdout)
            .matches("kept the newer git index in ")
            .count(),
        1
    );
}

#[test]
fn force_keeps_recreated_directory_and_its_new_children() {
    let (s, p) = capture_fixture("force_directory");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm -rf tree"));
    let id = last_id(&s);
    fs::create_dir(p.join("tree")).unwrap();
    fs::write(p.join("tree/new"), b"new user work").unwrap();
    symlink("../outside", p.join("tree/link")).unwrap();
    let result = run(s.saqta(&["undo", &id.to_string(), "--force"]));
    assert_success(&result);
    assert_tree(&before, &p);
    let saved = displaced_for(&s, id, &p.join("tree"));
    assert_eq!(saved.len(), 1);
    assert_eq!(fs::read(saved[0].join("new")).unwrap(), b"new user work");
    assert_eq!(
        fs::read_link(saved[0].join("link")).unwrap(),
        Path::new("../outside")
    );
}

#[test]
fn force_keeps_edited_rename_input_before_moving_it_back() {
    let (s, p) = capture_fixture("force_rename");
    sample(&p);
    assert_success(&script(&s, &p, "mv tree moved"));
    let id = last_id(&s);
    // Adding an immediate child changes the moved directory's recorded state.
    fs::write(p.join("moved/new"), b"new user work").unwrap();
    let result = run(s.saqta(&["undo", &id.to_string(), "--force"]));
    assert_success(&result);
    assert!(!p.join("moved").exists());
    assert_eq!(fs::read(p.join("tree/new")).unwrap(), b"new user work");
    let saved = displaced_for(&s, id, &p.join("moved"));
    assert_eq!(saved.len(), 1);
    assert_eq!(fs::read(saved[0].join("new")).unwrap(), b"new user work");
    assert_eq!(
        fs::read(saved[0].join("nested/file")).unwrap(),
        b"original\0bytes\xff\n"
    );
}

#[test]
fn dry_run_reports_conflicts_without_changes() {
    let (s, p) = capture_fixture("dry_run_conflicts");
    for i in 0..12 {
        fs::write(p.join(format!("file{i:02}")), b"original").unwrap();
    }
    assert_success(&script(&s, &p, "rm file*"));
    let id = last_id(&s);
    for i in 0..12 {
        fs::write(p.join(format!("file{i:02}")), b"new user bytes").unwrap();
    }
    let before = tree_hash(&p);
    for flags in [&["--dry-run"][..], &["--dry-run", "--force"][..]] {
        let mut command = s.saqta(&["undo", &id.to_string()]);
        let result = run({
            command.args(flags);
            command
        });
        assert_success(&result);
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("+2 more"));
        assert!(output.contains("with --force, would keep newer entries"));
        assert!(output.contains("would restore deleted files: 12"));
        assert_tree(&before, &p);
        assert!(displaced_for(&s, id, &p.join("file00")).is_empty());
    }
    let refused = undo(&s, id);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("file09"));
    assert!(!message.contains("file10"));
    assert!(message.contains("+2 more"));
    assert_tree(&before, &p);
}

#[test]
fn piped_log_and_show_exit_quietly() {
    let (s, p) = capture_fixture("piped_output");
    if s.command("head").arg("--version").output().is_err() {
        note("SKIP: head is missing");
        return;
    }
    for i in 0..600 {
        fs::write(p.join(format!("file{i:04}")), b"before").unwrap();
    }
    assert_success(&script(&s, &p, "rm file*"));
    assert_success(&script(&s, &p, ": > created"));
    let show = run(s.saqta(&["show", "2"]));
    assert_success(&show);
    let text = String::from_utf8(show.stdout).unwrap();
    assert!(text.lines().next().unwrap().contains("1 effects, 0 kept"));
    let row: Vec<_> = text.lines().last().unwrap().split_whitespace().collect();
    assert_eq!(&row[3..5], &["-", "-"]);
    let home = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
    let folder = home.join("cmd/2");
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(folder.join("meta")).unwrap()).unwrap();
    // Empty, legacy-format records make log exceed pipe capacity without
    // spawning hundreds of supervisors or duplicating the large effect log.
    for id in 3..1000 {
        let dest = home.join("cmd").join(id.to_string());
        fs::create_dir(&dest).unwrap();
        let mut meta = meta.clone();
        meta["id"] = id.into();
        fs::write(dest.join("meta"), serde_json::to_vec(&meta).unwrap()).unwrap();
        fs::copy(folder.join("end"), dest.join("end")).unwrap();
    }
    for text in [
        "saqta log -n 1000 | head -1",
        "saqta show 1 | head -1",
        "saqta show 1 | head -20",
    ] {
        let mut command = s.command("sh");
        let result = run({
            command.args(["-c", text]);
            command
        });
        assert_success(&result);
        assert!(!result.stdout.is_empty());
        assert!(
            result.stderr.is_empty(),
            "{text}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn undo_survives_closed_stdout() {
    for verbose in [false, true] {
        let (s, p) = capture_fixture("undo_closed_stdout");
        for program in ["bash", "head"] {
            if s.command(program).arg("--version").output().is_err() {
                note(format!("SKIP: {program} is missing for the pipefail test"));
                return;
            }
        }
        // Verbose output exceeds pipe capacity. The first line is flushed
        // before restoration starts, so head closes while work remains.
        for i in 0..500 {
            fs::write(
                p.join(format!("file{i:04}-{}", "x".repeat(80))),
                format!("original {i}\0\n"),
            )
            .unwrap();
        }
        let before = tree_hash(&p);
        assert_success(&script(&s, &p, "rm file*"));
        let id = last_id(&s);
        let text = if verbose {
            "saqta undo \"$1\" --verbose | head -1"
        } else {
            "saqta undo \"$1\" | head -1"
        };
        let mut command = s.command("bash");
        command.args(["-o", "pipefail", "-c", text, "pipe-test", &id.to_string()]);
        let started = Instant::now();
        let result = run(command);
        note(format!(
            "500-file undo with closed stdout, verbose={verbose}: {:?}",
            started.elapsed()
        ));
        // pipefail checks Saqta's status too, rather than only head's status.
        assert_success(&result);
        assert_eq!(String::from_utf8_lossy(&result.stdout).lines().count(), 1);
        assert!(
            result.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_tree(&before, &p);
        assert_eq!(shown(&s, id)["undone"], true);
        let home = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
        assert!(!home.join(format!("cmd/{id}/undo-in-progress")).exists());
    }
}

#[test]
fn undo_prints_summary_by_default_and_everything_with_verbose() {
    for verbose in [false, true] {
        let (s, p) = capture_fixture("undo_summary");
        fs::create_dir(p.join("removed")).unwrap();
        for i in 0..15 {
            fs::write(
                p.join(format!("removed/file{i:02}")),
                format!("original {i}\n"),
            )
            .unwrap();
        }
        fs::write(p.join("modified"), b"original\0\xff").unwrap();
        fs::write(p.join("rename-from"), b"rename original").unwrap();
        let before = tree_hash(&p);
        assert_success(&script(
            &s,
            &p,
            "rm -rf removed; printf changed > modified; : > created; mv rename-from rename-to",
        ));
        let id = last_id(&s);
        let record = shown(&s, id);
        let effects = record["effects"].as_array().unwrap();
        assert_eq!(effects.len(), 19);
        let after = tree_hash(&p);
        for dry_run in [true, false] {
            let mut command = s.saqta(&["undo", &id.to_string()]);
            if verbose {
                command.arg("--verbose");
            }
            if dry_run {
                command.arg("--dry-run");
            }
            let result = run(command);
            assert_success(&result);
            assert!(result.stderr.is_empty());
            let text = String::from_utf8(result.stdout).unwrap();
            let labels = if dry_run {
                [
                    "would restore deleted files: 15",
                    "would restore modified files: 1",
                    "would remove created paths: 1",
                    "would recreate directories: 1",
                    "would reverse renames: 1",
                ]
            } else {
                [
                    "restored deleted files: 15",
                    "restored modified files: 1",
                    "removed created paths: 1",
                    "recreated directories: 1",
                    "reversed renames: 1",
                ]
            };
            for label in labels {
                assert!(text.lines().any(|line| line == label), "{text}");
            }
            assert!(text.contains("elapsed: "));
            let examples = text
                .lines()
                .filter(|line| line.starts_with("example path: "))
                .count();
            assert_eq!(examples, if verbose { 0 } else { 10 }, "{text}");
            for effect in effects {
                let line = format!(
                    "{} {} {}",
                    if dry_run { "would undo" } else { "restored" },
                    effect["op"].as_str().unwrap(),
                    effect["path"].as_str().unwrap()
                );
                assert_eq!(
                    text.lines().any(|printed| printed == line),
                    verbose,
                    "{line}\n{text}"
                );
            }
            if dry_run {
                assert!(text.contains("would keep displaced versions in "));
                assert_tree(&after, &p);
                assert_eq!(shown(&s, id)["undone"], false);
            } else {
                assert!(text.contains("displaced versions kept in "));
                assert_tree(&before, &p);
                assert_eq!(shown(&s, id)["undone"], true);
            }
        }
    }
}

#[test]
fn undo_reports_every_unprotected_item_in_summary() {
    let (mut s, p) = capture_fixture("undo_unprotected_summary");
    s.env.insert("SAQTA_MAX_COPY".into(), "1".into());
    for i in 0..12 {
        fs::write(p.join(format!("file{i:02}")), b"larger than copy limit").unwrap();
    }
    assert_success(&script(
        &s,
        &p,
        "for path in file*; do printf replaced > \"$path\"; done",
    ));
    let id = last_id(&s);
    let after = tree_hash(&p);
    for dry_run in [true, false] {
        let mut command = s.saqta(&["undo", &id.to_string()]);
        if dry_run {
            command.arg("--dry-run");
        }
        let result = run(command);
        assert_success(&result);
        let text = String::from_utf8(result.stdout).unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("could not restore "))
                .count(),
            12,
            "{text}"
        );
        for i in 0..12 {
            assert!(
                text.contains(&format!(
                    "could not restore {}: not protected:",
                    p.join(format!("file{i:02}")).display()
                )),
                "{text}"
            );
        }
        assert_tree(&after, &p);
    }
}

#[test]
fn log_shows_local_time_and_json_keeps_nanoseconds() {
    let (s, p) = capture_fixture("local_time");
    let long_command = format!("true # {}\n# second line", "x".repeat(200));
    assert_success(&script(&s, &p, &long_command));
    let home = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("cmd/1/meta")).unwrap()).unwrap();
    let end: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("cmd/1/end")).unwrap()).unwrap();
    // A fixed legacy timestamp exercises local conversion and subsecond
    // precision deterministically, independent of the machine's timezone.
    let start_ns = 1_700_000_000_123_456_789u64;
    let mut legacy_meta = meta;
    legacy_meta["start_ns"] = start_ns.into();
    let mut legacy_end = end;
    legacy_end["end_ns"] = (start_ns + 800_000_000).into();
    fs::write(
        home.join("cmd/1/meta"),
        serde_json::to_vec(&legacy_meta).unwrap(),
    )
    .unwrap();
    fs::write(
        home.join("cmd/1/end"),
        serde_json::to_vec(&legacy_end).unwrap(),
    )
    .unwrap();
    let mut command = s.saqta(&["log"]);
    let result = run({
        command.env("TZ", "UTC-5").env("COLUMNS", "120");
        command
    });
    assert_success(&result);
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("2023-11-15 03:13:20"), "{text}");
    assert!(text.contains("0.8 s"));
    assert_eq!(text.lines().count(), 2);
    assert!(
        text.lines().all(|line| line.chars().count() <= 120),
        "{text}"
    );
    assert!(text.trim_end().ends_with("..."));
    assert_eq!(
        text.lines().next().unwrap().find("STARTED"),
        text.lines().nth(1).unwrap().find("2023-11-15")
    );
    let mut command = s.saqta(&["log"]);
    command.env("TZ", "UTC-5").env("COLUMNS", "80");
    let narrow = run(command);
    assert_success(&narrow);
    assert!(
        String::from_utf8_lossy(&narrow.stdout)
            .lines()
            .all(|line| line.chars().count() <= 80)
    );
    let result = run(s.saqta(&["log", "--json"]));
    assert_success(&result);
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json[0]["start_ns"], start_ns);
    assert_eq!(json[0]["end_ns"], start_ns + 800_000_000);
    assert_eq!(json[0]["duration_ns"], 800_000_000);
    assert_eq!(json[0]["command"], legacy_meta["command"]);
}
#[test]
fn sed_rename_over_fifty_files() {
    let (s, p) = capture_fixture("sed_fifty");
    for i in 0..50 {
        fs::write(p.join(format!("f{i}")), format!("before {i}\n")).unwrap();
    }
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "sed -i 's/before/after/' f*"));
    assert_eq!(fs::read(p.join("f49")).unwrap(), b"after 49\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn shell_truncation_restores_empty_and_nonempty_files() {
    let (s, p) = capture_fixture("truncation");
    fs::write(p.join("file"), b"before\0data").unwrap();
    fs::write(p.join("empty"), b"").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, ": > file; printf x > empty"));
    assert_eq!(fs::read(p.join("file")).unwrap(), b"");
    restore_exact(&s, &p, &before);
}
#[test]
fn rdwr_without_truncation_captures_once_per_inode() {
    let (s, p) = capture_fixture("rdwr");
    if !python(&s) {
        return;
    }
    fs::write(p.join("file"), b"original data").unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); f=l.open(b'file',2); assert f>=0; assert l.pwrite(f,b'CHANGED',7,0)==7; l.close(f); f=l.open(b'file',2); assert l.pwrite(f,b'XX',2,0)==2; l.close(f)";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_ne!(fs::read(p.join("file")).unwrap(), b"original data");
    let id = last_id(&s);
    let shown = shown(&s, id);
    let effects = shown["effects"].as_array().unwrap();
    assert_eq!(
        effects
            .iter()
            .filter(|e| e["note"]["kept"] == "copy")
            .count(),
        1
    );
    restore_exact(&s, &p, &before);
}
#[test]
fn mv_over_file_restores_both_names() {
    let (s, p) = capture_fixture("mv_over");
    fs::write(p.join("a"), "source").unwrap();
    fs::write(p.join("b"), "victim").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "mv a b"));
    assert!(!p.join("a").exists());
    assert_eq!(fs::read(p.join("b")).unwrap(), b"source");
    restore_exact(&s, &p, &before);
}
#[test]
fn mv_directory_and_later_deletion() {
    let (s, p) = capture_fixture("mv_directory");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "mv tree moved; rm -rf moved/nested"));
    assert!(p.join("moved").exists());
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn rename_exchange_swaps_back() {
    let (s, p) = capture_fixture("exchange");
    if !python(&s) {
        return;
    }
    fs::write(p.join("a"), "aaa").unwrap();
    fs::write(p.join("b"), "bbb").unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); r=l.syscall(316,-100,b'a',-100,b'b',2); assert r==0,ctypes.get_errno()";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_eq!(fs::read(p.join("a")).unwrap(), b"bbb");
    restore_exact(&s, &p, &before);
}
#[test]
fn creations_only_are_removed_in_reverse_order() {
    let (s, p) = capture_fixture("creations");
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "mkdir -p a/b/c; touch a/file; ln a/file a/hard; ln -s missing a/sym; mkfifo a/pipe",
    ));
    assert!(p.join("a/pipe").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn creation_then_overwrite_then_delete_needs_no_content() {
    let (s, p) = capture_fixture("ephemeral");
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo first > ephemeral; echo second > ephemeral; rm ephemeral",
    ));
    assert!(!p.join("ephemeral").exists());
    let shown = shown(&s, last_id(&s));
    assert!(
        shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["quarantine"] == "")
    );
    restore_exact(&s, &p, &before);
}
#[test]
fn deletion_outside_project_in_decoy_home() {
    let (s, p) = capture_fixture("outside_project");
    let home = PathBuf::from(&s.env[OsStr::new("HOME")]);
    fs::write(home.join("outside"), b"outside project").unwrap();
    let before = tree_hash(&home);
    assert_success(&script(&s, &p, "rm ../outside"));
    assert!(!home.join("outside").exists());
    restore_exact(&s, &home, &before);
}
#[test]
fn decoy_home_wipe_preserves_saqta_and_restores_everything_else() {
    let (mut s, p) = capture_fixture("home_wipe");
    let home = PathBuf::from(&s.env[OsStr::new("HOME")]);
    let store = home.join(".saqta");
    s.env
        .insert("SAQTA_HOME".into(), store.clone().into_os_string());
    sample(&p);
    fs::write(home.join("outside"), b"decoy home").unwrap();
    let before = tree_hash(&home);
    let o = script(&s, &p, "rm -rf \"$HOME\"");
    assert!(!o.status.success());
    assert!(store.join("cmd").exists());
    assert!(!p.exists());
    assert_success(&undo(&s, last_id(&s)));
    let mut after = tree_hash(&home);
    after.retain(|p, _| !p.starts_with(b".saqta"));
    assert_eq!(before, after);
}
#[test]
fn find_delete_uses_directory_fds() {
    let (s, p) = capture_fixture("find_delete");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "find tree -delete"));
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn eight_parallel_removers_share_one_journal() {
    let (s, p) = capture_fixture("parallel_rm");
    for i in 0..96 {
        fs::write(p.join(format!("f{i}")), format!("{i}\n")).unwrap();
    }
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "find . -type f -print0 | xargs -0 -P 8 -n 1 rm",
    ));
    assert_eq!(fs::read_dir(&p).unwrap().count(), 0);
    restore_exact(&s, &p, &before);
}
#[test]
fn renamed_then_deleted_working_directory() {
    let (s, p) = capture_fixture("renamed_cwd");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "cd tree/nested; mv ../../tree ../../moved; rm file; cd -P ..; rmdir nested/empty; rmdir nested; cd -P ..; rmdir moved/empty; rmdir moved",
    ));
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn agent_settings_and_store_writes_are_blocked() {
    let (s, p) = capture_fixture("deny_floor");
    let home = PathBuf::from(&s.env[OsStr::new("HOME")]);
    for dir in [home.join(".claude"), home.join(".codex"), p.join(".claude")] {
        fs::create_dir(&dir).unwrap();
    }
    for f in [
        home.join(".claude/settings.json"),
        home.join(".claude/settings.local.json"),
        home.join(".codex/config.toml"),
        home.join(".codex/hooks.json"),
        p.join(".claude/settings.json"),
        p.join(".claude/settings.local.json"),
    ] {
        fs::write(f, "keep\n").unwrap();
    }
    let before = tree_hash(&home);
    let o = script(
        &s,
        &p,
        "for f in \"$HOME\"/.claude/*.json \"$HOME\"/.codex/* .claude/*.json \"$SAQTA_HOME/new\"; do (echo damage > \"$f\") && exit 99; done; mv .claude/settings.json moved; rm .claude/settings.local.json; exit 0",
    );
    assert_success(&o);
    assert!(String::from_utf8_lossy(&o.stderr).contains("Operation not permitted"));
    assert_tree(&before, &home);
    assert!(
        !PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")])
            .join("new")
            .exists()
    );
    let shown = shown(&s, last_id(&s));
    assert!(
        shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["op"] == "blocked")
            .count()
            >= 9
    );
}
#[test]
fn tmpfs_mount_is_skipped() {
    let (s, p) = capture_fixture("tmpfs");
    let mount = p.join("mount");
    fs::create_dir(&mount).unwrap();
    let mut c = s.command("mount");
    c.args(["-t", "tmpfs", "tmpfs"]).arg(&mount);
    let o = match c.output() {
        Ok(o) => o,
        Err(e) => {
            note(format!("SKIP: cannot run mount: {e}"));
            return;
        }
    };
    if !o.status.success() {
        note(format!(
            "SKIP: cannot mount tmpfs: {}",
            String::from_utf8_lossy(&o.stderr)
        ));
        return;
    }
    // Always unmount before Sandbox cleanup, including an assertion failure.
    struct Mounted<'a>(&'a Sandbox, PathBuf);
    impl Drop for Mounted<'_> {
        fn drop(&mut self) {
            let mut c = self.0.command("umount");
            c.arg(&self.1);
            assert_success(&run(c));
        }
    }
    let _mounted = Mounted(&s, mount.clone());
    fs::write(mount.join("victim"), "volatile").unwrap();
    assert_success(&script(&s, &p, "rm mount/victim; touch mount/new"));
    assert!(!mount.join("victim").exists());
    let shown = shown(&s, last_id(&s));
    assert!(shown["effects"].as_array().unwrap().is_empty());
}
#[test]
fn unsupported_interfaces_return_enosys() {
    let (s, p) = capture_fixture("enosys");
    if !python(&s) {
        return;
    }
    for (nr, args) in [
        (
            437,
            "-100,b'.',ctypes.byref(ctypes.create_string_buffer(24)),24",
        ),
        (425, "1,ctypes.byref(ctypes.create_string_buffer(120))"),
    ] {
        let code = format!(
            "import ctypes; l=ctypes.CDLL(None,use_errno=True); r=l.syscall({nr},{args}); print(ctypes.get_errno() if r<0 else 0); r>=0 and l.close(r)"
        );
        let mut bare = s.command("python3");
        bare.current_dir(&p).args(["-c", &code]);
        let bare = run(bare);
        if !bare.status.success() || bare.stdout == b"38\n" {
            note(format!(
                "SKIP: bare syscall {nr} unavailable: {}",
                String::from_utf8_lossy(&bare.stdout)
            ));
            continue;
        }
        note(format!(
            "bare syscall {nr}: {}",
            String::from_utf8_lossy(&bare.stdout).trim()
        ));
        let mut c = s.exec(&["--", "python3", "-c", &code]);
        c.current_dir(&p);
        let o = run(c);
        assert_success(&o);
        assert_eq!(o.stdout, b"38\n");
    }
}
#[test]
fn undo_conflict_lists_path_and_changes_nothing() {
    let (s, p) = capture_fixture("conflict");
    fs::write(p.join("a"), "original").unwrap();
    fs::write(p.join("b"), "deleted").unwrap();
    assert_success(&script(&s, &p, "echo command > a; rm b"));
    let id = last_id(&s);
    fs::write(p.join("a"), "later user edits").unwrap();
    let before = tree_hash(&p);
    let o = undo(&s, id);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains(&p.join("a").display().to_string()));
    assert_tree(&before, &p);
}
#[test]
fn second_undo_is_refused() {
    let (s, p) = capture_fixture("second_undo");
    fs::write(p.join("a"), "before").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm a"));
    let id = last_id(&s);
    assert_success(&undo(&s, id));
    assert_tree(&before, &p);
    let o = undo(&s, id);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("already undone"));
    assert_tree(&before, &p);
}
#[test]
fn killed_supervisor_does_not_hang_and_imports_incomplete() {
    let (s, p) = capture_fixture("killed_supervisor");
    let tree = p.join("tree");
    fs::create_dir(&tree).unwrap();
    for i in 0..3000 {
        fs::write(tree.join(format!("f{i}")), format!("original {i}\n")).unwrap();
    }
    let before = tree_hash(&p);
    let mut c = s.exec(&["--", "rm", "-rf", "tree"]);
    c.current_dir(&p);
    let running = Running::spawn(c, None);
    let store = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
    wait_until(Duration::from_secs(5), || {
        s.run_markers().len() == 1
            && fs::read(store.join("cmd/1/effects.log"))
                .is_ok_and(|b| b.iter().filter(|&&v| v == b'\n').count() >= 20)
    });
    let supervisor = s.run_markers()[0];
    let start = Instant::now();
    signal(supervisor, libc::SIGKILL);
    let o = running.output();
    assert!(!o.status.success());
    note(format!(
        "supervisor kill to wrapper return: {:?}",
        start.elapsed()
    ));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(fs::read_dir(&tree).unwrap().count() < 3000);
    let id = last_id(&s);
    let shown = shown(&s, id);
    assert_eq!(shown["status"], "incomplete");
    assert_success(&undo(&s, id));
    assert_tree(&before, &p);
}
#[test]
fn setsid_background_deletions_remain_in_same_command() {
    let (s, p) = capture_fixture("setsid_background");
    sample(&p);
    let before = tree_hash(&p);
    let start = Instant::now();
    let o = script(
        &s,
        &p,
        "setsid sh -c 'sleep 0.5; rm -rf tree' >/dev/null 2>&1 & echo started",
    );
    assert_success(&o);
    assert!(start.elapsed() < Duration::from_millis(450));
    let id = last_id(&s);
    let o = undo(&s, id);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("still running"));
    wait_until(Duration::from_secs(5), || s.run_markers().is_empty());
    assert!(!p.join("tree").exists());
    assert_eq!(last_id(&s), id);
    restore_exact(&s, &p, &before);
}
#[test]
fn dry_run_preserves_damage_then_real_undo_restores() {
    let (s, p) = capture_fixture("dry_run");
    fs::write(p.join("file"), "before").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm file"));
    let id = last_id(&s);
    let damage = tree_hash(&p);
    let o = run(s.saqta(&["undo", &id.to_string(), "--dry-run"]));
    assert_success(&o);
    assert!(String::from_utf8_lossy(&o.stdout).contains("would undo"));
    assert_tree(&damage, &p);
    assert_success(&undo(&s, id));
    assert_tree(&before, &p);
}
#[test]
fn undo_inside_supervised_environment_is_refused() {
    let (s, p) = capture_fixture("nested_undo");
    let o = script(&s, &p, "saqta undo 1");
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("run undo in your own terminal"));
}
#[test]
fn final_symlink_write_restores_target_without_replacing_link() {
    let (s, p) = capture_fixture("follow_final_link");
    fs::write(p.join("target"), "original target").unwrap();
    symlink("target", p.join("alias")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo changed > alias"));
    assert_eq!(fs::read(p.join("target")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn max_copy_limit_reports_unprotected_and_keeps_deletes() {
    let (mut s, p) = capture_fixture("max_copy");
    s.env.insert("SAQTA_MAX_COPY".into(), "8".into());
    fs::write(p.join("modified"), "larger than eight bytes").unwrap();
    fs::write(p.join("deleted"), "also larger than eight bytes").unwrap();
    assert_success(&script(&s, &p, ": > modified; rm deleted"));
    let id = last_id(&s);
    let shown = shown(&s, id);
    assert!(
        shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["op"] == "not protected" && e["note"]["message"] == "too large")
    );
    assert_success(&undo(&s, id));
    assert_eq!(
        fs::read(p.join("deleted")).unwrap(),
        b"also larger than eight bytes"
    );
    assert_eq!(fs::read(p.join("modified")).unwrap(), b"");
}
#[test]
fn growing_truncate_records_only_size_and_restores_mtime() {
    let (s, p) = capture_fixture("grow_truncate");
    if !python(&s) {
        return;
    }
    fs::write(p.join("file"), "before").unwrap();
    let before = tree_hash(&p);
    let code =
        "import ctypes; l=ctypes.CDLL(None,use_errno=True); assert l.truncate(b'file',100)==0";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_eq!(fs::metadata(p.join("file")).unwrap().len(), 100);
    let shown = shown(&s, last_id(&s));
    assert!(
        shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["op"] == "size" && e["quarantine"] == "")
    );
    restore_exact(&s, &p, &before);
}
#[test]
fn symlink_parent_substitution_refuses_undo() {
    let (s, p) = capture_fixture("unsafe_parent");
    sample(&p);
    assert_success(&script(&s, &p, "rm tree/nested/file"));
    let id = last_id(&s);
    fs::rename(p.join("tree"), p.join("moved")).unwrap();
    symlink("moved", p.join("tree")).unwrap();
    let before = tree_hash(&p);
    let o = undo(&s, id);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("changed after"));
    assert_tree(&before, &p);
    let forced = run(s.saqta(&["undo", &id.to_string(), "--force"]));
    assert!(!forced.status.success());
    assert!(
        String::from_utf8_lossy(&forced.stderr)
            .contains("cannot bypass directory traversal checks")
    );
    assert_tree(&before, &p);
}
#[test]
fn failed_rename_does_not_reverse_an_operation_that_never_happened() {
    let (s, p) = capture_fixture("failed_rename");
    fs::create_dir(p.join("a")).unwrap();
    fs::create_dir(p.join("b")).unwrap();
    fs::write(p.join("b/keep"), "keep").unwrap();
    let before = tree_hash(&p);
    let o = script(&s, &p, "mv -T a b");
    assert!(!o.status.success());
    restore_exact(&s, &p, &before);
}
#[test]
fn deleted_hard_link_is_frozen_before_other_alias_is_written() {
    let (s, p) = capture_fixture("freeze_link");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm a; echo changed > b"));
    assert!(!p.join("a").exists());
    restore_exact(&s, &p, &before);
}

#[test]
fn previously_created_name_can_later_hold_an_original_file() {
    let (s, p) = capture_fixture("created_name_reused");
    fs::write(p.join("original"), "keep original").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo temporary > reused; rm reused; mv original reused; rm reused",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn rename_over_created_name_then_write_keeps_original_source() {
    let (s, p) = capture_fixture("rename_over_created");
    fs::write(p.join("original"), "original bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo temporary > new; mv original new; echo changed > new; rm new",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn deleting_original_then_writing_new_hard_link_keeps_preimage() {
    let (s, p) = capture_fixture("new_link_after_delete");
    fs::write(p.join("original"), "keep bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "ln original new; rm original; echo changed > new",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn writing_new_hard_link_restores_original_name() {
    let (s, p) = capture_fixture("write_new_link");
    fs::write(p.join("original"), "keep bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "ln original new; echo changed > new"));
    assert_eq!(fs::read(p.join("original")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn settings_symlink_to_plain_file_still_blocks_write() {
    let (s, p) = capture_fixture("settings_alias");
    fs::create_dir(p.join(".claude")).unwrap();
    fs::write(p.join("plain"), "keep").unwrap();
    symlink("../plain", p.join(".claude/settings.json")).unwrap();
    let before = tree_hash(&p);
    let o = script(&s, &p, "echo changed > .claude/settings.json");
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("Operation not permitted"));
    assert_tree(&before, &p);
    assert!(
        shown(&s, last_id(&s))["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["op"] == "blocked")
    );
}
#[test]
fn legacy_open_creat_and_shrinking_truncate() {
    let (s, p) = capture_fixture("legacy_calls");
    if !python(&s) {
        return;
    }
    for name in ["open", "creat", "shrink", "equal"] {
        fs::write(p.join(name), "original bytes").unwrap();
    }
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); f=l.syscall(2,b'open',1,0); assert f>=0; assert l.write(f,b'XX',2)==2; l.close(f); f=l.syscall(85,b'creat',384); assert f>=0; l.close(f); assert l.syscall(76,b'shrink',2)==0; assert l.syscall(76,b'equal',14)==0";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_eq!(fs::read(p.join("creat")).unwrap(), b"");
    assert_eq!(fs::read(p.join("shrink")).unwrap(), b"or");
    restore_exact(&s, &p, &before);
}
#[test]
fn raw_directory_fd_operations_restore_exactly() {
    let (s, p) = capture_fixture("raw_dirfds");
    if !python(&s) {
        return;
    }
    fs::create_dir(p.join("d")).unwrap();
    fs::write(p.join("d/original"), "old").unwrap();
    fs::write(p.join("d/victim"), "victim").unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); d=l.open(b'd',2097152|65536); assert d>=0; assert l.syscall(258,d,b'newdir',448)==0; assert l.syscall(266,b'original',d,b'sym')==0; assert l.syscall(265,d,b'original',d,b'hard',0)==0; assert l.syscall(259,d,b'fifo',4096|384,0)==0; assert l.syscall(264,d,b'original',d,b'victim')==0; assert l.syscall(316,d,b'victim',d,b'final',1)==0; assert l.syscall(263,d,b'newdir',512)==0; l.close(d)";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert!(!p.join("d/original").exists());
    assert_eq!(fs::read(p.join("d/final")).unwrap(), b"old");
    restore_exact(&s, &p, &before);
}
#[test]
fn existing_fifo_and_socket_are_linked_without_opening_content() {
    let (s, p) = capture_fixture("special_files");
    let mut c = s.command("mkfifo");
    c.arg(p.join("fifo"));
    assert_success(&run(c));
    let socket = match std::os::unix::net::UnixListener::bind(p.join("socket")) {
        Ok(socket) => Some(socket),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            note(format!(
                "SKIP: Unix socket fixture unavailable: {e}; still testing FIFO"
            ));
            None
        }
        Err(e) => panic!("cannot bind fixture socket: {e}"),
    };
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm -f fifo socket"));
    assert!(!p.join("fifo").exists());
    restore_exact(&s, &p, &before);
    drop(socket);
}
#[test]
fn sqlite_index_uses_blob_paths_and_can_be_rebuilt() {
    let (s, p) = capture_fixture("sqlite_index");
    let name = OsString::from_vec(b"raw-\xff\n%".to_vec());
    fs::write(p.join(&name), "bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm -- *"));
    let id = last_id(&s);
    let dbpath = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]).join("journal.db");
    {
        let db = rusqlite::Connection::open(&dbpath).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(db.query_row("SELECT count(*) FROM effects WHERE typeof(path) != 'blob' OR typeof(path2) != 'blob'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        let stored: Vec<u8> = db
            .query_row(
                "SELECT path FROM effects WHERE command_id=?1",
                [id as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, bytes(&p.join(&name)));
    }
    fs::remove_file(dbpath).unwrap();
    assert_eq!(last_id(&s), id);
    assert_success(&undo(&s, id));
    assert_tree(&before, &p);
}
#[test]
fn concurrent_commands_allocate_unique_ids_and_log_limit() {
    let (s, p) = capture_fixture("unique_ids");
    let children: Vec<_> = (0..12)
        .map(|i| {
            let mut c = s.exec(&["-c", &format!("echo x > f{i}; rm f{i}")]);
            c.current_dir(&p);
            Running::spawn(c, None)
        })
        .collect();
    for child in children {
        assert_success(&child.output());
    }
    let o = run(s.saqta(&["log", "--json", "-n", "4"]));
    assert_success(&o);
    let values: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let values = values.as_array().unwrap();
    assert_eq!(values.len(), 4);
    assert_eq!(values[0]["id"], 12);
    for v in values {
        assert_eq!(v["counts"]["created"], 1);
        assert_eq!(v["counts"]["deleted"], 1);
        assert_eq!(v["exit_status"], 0);
    }
    assert_eq!(fs::read_dir(&p).unwrap().count(), 0);
}

#[test]
fn in_place_write_restores_untouched_hard_link_alias() {
    let (s, p) = capture_fixture("untouched_alias");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo changed > a"));
    assert_eq!(fs::read(p.join("b")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn modified_then_deleted_hard_link_restores_untouched_alias() {
    let (s, p) = capture_fixture("modified_deleted_alias");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo changed > a; rm a"));
    assert!(!p.join("a").exists());
    assert_eq!(fs::read(p.join("b")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn two_written_hard_links_restore_both_names() {
    let (s, p) = capture_fixture("both_aliases");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo first > a; echo second > b"));
    assert_eq!(fs::read(p.join("b")).unwrap(), b"second\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn pathname_read_stops_at_nul_before_protected_page() {
    let (s, p) = capture_fixture("memory_boundary");
    if !python(&s) {
        return;
    }
    fs::write(p.join("file"), "saved").unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); page=l.getpagesize(); l.mmap.restype=ctypes.c_void_p; mem=l.mmap(None,page*2,3,34,-1,0); assert mem!=ctypes.c_void_p(-1).value; assert l.mprotect(ctypes.c_void_p(mem+page),page,0)==0; address=mem+page-5; ctypes.memmove(address,b'file\\0',5); assert l.syscall(87,ctypes.c_void_p(address))==0; l.munmap(ctypes.c_void_p(mem),page*2)";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert!(!p.join("file").exists());
    restore_exact(&s, &p, &before);
}

#[test]
fn growing_truncate_restores_hard_link_alias_size() {
    let (s, p) = capture_fixture("truncate_alias");
    if !python(&s) {
        return;
    }
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); assert l.truncate(b'a',1024)==0";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_eq!(fs::metadata(p.join("b")).unwrap().len(), 1024);
    restore_exact(&s, &p, &before);
}
