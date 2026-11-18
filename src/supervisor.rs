use crate::{
    capture::Capture,
    classify,
    exec::{Prepared, fallback},
    filter, hold,
    journal::Journal,
    sys,
};
use std::{
    collections::HashSet,
    ffi::{CStr, CString, OsStr},
    fs, io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

fn proc_directory(path: &CStr, flags: i32) -> io::Result<std::os::fd::OwnedFd> {
    sys::open_at(
        libc::AT_FDCWD,
        path,
        libc::O_PATH | libc::O_DIRECTORY | flags,
        0,
    )
}

fn proc_parent(directory: &Path) -> io::Result<u32> {
    let status = fs::read(directory.join("status"))?;
    status
        .split(|&b| b == b'\n')
        .find_map(|line| {
            std::str::from_utf8(line.strip_prefix(b"PPid:")?)
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .ok_or_else(|| io::Error::other("missing or invalid PPid in proc status"))
}

/// A subreaper remains an ancestor after its command daemonizes. Any failed
/// proc read leaves release unable to establish that it is outside that tree.
pub fn in_ancestry() -> io::Result<bool> {
    let mut task = proc_directory(c"/proc/self", 0)?;
    let executable = fs::metadata("/proc/self/exe")?;
    let directory = |fd| PathBuf::from(format!("/proc/self/fd/{fd}"));
    let mut parent = proc_parent(&directory(task.as_raw_fd()))?;
    let mut seen = HashSet::new();
    while parent != 0 {
        if !seen.insert(parent) {
            return Err(io::Error::other("cyclic proc ancestry"));
        }
        // Pin the proc directory so subsequent reads cannot follow a reused PID.
        let path = CString::new(format!("/proc/{parent}"))?;
        let ancestor = proc_directory(&path, libc::O_NOFOLLOW)?;
        let proc = directory(ancestor.as_raw_fd());
        let arguments = fs::read(proc.join("cmdline"))?;
        if arguments.split(|&b| b == 0).nth(1) == Some(b"__supervisor") {
            let link = proc.join("exe");
            let image = fs::read_link(&link)?;
            if matches!(image.file_name(), Some(name) if name == OsStr::new("saqta") || name == OsStr::new("saqta (deleted)"))
            {
                return Ok(true);
            }
            let image = fs::metadata(link)?;
            if (image.dev(), image.ino()) == (executable.dev(), executable.ino()) {
                return Ok(true);
            }
        }
        let next = proc_parent(&proc)?;
        if proc_parent(&directory(task.as_raw_fd()))? != parent {
            return Err(io::Error::other("process reparented during release check"));
        }
        task = ancestor;
        parent = next;
    }
    Ok(false)
}

fn report(fd: i32, status: i32, detached: bool, message: &str) {
    let mut msg = [0u8; 12];
    msg[..4].copy_from_slice(&status.to_ne_bytes());
    msg[4..8].copy_from_slice(&(detached as i32).to_ne_bytes());
    msg[8..].copy_from_slice(&(message.len() as u32).to_ne_bytes());
    sys::write(fd, &msg);
    sys::write(fd, message.as_bytes());
}
// W re-execs this entry before any dynamic supervisor work. Only C takes the
// restricted post-fork path; S is now a fresh single-threaded Rust process.
pub fn internal(args: Vec<std::ffi::OsString>) -> anyhow::Result<i32> {
    use std::os::unix::ffi::OsStrExt;
    let number = |i: usize| -> anyhow::Result<i32> {
        Ok(args
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("missing supervisor argument"))?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid supervisor argument"))?
            .parse()?)
    };
    let ws = number(0)?;
    let group = number(1)?;
    let strict = number(2)? != 0;
    let sync = number(3)? != 0;
    let log_fd = number(4)?;
    sys::debug_timestamp(log_fd, b"reexec_end");
    sys::inherit(ws, false)?;
    if log_fd >= 0 {
        sys::inherit(log_fd, false)?;
    }
    let home = std::path::PathBuf::from(
        args.get(5)
            .ok_or_else(|| anyhow::anyhow!("missing supervisor home"))?,
    );
    let argv = args
        .get(7..)
        .ok_or_else(|| anyhow::anyhow!("missing command"))?
        .to_vec();
    let cwd = std::env::current_dir()?;
    let session = args
        .get(6)
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let command = Prepared::new(argv.clone())?;
    let bpf = filter::program();
    let cs = sys::pair()?;
    let sf = sys::signal_fd(&[libc::SIGCHLD])?;
    let chld = sys::action(libc::SIGCHLD, None)?;
    let journal = match Journal::start_session(home.clone(), &argv, &cwd, log_fd, session.clone()) {
        Ok(j) => j,
        Err(_) => {
            report(ws, -2, false, "");
            return Ok(0);
        }
    };
    let mut capture = match Capture::new(journal) {
        Ok(c) => c,
        Err(_) => {
            report(ws, -1, false, "");
            return Ok(0);
        }
    };
    if std::env::var_os("SAQTA_RULES").is_none_or(|s| s != "off") {
        let (policy, errors) = crate::rules::Policy::load(&cwd);
        capture.policy = Some(policy);
        for error in errors {
            sys::write(log_fd, format!("rules configuration: {error}\n").as_bytes());
        }
        if let Some(session) = &session {
            match hold::prior(&home, session, capture.journal.meta.id) {
                Ok(created) => capture.prior_created = created,
                Err(error) => {
                    sys::write(log_fd, format!("session history: {error}\n").as_bytes());
                }
            }
        }
    }
    let mut prefix = home.join("run").into_os_string().as_bytes().to_vec();
    prefix.push(b'/');
    start(
        ws,
        cs,
        sf,
        group,
        &chld,
        &command,
        &bpf,
        &prefix,
        strict,
        sync,
        log_fd,
        &mut capture,
    )
}
#[allow(clippy::too_many_arguments)]
fn start(
    ws: i32,
    cs: [i32; 2],
    sf: i32,
    group: i32,
    chld: &libc::sigaction,
    command: &Prepared,
    bpf: &[libc::sock_filter],
    prefix: &[u8],
    strict: bool,
    sync: bool,
    log_fd: i32,
    capture: &mut Capture,
) -> ! {
    sys::debug_timestamp(log_fd, b"handoff_begin");
    if sys::subreaper().is_err()
        || sys::set_group(0).is_err()
        || sys::block(&[libc::SIGCHLD]).is_err()
    {
        report(ws, -1, false, "");
        sys::exit(0);
    }
    sys::disposition(libc::SIGCHLD, libc::SIG_DFL);
    let child = match sys::fork() {
        Ok(p) => p,
        Err(_) => {
            report(ws, -1, false, "");
            sys::exit(0);
        }
    };
    if child == 0 {
        sys::close(log_fd);
        sys::close(ws);
        sys::close(sf);
        sys::close(cs[0]);
        if sys::set_group(group).is_err() {
            sys::exit(125);
        }
        let _ = sys::action(libc::SIGCHLD, Some(chld));
        sys::disposition(libc::SIGPIPE, libc::SIG_DFL);
        sys::clear_mask();
        match sys::install(bpf) {
            Ok(fd) => {
                if sys::pass_fd(cs[1], Some(fd)) < 0 {
                    sys::exit(125);
                }
                sys::close(fd);
                sys::close(cs[1]);
                command.execute();
            }
            Err(e) => {
                sys::close(cs[1]);
                let reason: &[u8] = if e.raw_os_error() == Some(libc::EBUSY) {
                    b"another seccomp supervisor is already active"
                } else {
                    b"seccomp listener is unavailable"
                };
                fallback(command, strict, reason);
            }
        }
    }
    sys::close(cs[1]);
    // Close caller's streams immediately, including while waiting for the listener.
    sys::detach_stdio();
    if !prefix.is_empty() {
        sys::marker(prefix, true);
    }
    if let Err(e) = capture.journal.child(child) {
        capture.journal.error(&e);
    }
    let listener = sys::pass_fd(cs[0], None);
    sys::close(cs[0]);
    if sync && listener >= 0 {
        sys::sync_wake(listener);
    }
    sys::debug_timestamp(log_fd, b"handoff_end");
    sys::write(log_fd, b"supervisor: listener handoff complete\n");
    serve(ws, sf, listener, child, log_fd, capture);
    sys::write(log_fd, b"supervisor: exiting\n");
    if !prefix.is_empty() {
        sys::marker(prefix, false);
    }
    sys::exit(0)
}
fn serve(ws: i32, sf: i32, listener: i32, child: i32, log_fd: i32, capture: &mut Capture) {
    let (mut done, mut status, mut sent, mut gone) = (false, 0, false, false);
    let mut deadline = None;
    let mut killed = false;
    let mut foreground = 0;
    let (mut received, mut answered) = (0u64, 0u64);
    loop {
        let mut fds = [
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sf,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if gone { -1 } else { ws },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: capture.task_exit_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        sys::poll(&mut fds, 100);
        if fds[0].revents == 0 || fds[3].revents != 0 {
            capture.prune_tasks();
        }
        // HUP means all filtered tasks are gone. Reap before reporting status.
        let mut hup = listener >= 0 && fds[0].revents & libc::POLLHUP != 0;
        if hup {
            while let Some((pid, st)) = sys::wait(-1, true) {
                capture.exited(pid as u32);
                if pid == child {
                    done = true;
                    status = st;
                }
            }
        } else if fds[1].revents != 0 {
            while sys::signal_event(sf).is_some() {}
            while let Some((pid, st)) = sys::wait(-1, false) {
                capture.exited(pid as u32);
                if pid == child {
                    done = true;
                    status = st;
                }
            }
        }
        if fds[2].revents != 0 {
            let mut b = [0u8; 32];
            let n = sys::read(ws, &mut b);
            if n <= 0 {
                gone = true;
                if !done {
                    sys::kill_tree();
                }
            } else {
                for &sig in &b[..n as usize] {
                    if sig == libc::SIGTERM as u8 || sig == libc::SIGHUP as u8 {
                        sys::tree_signal(sig as i32);
                        deadline = Some(sys::monotonic_ms() + 2000);
                    }
                }
            }
        }
        if deadline.is_some_and(|d| sys::monotonic_ms() >= d) {
            sys::kill_tree();
            deadline = None;
        }
        if !hup
            && fds[0].revents & libc::POLLIN != 0
            && let Ok(n) = sys::receive(listener)
        {
            received += 1;
            capture
                .journal
                .notification_progress(received, answered, n.id, n.pid);
            answered += u64::from(capture.handle(listener, &n));
            capture
                .journal
                .notification_progress(received, answered, n.id, n.pid);
        }
        if done && !hup && capture.policy.is_some() {
            capture.observe_pending();
        }
        if fds[0].revents == 0 {
            // Flush a potentially decisive Git batch at an idle poll so a
            // sleeping tracee cannot postpone the breaker after its last delete.
            capture.check_git_deletions(true);
        }
        if !hup && !killed && capture.tripped() {
            sys::kill_tree();
            killed = true;
        }
        if done && !sent {
            if listener < 0 {
                hup = true;
            }
            if !hup {
                let mut p = [libc::pollfd {
                    fd: listener,
                    events: 0,
                    revents: 0,
                }];
                sys::poll(&mut p, 0);
                hup = p[0].revents & libc::POLLHUP != 0;
                if hup {
                    while sys::wait(-1, true).is_some() {}
                }
            }
            sys::write(
                log_fd,
                if hup {
                    b"supervisor: command done\n"
                } else {
                    b"supervisor: background tasks remain\n"
                },
            );
            let final_now = hup || killed || sys::parent_pid() == 1;
            if final_now && !hup {
                // PID 1 exiting would destroy the remaining tasks and S.
                sys::kill_tree();
                while let Some((pid, _)) = sys::wait(-1, true) {
                    capture.exited(pid as u32);
                }
                hup = true;
            }
            if !final_now {
                foreground = capture.journal.next_seq() - 1;
                if capture.policy.is_some() {
                    capture.snapshot();
                }
            }
            let (reported, message) = if !final_now
                && capture.policy.is_some()
                && assess(capture, 0).is_ok_and(|assessment| {
                    assessment.is_some_and(|(_, decision)| decision.rule().is_some())
                }) {
                sys::freeze_tree();
                let result = match capture.background() {
                    Ok(mut background) => {
                        // Keep both journal phases under the same supervisor. The
                        // foreground post-state is sealed before the tree resumes.
                        let result = settle(capture, status, true, false, 0);
                        background.breaker = std::mem::take(&mut capture.breaker);
                        background.indexes = std::mem::take(&mut capture.indexes);
                        *capture = background;
                        foreground = 0;
                        result
                    }
                    Err(error) => {
                        let _ = std::fs::write(
                            capture.journal.folder.join("rules-error"),
                            format!("{error:#}"),
                        );
                        settle(capture, status, false, false, 0)
                    }
                };
                sys::tree_signal(libc::SIGCONT);
                result
            } else {
                settle(capture, status, final_now, false, 0)
            };
            if !gone {
                report(ws, reported, !hup, &message);
            }
            sent = true;
        }
        if hup {
            if sent && !capture.journal.folder.join("end").exists() {
                settle(capture, status, true, true, foreground);
            }
            break;
        }
    }
}

fn settle(
    capture: &mut Capture,
    status: i32,
    finish: bool,
    detached: bool,
    after: u64,
) -> (i32, String) {
    if finish {
        capture.settle();
    }
    if capture.policy.is_none() {
        if finish {
            capture.finish(status);
        } else {
            capture.snapshot();
        }
        return (status, String::new());
    }
    capture.snapshot();
    let result = evaluate(capture, finish, detached, after);
    let (status, message) = match result {
        Ok(Some((true, text))) => (77 << 8, text),
        Ok(Some((false, text))) => (status, text),
        Ok(None) => (status, String::new()),
        Err(e) => {
            let j = &capture.journal;
            let _ = std::fs::write(j.folder.join("rules-error"), format!("{e:#}"));
            let id = j.meta.id;
            (
                125 << 8,
                format!(
                    "[saqta {id}] Automatic protection could not finish. See: saqta show {id}\n"
                ),
            )
        }
    };
    if capture.journal.meta.session.is_some() {
        let created: std::collections::HashSet<_> = capture
            .created_paths()
            .filter_map(classify::Birth::read)
            .collect();
        if !created.is_empty() {
            let _ = crate::journal::json_write(&capture.journal.folder.join("created"), &created);
        }
    }
    if finish && let Err(e) = capture.journal.end(status) {
        capture.journal.error(&e);
    }
    (status, message)
}

fn assess(
    capture: &Capture,
    after: u64,
) -> anyhow::Result<Option<(crate::journal::Record, classify::Decision)>> {
    let Some(policy) = &capture.policy else {
        return Ok(None);
    };
    if !capture.needs_rule_evaluation(after) {
        return Ok(None);
    }
    let j = &capture.journal;
    let record = crate::journal::load(&j.home, j.meta.id)?;
    let mut decision = classify::evaluate(
        &record,
        &j.folder,
        &j.quarantine,
        policy,
        &capture.indexes,
        capture.tripped().then_some(&capture.breaker),
        after,
    )?;
    if capture.tripped() {
        decision.interrupted_locks = capture.interrupted_git_locks()?;
    }
    Ok(Some((record, decision)))
}

fn evaluate(
    capture: &Capture,
    finish: bool,
    detached: bool,
    after: u64,
) -> anyhow::Result<Option<(bool, String)>> {
    let Some(policy) = &capture.policy else {
        return Ok(None);
    };
    let j = &capture.journal;
    let after = if j.folder.join("defer-rules").exists() {
        0
    } else {
        after
    };
    let Some((record, decision)) = assess(capture, after)? else {
        return Ok(None);
    };
    let id = j.meta.id;
    if let Some(rule) = decision.rule() {
        if finish {
            let mut message = rule.message(
                id,
                &decision.names(),
                &policy.project,
                decision.groups.len() > 1,
                capture.breaker.mostly_dirty(),
            );
            if record
                .effects
                .iter()
                .any(|e| e.op == "not protected" && record.outcomes.get(&e.seq) != Some(&false))
            {
                message.push_str(&format!(
                    "  Some effects could not be protected. See: saqta show {id}\n"
                ));
            }
            hold::start(j, record, &decision, detached, after)?;
            return Ok(Some((true, message)));
        }
        // A live background process can still modify these paths. Defer the
        // restore until its final post-state has been captured.
        crate::journal::json_write(&j.folder.join("defer-rules"), &true)?;
        return Ok(None);
    }
    if decision.valuable != 0 && !detached {
        let count = decision.valuable;
        let noun = if count == 1 { "file" } else { "files" };
        return Ok(Some((
            false,
            format!("[saqta {id}] kept {count} edited {noun} (undo: saqta undo {id})\n"),
        )));
    }
    Ok(None)
}
