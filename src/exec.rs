use crate::{filter, paths, supervisor, sys};
use anyhow::{Context, Result, bail};
use std::{
    env,
    ffi::{CString, OsString},
    os::unix::ffi::OsStrExt,
};

pub struct Prepared {
    pub program: CString,
    pub argv: Vec<*const libc::c_char>,
    pub env: Vec<*const libc::c_char>,
    shell_argv: Vec<*const libc::c_char>,
    resolution_status: Option<i32>,
    // Own all strings until exec. Pointer arrays never outlive these owners.
    _args: Vec<CString>,
    _env: Vec<CString>,
}
impl Prepared {
    fn new(args: Vec<OsString>) -> Result<Self> {
        let Some(first) = args.first() else {
            bail!("exec needs a program or -c STRING");
        };
        let (program, resolution_status) = resolve(first)?;
        let strings = args
            .iter()
            .map(|a| CString::new(a.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut vars = env::vars_os()
            .filter(|(k, _)| k != "SAQTA_SUPERVISED")
            .map(|(k, v)| {
                let mut b = k.as_bytes().to_vec();
                b.push(b'=');
                b.extend_from_slice(v.as_bytes());
                CString::new(b)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        vars.push(CString::new("SAQTA_SUPERVISED=1")?);
        let mut argv: Vec<_> = strings.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut environ: Vec<_> = vars.iter().map(|a| a.as_ptr()).collect();
        environ.push(std::ptr::null());
        let mut shell_argv = vec![c"/bin/sh".as_ptr(), program.as_ptr()];
        shell_argv.extend_from_slice(&argv[1..]);
        Ok(Self {
            resolution_status,
            shell_argv,
            program,
            argv,
            env: environ,
            _args: strings,
            _env: vars,
        })
    }
    pub fn execute(&self) -> ! {
        if let Some(code) = self.resolution_status {
            sys::write(2, b"saqta: cannot resolve command through PATH\n");
            sys::exit(code);
        }
        sys::exec(&self.program, &self.argv, &self.shell_argv, &self.env)
    }
}
fn resolve(name: &OsString) -> Result<(CString, Option<i32>)> {
    if name.as_bytes().contains(&b'/') {
        return Ok((CString::new(name.as_bytes())?, None));
    }
    let mut denied = false;
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into())) {
        let p = dir.join(name);
        let c = CString::new(p.as_os_str().as_bytes())?;
        if sys::executable(&c) && !p.is_dir() {
            return Ok((c, None));
        }
        denied |= p.exists();
    }
    // Never execute a cwd file unless PATH actually selected it.
    Ok((
        CString::new(name.as_bytes())?,
        Some(if denied { 126 } else { 127 }),
    ))
}
pub fn fallback(p: &Prepared, strict: bool, reason: &[u8]) -> ! {
    sys::write(2, b"[saqta] not protected: ");
    sys::write(2, reason);
    sys::write(2, b"\n");
    if strict {
        sys::exit(125);
    }
    p.execute()
}
pub fn run(m: &clap::ArgMatches) -> Result<i32> {
    let args = if let Some(c) = m.get_one::<OsString>("command") {
        vec![
            m.get_one::<OsString>("shell")
                .cloned()
                .unwrap_or_else(|| "/bin/sh".into()),
            "-c".into(),
            c.clone(),
        ]
    } else {
        m.get_many::<OsString>("argv")
            .map(|a| a.cloned().collect())
            .unwrap_or_default()
    };
    let p = Prepared::new(args)?;
    if env::var_os("SAQTA_SUPERVISED").is_some()
        && std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .any(|s| s.split_whitespace().eq(["Seccomp:", "2"]))
    {
        p.execute();
    }
    let strict = env::var_os("SAQTA_STRICT").is_some_and(|s| s == "1");
    let sync = !env::var_os("SAQTA_NO_SYNC_WAKE").is_some_and(|s| s == "1");
    let prefix = paths::home()
        .ok()
        .and_then(|home| {
            use std::os::unix::fs::DirBuilderExt;
            let run = home.join("run");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&run)
                .ok()?;
            let mut b = run.into_os_string().as_bytes().to_vec();
            b.push(b'/');
            Some(b)
        })
        .unwrap_or_default();
    let log_fd = if env::var_os("SAQTA_DEBUG").is_some_and(|s| s == "1") {
        use std::os::fd::IntoRawFd;
        paths::home()
            .ok()
            .and_then(|h| {
                use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
                let logs = h.join("logs");
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&logs)
                    .ok()?;
                std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .open(logs.join(format!("{}.log", sys::pid())))
                    .ok()
                    .map(|f| f.into_raw_fd())
            })
            .unwrap_or(-1)
    } else {
        -1
    };
    let bpf = filter::program();
    let setup = || -> std::io::Result<_> {
        let ws = sys::pair()?;
        let cs = match sys::pair() {
            Ok(f) => f,
            Err(e) => {
                for f in ws {
                    sys::close(f);
                }
                return Err(e);
            }
        };
        let sf = sys::signal_fd(&[libc::SIGCHLD])?;
        let wf = sys::signal_fd(&[libc::SIGTERM, libc::SIGHUP])?;
        Ok((ws, cs, sf, wf))
    };
    let (ws, cs, sf, wf) = match setup() {
        Ok(s) => s,
        Err(_) => fallback(&p, strict, b"cannot create supervisor resources"),
    };
    let chld = sys::action(libc::SIGCHLD, None).context("read SIGCHLD disposition")?;
    let group = sys::group();
    let s = match sys::fork() {
        Ok(s) => s,
        Err(_) => fallback(&p, strict, b"cannot fork supervisor"),
    };
    if s == 0 {
        sys::close(ws[0]);
        sys::close(wf);
        supervisor::start(
            ws[1], cs, sf, group, &chld, &p, &bpf, &prefix, strict, sync, log_fd,
        );
    }
    sys::close(log_fd);
    sys::close(ws[1]);
    for f in cs {
        sys::close(f);
    }
    sys::close(sf);
    let original_int = sys::action(libc::SIGINT, None)?;
    let original_quit = sys::action(libc::SIGQUIT, None)?;
    sys::disposition(libc::SIGINT, libc::SIG_IGN);
    sys::disposition(libc::SIGQUIT, libc::SIG_IGN);
    sys::block(&[libc::SIGTERM, libc::SIGHUP])?;
    let mut msg = [0u8; 8];
    let mut got = 0;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: ws[0],
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wf,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        sys::poll(&mut fds, -1);
        if fds[1].revents != 0 {
            while let Some(sig) = sys::signal_event(wf) {
                sys::write(ws[0], &[sig as u8]);
            }
        }
        if fds[0].revents != 0 {
            let n = sys::read(ws[0], &mut msg[got..]);
            if n <= 0 {
                bail!("supervisor lost");
            }
            got += n as usize;
            if got == msg.len() {
                break;
            }
        }
    }
    let st = i32::from_ne_bytes(msg[..4].try_into()?);
    let detached = i32::from_ne_bytes(msg[4..].try_into()?);
    sys::close(ws[0]);
    sys::close(wf);
    if detached == 0 {
        sys::wait(s, true);
    }
    if st < 0 {
        let _ = sys::action(libc::SIGINT, Some(&original_int));
        let _ = sys::action(libc::SIGQUIT, Some(&original_quit));
        sys::clear_mask();
        fallback(&p, strict, b"cannot initialize supervisor");
    }
    if libc::WIFEXITED(st) {
        Ok(libc::WEXITSTATUS(st))
    } else if libc::WIFSIGNALED(st) {
        Ok(sys::reraise(libc::WTERMSIG(st)))
    } else {
        bail!("invalid command status")
    }
}
