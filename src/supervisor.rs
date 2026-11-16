use crate::{
    capture::Capture,
    exec::{Prepared, fallback},
    filter,
    journal::Journal,
    sys,
};
fn report(fd: i32, status: i32, detached: bool) {
    let mut msg = [0u8; 8];
    msg[..4].copy_from_slice(&status.to_ne_bytes());
    msg[4..].copy_from_slice(&(detached as i32).to_ne_bytes());
    sys::write(fd, &msg);
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
        .get(6..)
        .ok_or_else(|| anyhow::anyhow!("missing command"))?
        .to_vec();
    let command = Prepared::new(argv.clone())?;
    let bpf = filter::program();
    let cs = sys::pair()?;
    let sf = sys::signal_fd(&[libc::SIGCHLD])?;
    let chld = sys::action(libc::SIGCHLD, None)?;
    let journal = match Journal::start(home.clone(), &argv, &std::env::current_dir()?, log_fd) {
        Ok(j) => j,
        Err(_) => {
            report(ws, -2, false);
            return Ok(0);
        }
    };
    let mut capture = match Capture::new(journal) {
        Ok(c) => c,
        Err(_) => {
            report(ws, -1, false);
            return Ok(0);
        }
    };
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
        report(ws, -1, false);
        sys::exit(0);
    }
    sys::disposition(libc::SIGCHLD, libc::SIG_DFL);
    let child = match sys::fork() {
        Ok(p) => p,
        Err(_) => {
            report(ws, -1, false);
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
        ];
        sys::poll(&mut fds, 100);
        // HUP means all filtered tasks are gone. Reap before reporting status.
        let mut hup = listener >= 0 && fds[0].revents & libc::POLLHUP != 0;
        if hup {
            while let Some((pid, st)) = sys::wait(-1, true) {
                capture.complete(pid as u32);
                if pid == child {
                    done = true;
                    status = st;
                }
            }
        } else {
            while sys::signal_event(sf).is_some() {}
            while let Some((pid, st)) = sys::wait(-1, false) {
                capture.complete(pid as u32);
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
            capture.handle(listener, &n);
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
            if hup || sys::parent_pid() == 1 {
                // PID 1 exiting destroys this namespace, including S.
                capture.finish(status);
            } else {
                capture.snapshot();
            }
            if !gone {
                report(ws, status, !hup);
            }
            sent = true;
        }
        if hup {
            if sent && !capture.journal.folder.join("end").exists() {
                capture.finish(status);
            }
            break;
        }
    }
}
