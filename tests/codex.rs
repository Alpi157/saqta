mod common;
use common::*;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Command, Output, Stdio},
};

fn hook(s: &Sandbox, payload: &Value, off: bool) -> Output {
    let mut c = s.saqta(&["hook", "codex"]);
    if off {
        c.env("SAQTA_OFF", "1");
    }
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(payload).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}
fn payload(tool: &str, event: &str, cwd: &Path, command: &str) -> Value {
    json!({"session_id":"synthetic-session","turn_id":"turn-1","transcript_path":"unused","cwd":cwd,"hook_event_name":event,"model":"synthetic","permission_mode":"default","tool_name":tool,"tool_use_id":"synthetic-call","tool_input":{"command":command}})
}
#[test]
fn hook_rewrite_and_fail_open() {
    let s = Sandbox::new("hooks");
    let v = payload(
        "Bash",
        "PreToolUse",
        &s.dir,
        "printf '%s\\n' 'a$b'\ncat <<'END'\nquote '\\ & ;\nEND\nexit 7",
    );
    let start = std::time::Instant::now();
    let o = hook(&s, &v, false);
    assert_success(&o);
    note(format!("Bash hook elapsed: {:?}", start.elapsed()));
    let result: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(result["hookSpecificOutput"]["permissionDecision"], "allow");
    let rewritten = result["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    let mut original = s.command("/bin/bash");
    original.args(["-lc", v["tool_input"]["command"].as_str().unwrap()]);
    let mut wrapped = s.command("/bin/bash");
    wrapped.args(["-lc", rewritten]);
    let a = run(original);
    let b = run(wrapped);
    assert_eq!(
        (a.stdout, a.stderr, a.status.code()),
        (b.stdout, b.stderr, b.status.code())
    );
    let again = payload("Bash", "PreToolUse", &s.dir, rewritten);
    assert!(hook(&s, &again, false).stdout.is_empty());
    assert!(hook(&s, &v, true).stdout.is_empty());
    assert!(
        hook(&s, &payload("unknown", "PreToolUse", &s.dir, "true"), false)
            .stdout
            .is_empty()
    );
    assert_success(&hook(&s, &json!("garbage"), false));
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    assert!(
        fs::read_to_string(home.join("logs/hook.log"))
            .unwrap()
            .contains("unhandled")
    );
}
#[test]
fn patch_add_update_delete_move_and_undo() {
    let (s, p) = capture_fixture("patch");
    for (name, bytes) in [
        ("update", b"old update".as_slice()),
        ("delete", b"old delete"),
        ("move", b"old move"),
        ("destination", b"old destination"),
    ] {
        fs::write(p.join(name), bytes).unwrap();
    }
    let before = tree_hash(&p);
    let patch = "*** Begin Patch\n*** Add File: added\n+new\n*** Update File: update\n@@\n-old update\n+new update\n*** Delete File: delete\n*** Update File: move\n*** Move to: destination\n@@\n-old move\n+new move\n*** End Patch\n";
    let pre = payload("apply_patch", "PreToolUse", &p, patch);
    let o = hook(&s, &pre, false);
    assert_success(&o);
    assert!(o.stdout.is_empty());
    // Tiny applier for this fixture, independent of hook capture.
    for (name, bytes) in [
        ("added", "new"),
        ("update", "new update"),
        ("destination", "new move"),
    ] {
        fs::write(p.join(name), bytes).unwrap();
    }
    for name in ["delete", "move"] {
        fs::remove_file(p.join(name)).unwrap();
    }
    let mut post = pre;
    post["hook_event_name"] = json!("PostToolUse");
    assert_success(&hook(&s, &post, false));
    let id = last_id(&s);
    let shown = shown(&s, id);
    assert_eq!(shown["status"], "finished");
    assert_success(&undo(&s, id));
    assert_tree(&before, &p);
}
#[test]
fn patch_deny_floor_and_missing_post() {
    let s = Sandbox::new("patch-deny");
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let path = home.join("keep");
    fs::write(&path, b"secret").unwrap();
    let patch = format!(
        "*** Begin Patch\n*** Delete File: {}\n*** End Patch\n",
        path.display()
    );
    assert_success(&hook(
        &s,
        &payload("apply_patch", "PreToolUse", &s.dir, &patch),
        false,
    ));
    let r = shown(&s, last_id(&s));
    assert_eq!(r["status"], "incomplete");
    assert_eq!(r["effects"][0]["op"], "not protected");
    assert_eq!(r["effects"][0]["quarantine"], "");
}
fn init(s: &Sandbox, project: &Path, remove: bool) -> Output {
    let mut c = s.saqta(&[if remove { "uninstall" } else { "init" }, "--codex"]);
    c.arg(project);
    run(c)
}
#[test]
fn init_uninstall_exact_roundtrip_and_idempotence() {
    let (s, p) = capture_fixture("init");
    let mut git = s.command("git");
    git.args(["--no-optional-locks", "init", "-q"]).arg(&p);
    assert_success(&run(git));
    fs::create_dir(p.join(".codex")).unwrap();
    let h=b"{ \"custom\": 7, \"hooks\": { \"PreToolUse\": [{\"matcher\":\"other\",\"hooks\":[]}] } }\n";
    let c=b"# user's config\nmodel = 'custom'\n[sandbox_workspace_write]\n# keep comment\nwritable_roots = [ '/somewhere' ] # roots\n[other]\nflag = true\n";
    fs::write(p.join(".codex/hooks.json"), h).unwrap();
    fs::write(p.join(".codex/config.toml"), c).unwrap();
    let before = fs::read(p.join(".git/info/exclude")).unwrap();
    assert_success(&init(&s, &p, false));
    let once = tree_hash(&p);
    assert_success(&init(&s, &p, false));
    assert_tree(&once, &p);
    let mut status = s.saqta(&["status"]);
    status.current_dir(&p);
    let o = run(status);
    assert_success(&o);
    assert!(String::from_utf8_lossy(&o.stdout).contains("hooks 3/3"));
    assert_success(&init(&s, &p, true));
    assert_eq!(fs::read(p.join(".codex/hooks.json")).unwrap(), h);
    assert_eq!(fs::read(p.join(".codex/config.toml")).unwrap(), c);
    assert_eq!(fs::read(p.join(".git/info/exclude")).unwrap(), before);
}
#[test]
fn init_refuses_non_repo_and_saqta() {
    let s = Sandbox::new("refuse");
    assert!(!init(&s, &s.dir, false).status.success());
    assert!(
        !init(&s, Path::new(env!("CARGO_MANIFEST_DIR")), false)
            .status
            .success()
    );
}
// Probe only namespace setup. A protected-command failure must never turn into SKIP.
fn probe_namespace(mut command: Command, flags: &[&str]) -> Result<(), String> {
    let output = command.args(flags).arg("/bin/true").output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "{}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("unshare is missing: {e}"))
        }
        Err(e) => panic!("could not start namespace probe: {e}"),
    }
}
fn namespace_available(s: &Sandbox, flags: &[&str], capability: &str) -> bool {
    match probe_namespace(s.command("unshare"), flags) {
        Ok(()) => true,
        Err(reason) => {
            note(format!("SKIP: {capability} unavailable: {reason}"));
            false
        }
    }
}
const PID_NAMESPACE: &[&str] = &[
    "--user",
    "--map-root-user",
    "--pid",
    "--fork",
    "--mount-proc",
];
const MOUNT_NAMESPACE: &[&str] = &["--user", "--map-root-user", "--mount"];
// util-linux 2.37.2 ignores INT and TERM before forking its namespace child.
const OLD_UNSHARE_SIGNALS: &str = "import os,signal,sys; signal.signal(signal.SIGINT,signal.SIG_IGN); signal.signal(signal.SIGTERM,signal.SIG_IGN); os.execvp(sys.argv[1],sys.argv[1:])";
// Establish the test's requested signal dispositions, then make W exactly PID 1.
const DEFAULT_SIGNALS: &str = "import os,signal,sys; signal.signal(signal.SIGINT,signal.SIG_DFL); signal.signal(signal.SIGTERM,signal.SIG_DFL); os.execv(sys.argv[1],sys.argv[1:])";
#[test]
fn pid_one_namespace_finishes_journal() {
    let mut s = Sandbox::new("pid-one");
    if !namespace_available(&s, PID_NAMESPACE, "unprivileged PID namespace") || !python(&s) {
        return;
    }
    for no_sync in [false, true] {
        s.env.insert(
            "SAQTA_NO_SYNC_WAKE".into(),
            if no_sync { "1" } else { "0" }.into(),
        );
        for old_unshare in [false, true] {
            for (script, code) in [
                ("printf foreground > file", 0),
                ("printf background > file; sleep 10 >/dev/null 2>&1 &", 0),
                ("printf signal > file; kill -TERM $$", 143),
            ] {
                let mut c = if old_unshare {
                    let mut c = s.command("python3");
                    c.args(["-c", OLD_UNSHARE_SIGNALS, "unshare"]);
                    c
                } else {
                    s.command("unshare")
                };
                c.args(PID_NAMESPACE).args([
                    "python3",
                    "-c",
                    DEFAULT_SIGNALS,
                    BIN,
                    "exec",
                    "--shell",
                    "/bin/bash",
                    "-c",
                    script,
                ]);
                let o = run(c);
                assert_eq!(
                    o.status.code(),
                    Some(code),
                    "no_sync={no_sync}, old_unshare={old_unshare}, script={script}, stderr={}",
                    String::from_utf8_lossy(&o.stderr)
                );
                let id = last_id(&s);
                let record = shown(&s, id);
                assert_eq!(record["status"], "finished");
                assert_eq!(record["exit_status"], code);
                note(format!(
                    "PID 1: no_sync={no_sync}, old_unshare={old_unshare}, exit={code}, journal finished"
                ));
            }
        }
    }
}
#[test]
fn inherited_ignored_term_is_preserved_without_namespaces() {
    let mut s = Sandbox::new("ignored-term");
    if !python(&s) {
        return;
    }
    for no_sync in [false, true] {
        s.env.insert(
            "SAQTA_NO_SYNC_WAKE".into(),
            if no_sync { "1" } else { "0" }.into(),
        );
        let script = "printf before; kill -TERM $$; printf after";
        let mut direct = s.command("python3");
        direct.args(["-c", OLD_UNSHARE_SIGNALS, "/bin/bash", "-c", script]);
        let a = run(direct);
        let mut wrapped = s.command("python3");
        wrapped.args([
            "-c",
            OLD_UNSHARE_SIGNALS,
            BIN,
            "exec",
            "--shell",
            "/bin/bash",
            "-c",
            script,
        ]);
        let b = run(wrapped);
        assert_eq!(a.status.code(), Some(0));
        assert_eq!(b.status.code(), Some(0));
        assert_eq!((a.stdout, a.stderr), (b.stdout, b.stderr));
        let record = shown(&s, last_id(&s));
        assert_eq!(record["status"], "finished");
        assert_eq!(record["exit_status"], 0);

        // The namespace fixture's repair must work even on hosts that block namespaces.
        let mut restored = s.command("python3");
        restored.args([
            "-c",
            OLD_UNSHARE_SIGNALS,
            "python3",
            "-c",
            DEFAULT_SIGNALS,
            BIN,
            "exec",
            "--shell",
            "/bin/bash",
            "-c",
            script,
        ]);
        let o = run(restored);
        assert_eq!(o.status.signal(), Some(libc::SIGTERM));
        assert_eq!(o.stdout, b"before");
        let record = shown(&s, last_id(&s));
        assert_eq!(record["status"], "finished");
        assert_eq!(record["exit_status"], 143);
    }
}
#[test]
fn namespace_probe_reports_uid_map_denial() {
    let s = Sandbox::new("namespace-denied");
    // Exercise Ubuntu 24.04's failure text on every machine, without creating a namespace.
    let mut denied = s.command("/bin/sh");
    denied.args(["-c", "printf '%s\\n' 'unshare: write failed /proc/self/uid_map: Operation not permitted' >&2; exit 1", "sh"]);
    let reason = probe_namespace(denied, MOUNT_NAMESPACE).unwrap_err();
    assert!(reason.contains("exit status: 1"), "{reason}");
    assert!(
        reason.contains("write failed /proc/self/uid_map: Operation not permitted"),
        "{reason}"
    );
    let mut unexpected = s.command("/bin/sh");
    unexpected.args(["-c", "exit 77", "sh"]);
    assert!(
        probe_namespace(unexpected, MOUNT_NAMESPACE)
            .unwrap_err()
            .contains("77")
    );
}
#[test]
fn separate_bind_mount_five_thousand_deletes_restore() {
    // 5,000 cross-mount captures plus replayable undo checkpoints are a bulk-I/O
    // stress test. Two-core CI machines need more than the ordinary 10 seconds.
    const BULK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let fixture_started = std::time::Instant::now();
    let (mut s, p) = capture_fixture("bind-delete");
    let no_sync = std::env::var_os("SAQTA_NO_SYNC_WAKE").is_some_and(|v| v == "1");
    if no_sync {
        s.env.insert("SAQTA_NO_SYNC_WAKE".into(), "1".into());
    }
    note(format!(
        "bind-delete settings: no_sync={no_sync}; {}",
        fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find(|l| l.starts_with("Cpus_allowed_list:"))
            .unwrap()
    ));
    if !namespace_available(&s, MOUNT_NAMESPACE, "unprivileged mount namespace") {
        return;
    }
    let data = p.join("files");
    fs::create_dir(&data).unwrap();
    for i in 0..5000 {
        fs::write(data.join(format!("f{i}")), format!("content {i}\n")).unwrap();
    }
    let before = tree_hash(&data);
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let text = "mount --bind \"$1\" \"$1\" || exit 77; \"$2\" exec -- /bin/sh -c 'find \"$1\" -type f -delete' sh \"$3\"";
    let mut c = s.command("unshare");
    c.args([
        "--user",
        "--map-root-user",
        "--mount",
        "/bin/bash",
        "-c",
        text,
        "sh",
    ])
    .arg(&home)
    .arg(BIN)
    .arg(&data);
    let fixture_elapsed = fixture_started.elapsed();
    note(format!("bind-delete fixture: {fixture_elapsed:?}"));
    let start = std::time::Instant::now();
    let o = Running::spawn(c, None).output_with_timeout(BULK_TIMEOUT);
    if o.status.code() == Some(77) {
        note(format!(
            "SKIP: bind mount unavailable: {}",
            String::from_utf8_lossy(&o.stderr)
        ));
        return;
    }
    assert_success(&o);
    let delete_elapsed = start.elapsed();
    note(format!("bind-delete delete: {delete_elapsed:?}"));
    let inspect_started = std::time::Instant::now();
    let id = last_id(&s);
    let r = shown(&s, id);
    assert!(
        r["effects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["op"] == "delete")
            .all(|e| e["note"]["kept"] == "copied (cross-mount)")
    );
    let inspect_elapsed = inspect_started.elapsed();
    let undo_started = std::time::Instant::now();
    assert_success(
        &Running::spawn(s.saqta(&["undo", &id.to_string()]), None)
            .output_with_timeout(BULK_TIMEOUT),
    );
    let undo_elapsed = undo_started.elapsed();
    note(format!("bind-delete undo: {undo_elapsed:?}"));
    let verify_started = std::time::Instant::now();
    assert_tree(&before, &data);
    note(format!(
        "BIND_DELETE_TIMING {}",
        json!({"fixture_s":fixture_elapsed.as_secs_f64(),"delete_s":delete_elapsed.as_secs_f64(),"inspect_s":inspect_elapsed.as_secs_f64(),"undo_s":undo_elapsed.as_secs_f64(),"verify_s":verify_started.elapsed().as_secs_f64(),"no_sync":no_sync})
    ));
    note(format!(
        "5000 cross-mount deletes and restore: {:?}",
        start.elapsed()
    ));
}

#[test]
fn readonly_home_has_one_line_and_runs_command() {
    let s = Sandbox::new("readonly-home");
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    if !namespace_available(
        &s,
        MOUNT_NAMESPACE,
        "unprivileged mount namespace for read-only bind",
    ) {
        return;
    }
    let mut c = s.command("unshare");
    c.args(["--user","--map-root-user","--mount","/bin/bash","-c",r#"mount --bind "$1" "$1" && mount -o remount,bind,ro "$1" || exit 77; "$2" exec -- /bin/sh -c 'exit 7'"#,"sh"]).arg(&home).arg(BIN);
    let o = run(c);
    if o.status.code() == Some(77) {
        note(format!(
            "SKIP: read-only bind mount unavailable: {}",
            String::from_utf8_lossy(&o.stderr)
        ));
        return;
    }
    assert_eq!(
        o.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&o.stderr),
        format!(
            "[saqta] not protected here: {} is read-only (run `saqta init --codex` in this project)\n",
            home.display()
        )
    );
}
#[test]
fn real_hook_shell_cases() {
    let s = Sandbox::new("real-quoting");
    for text in [
        "printf '%s\\n' \"a\\\\b ' $HOME\"; exit 13",
        "cat <<'END'\n$quoted \\ ' \" λ\nEND\n",
        "printf hi; printf error >&2; (exit 0) & wait; exit 4",
    ] {
        let v = payload("Bash", "PreToolUse", &s.dir, text);
        let o = hook(&s, &v, false);
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        let rewritten = v["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        let mut a = s.command("/bin/bash");
        a.args(["-lc", text]);
        let mut b = s.command("/bin/bash");
        b.args(["-lc", rewritten]);
        let a = run(a);
        let b = run(b);
        assert_eq!(
            (a.stdout, a.stderr, a.status.code()),
            (b.stdout, b.stderr, b.status.code())
        );
    }
}
#[test]
fn uninstall_preserves_later_comments() {
    let (s, p) = capture_fixture("uninstall-edits");
    let mut g = s.command("git");
    g.args(["--no-optional-locks", "init", "-q"]).arg(&p);
    assert_success(&run(g));
    fs::create_dir(p.join(".codex")).unwrap();
    fs::write(
        p.join(".codex/config.toml"),
        b"# original\nmodel = 'custom'\n",
    )
    .unwrap();
    assert_success(&init(&s, &p, false));
    let config = p.join(".codex/config.toml");
    let mut b = fs::read(&config).unwrap();
    b.extend_from_slice(b"\n# later note\n");
    fs::write(&config, b).unwrap();
    assert_success(&init(&s, &p, true));
    assert_eq!(
        fs::read(config).unwrap(),
        b"# original\nmodel = 'custom'\n\n# later note\n"
    );
}

#[test]
fn doctor_inside_wrapped_command_checks_actual_capture() {
    let s = Sandbox::new("doctor-wrapped");
    let o = s.run(&["--", BIN, "doctor", "--json"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "inside Codex sandbox" && c["advice"] == "capture works"),
        "doctor omitted the actual capture check: {v}"
    );
    assert!(!s.dir.join(".saqta-doctor").exists());
}

#[test]
fn default_project_tracked_settings_and_closed_output() {
    let (s, p) = capture_fixture("tracked-init");
    let mut g = s.command("git");
    g.args(["--no-optional-locks", "init", "-q"]).arg(&p);
    assert_success(&run(g));
    fs::create_dir_all(p.join(".codex")).unwrap();
    fs::write(p.join(".codex/hooks.json"), b"{}\n").unwrap();
    let mut g = s.command("git");
    g.arg("--no-optional-locks")
        .arg("-C")
        .arg(&p)
        .args(["add", ".codex/hooks.json"]);
    assert_success(&run(g));
    let child = p.join("subdir");
    fs::create_dir(&child).unwrap();
    let mut c = s.saqta(&["init", "--codex"]);
    c.current_dir(&child);
    let o = run(c);
    assert_success(&o);
    assert!(String::from_utf8_lossy(&o.stdout).contains("is tracked; not excluded"));
    assert!(
        !fs::read_to_string(p.join(".git/info/exclude"))
            .unwrap()
            .lines()
            .any(|l| l == ".codex/hooks.json")
    );
    let mut c = s.saqta(&["uninstall", "--codex"]);
    c.current_dir(&child).stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    drop(child.stdout.take());
    assert!(child.wait().unwrap().success());
    assert_eq!(fs::read(p.join(".codex/hooks.json")).unwrap(), b"{}\n");
    let mut c = s.saqta(&["init", "--codex"]);
    c.current_dir(&p).stdout(Stdio::piped());
    let mut child = c.spawn().unwrap();
    drop(child.stdout.take());
    assert!(child.wait().unwrap().success());
    assert_success(&init(&s, &p, true));
}
#[test]
fn raw_garbage_hook_is_silent_and_successful() {
    let s = Sandbox::new("garbage-hook");
    let mut child = s
        .saqta(&["hook", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"not json\xff")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_success(&o);
    assert!(o.stdout.is_empty());
    assert!(o.stderr.is_empty());
}

#[test]
fn uninstall_keeps_new_foreign_hook_bytes() {
    let (s, p) = capture_fixture("new-hook");
    let mut g = s.command("git");
    g.args(["--no-optional-locks", "init", "-q"]).arg(&p);
    assert_success(&run(g));
    assert_success(&init(&s, &p, false));
    let path = p.join(".codex/hooks.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let foreign = json!({"matcher":"foreign","hooks":[{"type":"command","command":"true","extra":"preserve"}]});
    v["hooks"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .insert(0, foreign.clone());
    let current = serde_json::to_vec_pretty(&v).unwrap();
    fs::write(&path, &current).unwrap();
    assert_success(&init(&s, &p, true));
    let after = fs::read(&path).unwrap();
    let v: Value = serde_json::from_slice(&after).unwrap();
    assert_eq!(v["hooks"]["PreToolUse"], json!([foreign]));
    let fragment = serde_json::to_string_pretty(&foreign).unwrap();
    assert!(fragment.contains("preserve"));
}

#[test]
fn doctor_inside_systemd_named_namespace_checks_actual_capture() {
    let s = Sandbox::new("doctor-systemd");
    if !namespace_available(
        &s,
        PID_NAMESPACE,
        "PID namespace for systemd doctor reproduction",
    ) || !python(&s)
    {
        return;
    }
    // Model the VM's /proc/1/comm without altering the host's process name.
    let setup = "import ctypes,subprocess,sys; l=ctypes.CDLL(None,use_errno=True); assert l.prctl(15,ctypes.c_char_p(b'systemd'),0,0,0)==0; sys.exit(subprocess.run([sys.argv[1],'exec','--',sys.argv[1],'doctor','--json']).returncode)";
    let mut c = s.command("unshare");
    c.args(PID_NAMESPACE).args(["python3", "-c", setup, BIN]);
    let o = run(c);
    assert_success(&o);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "inside Codex sandbox" && c["advice"] == "capture works"),
        "doctor skipped capture on systemd host: {v}"
    );
    assert!(!s.dir.join(".saqta-doctor").exists());
}
