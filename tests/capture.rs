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
