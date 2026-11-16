mod common;
use common::*;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    time::{Duration, Instant},
};

#[test]
fn exit_codes_pass_through() {
    let s = Sandbox::new("exit_codes_pass_through");
    for code in [0, 3, 255] {
        assert_eq!(
            s.run(&["-c", &format!("exit {code}")]).status.code(),
            Some(code)
        );
    }
}
#[test]
fn sigterm_is_reraised() {
    let s = Sandbox::new("sigterm_is_reraised");
    assert_eq!(
        s.run(&["-c", "kill -TERM $$"]).status.signal(),
        Some(libc::SIGTERM)
    );
}
#[test]
fn sigsegv_is_reraised_without_core() {
    let s = Sandbox::new("sigsegv_is_reraised_without_core");
    assert_eq!(
        s.run(&["-c", "kill -SEGV $$"]).status.signal(),
        Some(libc::SIGSEGV)
    );
    assert!(!fs::read_dir(&s.dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .as_encoded_bytes()
            .starts_with(b"core")
    }));
}
#[test]
fn stdin_and_stderr_pass_through() {
    let s = Sandbox::new("stdin_and_stderr_pass_through");
    let o = Running::spawn(s.exec(&["-c", "tr a-z A-Z; echo err >&2"]), Some(b"hello")).output();
    assert_success(&o);
    assert_eq!(o.stdout, b"HELLO");
    assert_eq!(o.stderr, b"err\n");
}
#[test]
fn shell_syntax_matches_sh() {
    let s = Sandbox::new("shell_syntax_matches_sh");
    fs::write(s.dir.join("a.txt"), "").unwrap();
    let script = "v='two words'; printf '%s\\n' \"$v\" 'quote\"' *.txt | tr a-z A-Z > out\ncat out && printf end; printf '\\nnext\\n'";
    let mut bare = s.command("/bin/sh");
    bare.args(["-c", script]);
    let bare = run(bare);
    let o = s.run(&["-c", script]);
    assert_eq!(
        (o.status.code(), o.stdout, o.stderr),
        (bare.status.code(), bare.stdout, bare.stderr)
    );
    assert_eq!(
        s.run(&["--shell", "/bin/sh", "-c", "echo shell"]).stdout,
        b"shell\n"
    );
}
#[test]
fn argv_preserves_special_and_empty_arguments() {
    let s = Sandbox::new("argv_preserves_special_and_empty_arguments");
    let args = ["space here", "quote\"'here", "$", "*", ""];
    let mut cmd = s.exec(&["--", "printf", "%s\\0"]);
    cmd.args(args);
    let o = run(cmd);
    assert_success(&o);
    let expected: Vec<u8> = args
        .iter()
        .flat_map(|a| a.as_bytes().iter().copied().chain([0]))
        .collect();
    assert_eq!(o.stdout, expected);
}
#[test]
fn cwd_and_environment_are_preserved() {
    let s = Sandbox::new("cwd_and_environment_are_preserved");
    let o = s.run(&["--", "pwd"]);
    assert_success(&o);
    assert_eq!(o.stdout, [bytes(&s.dir), b"\n"].concat());
    let o = s.run(&["--", "env", "-0"]);
    assert_success(&o);
    let actual: BTreeMap<_, _> = o
        .stdout
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| {
            let at = a.iter().position(|&b| b == b'=').unwrap();
            (
                std::ffi::OsString::from_vec(a[..at].to_vec()),
                std::ffi::OsString::from_vec(a[at + 1..].to_vec()),
            )
        })
        .collect();
    let mut expected = s.env.clone();
    expected.insert("SAQTA_SUPERVISED".into(), "1".into());
    assert_eq!(actual, expected);
}
#[test]
fn background_job_is_served_after_wrapper_returns() {
    let s = Sandbox::new("background_job_is_served_after_wrapper_returns");
    fs::write(s.dir.join("victim"), "saved until background unlink").unwrap();
    let start = Instant::now();
    let o = s.run(&[
        "-c",
        "sh -c 'sleep 1; rm victim' >/dev/null 2>&1 & echo started",
    ]);
    let elapsed = start.elapsed();
    note(format!("background return: {elapsed:?}"));
    assert_success(&o);
    assert_eq!(o.stdout, b"started\n");
    assert!(elapsed < Duration::from_millis(500));
    wait_until(Duration::from_secs(2), || !s.dir.join("victim").exists());
    wait_until(Duration::from_secs(2), || s.run_markers().is_empty());
}
#[test]
fn supervisor_releases_stdout_pipe() {
    let s = Sandbox::new("supervisor_releases_stdout_pipe");
    let start = Instant::now();
    let o = s.run(&["-c", "sleep 0.2; echo hi"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"hi\n");
    assert!(start.elapsed() < Duration::from_secs(2));
}
#[test]
fn wrapper_kill_stops_setsid_grandchild() {
    let s = Sandbox::new("wrapper_kill_stops_setsid_grandchild");
    // Unique decimal sleep arguments are exact cmdline tags, with no process-name matching.
    let a = format!(
        "307.{}",
        s.tag
            .bytes()
            .fold(0u64, |n, b| n.wrapping_mul(31).wrapping_add(b as u64))
    );
    let b = a.replacen("307.", "308.", 1);
    let p = s.spawn(&format!("setsid sh -c 'sleep {a}' & sleep {b}"));
    wait_until(Duration::from_secs(3), || {
        !find_processes(&a).is_empty() && !find_processes(&b).is_empty()
    });
    signal(p.pid(), libc::SIGKILL);
    let o = p.output();
    assert_eq!(o.status.signal(), Some(libc::SIGKILL));
    wait_until(Duration::from_secs(3), || {
        find_processes(&a).is_empty() && find_processes(&b).is_empty()
    });
    wait_until(Duration::from_secs(3), || s.run_markers().is_empty());
}
#[test]
fn term_reaches_trap_and_preserves_its_exit_code() {
    let s = Sandbox::new("term_reaches_trap_and_preserves_its_exit_code");
    let p = s.spawn(
        "trap 'echo term > marker; exit 42' TERM; echo ready > ready; while :; do sleep 1; done",
    );
    s.wait_file("ready");
    signal(p.pid(), libc::SIGTERM);
    let o = p.output();
    assert_eq!(o.status.code(), Some(42));
    assert!(s.dir.join("marker").exists());
}
#[test]
fn term_kills_untrapped_command() {
    let s = Sandbox::new("term_kills_untrapped_command");
    let p = s.spawn("echo ready > ready; exec sleep 30");
    s.wait_file("ready");
    signal(p.pid(), libc::SIGTERM);
    assert_eq!(p.output().status.signal(), Some(libc::SIGTERM));
}
#[test]
fn ignored_term_escalates_to_kill() {
    let s = Sandbox::new("ignored_term_escalates_to_kill");
    let p = s.spawn("trap '' TERM; echo ready > ready; exec sleep 30");
    s.wait_file("ready");
    signal(p.pid(), libc::SIGTERM);
    assert_eq!(p.output().status.signal(), Some(libc::SIGKILL));
}
#[test]
fn command_shares_wrapper_group_and_supervisor_has_own_group() {
    let s = Sandbox::new("command_shares_wrapper_group_and_supervisor_has_own_group");
    let p = s.spawn("echo $$ > command-pid; exec sleep 30");
    s.wait_file("command-pid");
    let child = fs::read_to_string(s.dir.join("command-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until(Duration::from_secs(3), || !s.run_markers().is_empty());
    let supervisor = s.run_markers()[0];
    assert_eq!(process_group(child), process_group(p.pid()));
    assert_ne!(process_group(supervisor), process_group(p.pid()));
    signal(p.pid(), libc::SIGTERM);
    p.output();
}
#[test]
fn nested_exec_is_silent() {
    let s = Sandbox::new("nested_exec_is_silent");
    let o = s.run(&["-c", "saqta exec -c 'echo inner'"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"inner\n");
    assert!(o.stderr.is_empty());
}
#[test]
fn missing_nesting_marker_warns_or_fails_strictly() {
    let s = Sandbox::new("missing_nesting_marker_warns_or_fails_strictly");
    for strict in [false, true] {
        let script = format!(
            "env -u SAQTA_SUPERVISED {}saqta exec -c 'echo inner'",
            if strict { "SAQTA_STRICT=1 " } else { "" }
        );
        let o = s.run(&["-c", &script]);
        assert!(
            String::from_utf8_lossy(&o.stderr)
                .contains("[saqta] not protected: another seccomp supervisor is already active")
        );
        assert_eq!(o.status.code(), Some(if strict { 125 } else { 0 }));
        assert_eq!(o.stdout, if strict { &b""[..] } else { &b"inner\n"[..] });
    }
}
#[test]
fn parallel_commands_leave_no_supervisors() {
    let s = Sandbox::new("parallel_commands_leave_no_supervisors");
    let children: Vec<_> = (0..20)
        .map(|i| s.spawn(&format!("echo hi > file{i}; rm file{i}")))
        .collect();
    for child in children {
        assert_success(&child.output());
    }
    wait_until(Duration::from_secs(3), || s.run_markers().is_empty());
}
#[test]
fn overhead_p95_is_below_fifty_milliseconds() {
    let s = Sandbox::new("overhead_p95_is_below_fifty_milliseconds");
    let (mut bare, mut wrapped) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        let start = Instant::now();
        assert_success(&run(s.command("true")));
        bare.push(start.elapsed());
        let start = Instant::now();
        assert_success(&s.run(&["--", "true"]));
        wrapped.push(start.elapsed());
    }
    bare.sort();
    wrapped.sort();
    note(format!(
        "200 samples: direct median {:?}, p95 {:?}; saqta median {:?}, p95 {:?}",
        bare[100], bare[189], wrapped[100], wrapped[189]
    ));
    assert!(wrapped[189] < Duration::from_millis(50));
}
#[test]
fn sigpipe_is_default_in_command() {
    let s = Sandbox::new("sigpipe_is_default_in_command");
    let o = s.run(&["-c", "yes | head -1"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"y\n");
    assert!(o.stderr.is_empty());
}
#[test]
fn group_sigint_reaches_command_and_wrapper() {
    let s = Sandbox::new("group_sigint_reaches_command_and_wrapper");
    let mut command = s.exec(&["-c", "echo ready > ready; exec sleep 30"]);
    command.process_group(0);
    let p = Running::spawn(command, None);
    s.wait_file("ready");
    signal(-p.pid(), libc::SIGINT);
    assert_eq!(p.output().status.signal(), Some(libc::SIGINT));
}
#[test]
fn sync_wake_can_be_disabled() {
    let mut s = Sandbox::new("sync_wake_can_be_disabled");
    s.env.insert("SAQTA_NO_SYNC_WAKE".into(), "1".into());
    assert_success(&s.run(&["-c", "touch x; rm x"]));
}
#[test]
fn unwritable_home_does_not_prevent_exec() {
    let mut s = Sandbox::new("unwritable_home_does_not_prevent_exec");
    // A regular file inside the test directory makes this path impossible to create, even as root.
    fs::write(s.dir.join("not-a-directory"), "").unwrap();
    s.env.insert(
        "SAQTA_HOME".into(),
        s.dir.join("not-a-directory/home").into_os_string(),
    );
    let o = s.run(&["-c", "echo working"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"working\n");
}
#[test]
fn text_executable_uses_shell_and_path_does_not_search_cwd() {
    let s = Sandbox::new("text_executable_uses_shell_and_path_does_not_search_cwd");
    let script = s.dir.join("executable");
    fs::write(&script, "echo text-script \"$1\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let o = s.run(&["--", "./executable", "arg"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"text-script arg\n");
    assert_eq!(s.run(&["--", "executable"]).status.code(), Some(127));
    fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(s.run(&["--", "./executable"]).status.code(), Some(126));
}
#[test]
fn inherited_ignored_sigchld_preserves_command_status() {
    let s = Sandbox::new("inherited_ignored_sigchld_preserves_command_status");
    let mut command = s.exec(&["-c", "exit 3"]);
    // SAFETY: pre_exec runs only an async-signal-safe disposition change in the child.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    assert_eq!(run(command).status.code(), Some(3));
}
#[test]
fn resource_exhaustion_fails_open_or_strictly() {
    let mut s = Sandbox::new("resource_exhaustion_fails_open_or_strictly");
    for strict in [false, true] {
        s.env
            .insert("SAQTA_STRICT".into(), if strict { "1" } else { "0" }.into());
        let mut command = s.exec(&["--", "/bin/echo", "fallback"]);
        // SAFETY: pre_exec only passes a stack rlimit to async-signal-safe setrlimit.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 4,
                    rlim_max: 4,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let o = run(command);
        assert_eq!(o.status.code(), Some(if strict { 125 } else { 0 }));
        assert!(String::from_utf8_lossy(&o.stderr).contains("[saqta] not protected:"));
        assert_eq!(o.stdout, if strict { &b""[..] } else { &b"fallback\n"[..] });
    }
}
#[test]
fn debug_logging_creates_log_file() {
    let mut s = Sandbox::new("debug_logging_creates_log_file");
    s.env.insert("SAQTA_DEBUG".into(), "1".into());
    assert_success(&s.run(&["--", "true"]));
    let logs = std::path::Path::new(&s.env[OsStr::new("SAQTA_HOME")]).join("logs");
    assert!(
        fs::read_dir(logs)
            .unwrap()
            .any(|e| e.unwrap().path().extension() == Some(OsStr::new("log")))
    );
}
