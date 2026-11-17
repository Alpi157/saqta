mod common;
use common::*;
use std::{ffi::OsStr, fs, io::Write, panic::AssertUnwindSafe, path::PathBuf, time::Duration};

fn report(s: &Sandbox, running: Running, phase: &str) -> String {
    let pid = running.pid();
    note(format!(
        "TIMEOUT_CONTEXT test_pid={} command_pid={pid} pid_ns={:?} mount_ns={:?} user_ns={:?} debug={:?} no_sync={:?} store={:?}",
        std::process::id(),
        fs::read_link("/proc/self/ns/pid"),
        fs::read_link("/proc/self/ns/mnt"),
        fs::read_link("/proc/self/ns/user"),
        s.env.get(OsStr::new("SAQTA_DEBUG")),
        s.env.get(OsStr::new("SAQTA_NO_SYNC_WAKE")),
        s.env.get(OsStr::new("SAQTA_HOME")),
    ));
    for line in fs::read_to_string("/proc/self/status").unwrap().lines() {
        if [
            "NSpid:",
            "Cpus_allowed_list:",
            "NoNewPrivs:",
            "Seccomp:",
            "Seccomp_filters:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            note(format!("TIMEOUT_CONTEXT {line}"));
        }
    }
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        running.output_after_setup(Duration::from_millis(350))
    }));
    assert!(result.is_err(), "fault injection must hit the timeout");
    let home = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
    let text = fs::read_to_string(home.join(format!("logs/timeout-{pid}.log"))).unwrap();
    assert!(text.contains("elapsed="), "{text}");
    assert!(text.contains("setup="), "{text}");
    assert!(text.contains(&format!("phase={phase}")), "{text}");
    assert!(
        text.contains("pid ppid state wchan cpu_ticks delta cmdline"),
        "{text}"
    );
    // Killing S leaves a stale run marker. Exercise the normal dead-command
    // recovery instead of removing it by hand in the test.
    assert_success(&run(s.saqta(&["log", "--json"])));
    text
}
fn no_live_processes(tag: &str) {
    wait_until(Duration::from_secs(2), || {
        find_processes(tag).into_iter().all(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_none_or(|stat| {
                    stat.rsplit_once(')')
                        .unwrap()
                        .1
                        .trim_start()
                        .starts_with('Z')
                })
        })
    });
}
fn no_sync(s: &mut Sandbox) {
    s.env.insert("SAQTA_NO_SYNC_WAKE".into(), "1".into());
}
fn journal(s: &Sandbox, id: u64) -> PathBuf {
    PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")])
        .join("cmd")
        .join(id.to_string())
}
fn stopped(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .unwrap()
            .1
            .trim_start()
            .starts_with('T')
    })
}

#[test]
fn timeout_distinguishes_an_answering_supervisor_and_kills_the_tree() {
    let (mut s, root) = capture_fixture("timeout-progress");
    no_sync(&mut s);
    let mut command = s.exec(&[
        "--",
        "/bin/sh",
        "-c",
        "while :; do printf x >> victim; sleep .01; done",
        &s.tag,
    ]);
    command.current_dir(root);
    let mut running = Running::spawn(command, None);
    running.wait_ready("two successful notification replies", || {
        notification_counts(&journal(&s, 1)).is_ok_and(|counts| counts[1] >= 2)
    });
    let text = report(&s, running, "process exit");
    assert!(text.contains("YES, answering notifications"), "{text}");
    assert!(text.contains("effects.log"), "{text}");
    assert!(text.contains("outcomes.log"), "{text}");
    no_live_processes(&s.tag);
}

#[test]
fn timeout_identifies_a_stopped_supervisor_and_blocked_tracee() {
    let (mut s, root) = capture_fixture("timeout-stopped");
    no_sync(&mut s);
    let mut command = s.exec(&[
        "--",
        "/bin/sh",
        "-c",
        // Simulate startup exceeding the 350 ms fault window. Ordering still
        // uses the stdin barrier and observed process states, never this sleep.
        "sleep .5; printf ready > ready; read -r release; printf x > victim",
        &s.tag,
    ]);
    command.current_dir(&root);
    let mut running = Running::spawn_with_stdin(command);
    running.wait_ready("tracee ready and notification record published", || {
        root.join("ready").metadata().is_ok_and(|m| m.len() > 0)
            && notification_counts(&journal(&s, 1)).is_ok_and(|counts| counts[1] >= 1)
    });
    let supervisor = s.run_markers()[0];
    signal(supervisor, libc::SIGSTOP);
    running.wait_ready("supervisor stopped", || stopped(supervisor));
    running
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    let child: i32 =
        serde_json::from_slice(&fs::read(journal(&s, 1).join("child")).unwrap()).unwrap();
    running.wait_ready("tracee blocked in seccomp", || {
        fs::read_to_string(format!("/proc/{child}/wchan")).is_ok_and(|s| s.contains("seccomp"))
    });
    let text = report(&s, running, "process exit");
    assert!(text.contains("NO replies observed in sample"), "{text}");
    assert!(text.contains(" T "), "{text}");
    assert!(text.contains("seccomp"), "{text}");
    no_live_processes(&s.tag);
}

#[test]
fn timeout_finds_an_orphan_holding_stdout_after_wrapper_exit() {
    let s = Sandbox::new("timeout-orphan");
    let mut command = s.command("/bin/sh");
    // Clear the environment and reparent the sleeper. Pipe ownership still
    // connects it to the command even without SAQTA_HOME or a living parent.
    command.args([
        "-c",
        "env -i /bin/sh -c 'sleep 30' & echo $! > orphan",
        &s.tag,
    ]);
    let mut running = Running::spawn(command, None);
    let wrapper = running.pid();
    running.wait_ready("wrapper exited with orphan holding stdout", || {
        fs::read_to_string(format!("/proc/{wrapper}/stat"))
            .is_ok_and(|s| s.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
            && s.dir.join("orphan").metadata().is_ok_and(|m| m.len() > 0)
    });
    let text = report(&s, running, "stdout EOF");
    let pid = fs::read_to_string(s.dir.join("orphan")).unwrap();
    assert!(text.contains(&format!("{} ", pid.trim())), "{text}");
    assert!(text.contains("sleep 30"), "{text}");
    assert!(text.contains("wrapper exited"), "{text}");
    wait_until(Duration::from_secs(2), || {
        fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
            .ok()
            .is_none_or(|s| s.rsplit_once(')').unwrap().1.trim_start().starts_with('Z'))
    });
}

#[test]
fn foreign_build_is_rejected_before_fault_injection() {
    let s = Sandbox::new("timeout-build");
    let output = run(s.saqta(&["__harness", "--build-info"]));
    assert_success(&output);
    let foreign = s.dir.join("another-checkout");
    let error = verify_build(&output, &foreign).unwrap_err();
    assert!(error.contains("wrong Saqta test executable"), "{error}");
    assert!(error.contains("another-checkout"), "{error}");
    assert!(error.contains("cargo clean -p saqta"), "{error}");
}

#[test]
fn timeout_reports_the_requested_undos_recovery_state() {
    let (mut s, root) = capture_fixture("timeout-undo");
    no_sync(&mut s);
    fs::write(root.join("victim"), b"original bytes").unwrap();
    let before = tree_hash(&root);
    assert_success(&script(&s, &root, "rm victim"));
    let id = last_id(&s);
    let mut abort = s.saqta(&["undo", &id.to_string()]);
    abort.env("SAQTA_TEST_ABORT_AFTER", "1");
    assert_eq!(run(abort).status.code(), Some(86));
    // The requested undo is deliberately outside the latest three journals.
    for _ in 0..3 {
        assert_success(&s.run(&["--", "/bin/true"]));
    }
    let mut command = s.saqta(&["undo", &id.to_string(), "--resume"]);
    command.env("SAQTA_TEST_STOP_UNDO", "1");
    let mut running = Running::spawn(command, None);
    let pid = running.pid();
    running.wait_ready("undo stopped itself before restoring", || stopped(pid));
    let text = report(&s, running, "process exit");
    assert!(text.contains("undo progress: before=effect=1"), "{text}");
    assert!(text.contains("recovery.log"), "{text}");
    assert!(
        text.contains("command ended; no active listener expected during undo"),
        "{text}"
    );
    assert_success(&run(s.saqta(&["undo", &id.to_string(), "--resume"])));
    assert_tree(&before, &root);
}
