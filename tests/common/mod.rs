// Each integration test binary uses a different subset of these helpers.
#![allow(dead_code)]
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

pub const BIN: &str = env!("CARGO_BIN_EXE_saqta");
const TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

pub struct Sandbox {
    pub dir: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub tag: String,
}
impl Sandbox {
    pub fn new(name: &str) -> Self {
        let tag = format!(
            "saqta-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/saqta-tests")
            .join(name)
            .join(&tag);
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::create_dir(&dir).unwrap();
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        for key in [
            "SAQTA_SUPERVISED",
            "SAQTA_STRICT",
            "SAQTA_NO_SYNC_WAKE",
            "SAQTA_DEBUG",
        ] {
            env.remove(OsStr::new(key));
        }
        for key in [
            "HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "SAQTA_HOME",
        ] {
            let path = dir.join(key);
            fs::create_dir(&path).unwrap();
            env.insert(key.into(), path.into_os_string());
        }
        let mut search = vec![Path::new(BIN).parent().unwrap().to_owned()];
        search.extend(std::env::split_paths(
            env.get(OsStr::new("PATH"))
                .unwrap_or(&OsString::from("/bin:/usr/bin")),
        ));
        env.insert("PATH".into(), std::env::join_paths(search).unwrap());
        Self { dir, env, tag }
    }
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command.current_dir(&self.dir).env_clear().envs(&self.env);
        command
    }
    pub fn saqta(&self, args: &[&str]) -> Command {
        let mut command = self.command(BIN);
        command.args(args);
        command
    }
    pub fn exec(&self, args: &[&str]) -> Command {
        let mut command = self.saqta(&["exec"]);
        command.args(args);
        command
    }
    pub fn run(&self, args: &[&str]) -> Output {
        run(self.exec(args))
    }
    pub fn spawn(&self, script: &str) -> Running {
        Running::spawn(self.exec(&["-c", script]), None)
    }
    pub fn run_markers(&self) -> Vec<i32> {
        let home = self
            .env
            .get(OsStr::new("SAQTA_HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                self.env
                    .get(OsStr::new("XDG_DATA_HOME"))
                    .map(|p| PathBuf::from(p).join("saqta"))
                    .unwrap_or_else(|| {
                        PathBuf::from(&self.env[OsStr::new("HOME")]).join(".local/share/saqta")
                    })
            });
        fs::read_dir(home.join("run"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    }
    pub fn wait_file(&self, file: &str) {
        wait_until(Duration::from_secs(3), || {
            self.dir.join(file).metadata().is_ok_and(|m| m.len() > 0)
        });
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        if !thread::panicking() {
            wait_until(Duration::from_secs(3), || self.run_markers().is_empty());
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }
}

pub fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition did not become true within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
pub fn find_processes(tag: &str) -> Vec<i32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
            cmdline
                .split(|&b| b == 0)
                .any(|arg| arg == tag.as_bytes())
                .then_some(pid)
        })
        .collect()
}
pub fn process_group(pid: i32) -> i32 {
    let stat = fs::read(format!("/proc/{pid}/stat")).unwrap();
    let end = stat.iter().rposition(|&b| b == b')').unwrap();
    std::str::from_utf8(&stat[end + 2..])
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap()
}
pub fn signal(pid: i32, sig: i32) {
    // SAFETY: callers pass a pid owned by this test, or its explicitly created process group.
    let result = unsafe { libc::kill(pid, sig) };
    assert_eq!(
        result,
        0,
        "kill({pid}, {sig}): {}",
        std::io::Error::last_os_error()
    );
}
fn reader(mut stream: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let _ = tx.send(bytes);
    });
    rx
}
// Drain both pipes concurrently, and bound both process wait and pipe EOF waits.
// Dropping a running wrapper kills it so S cleans up its descendants on test failure.
pub struct Running {
    pub child: Child,
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
}
impl Running {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
    pub fn spawn(mut command: Command, input: Option<&[u8]>) -> Self {
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = reader(child.stdout.take().unwrap());
        let stderr = reader(child.stderr.take().unwrap());
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        Self {
            child,
            stdout,
            stderr,
        }
    }
    pub fn output(mut self) -> Output {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "command {} timed out",
                self.pid()
            );
            thread::sleep(Duration::from_millis(2));
        };
        let stdout = self
            .stdout
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("stdout never reached EOF");
        let stderr = self
            .stderr
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("stderr never reached EOF");
        Output {
            status,
            stdout,
            stderr,
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
pub fn run(command: Command) -> Output {
    Running::spawn(command, None).output()
}
pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
pub fn bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

// Write directly so successful-test capture cannot hide skips or measurements.
pub fn note(message: impl std::fmt::Display) {
    writeln!(std::io::stdout(), "{message}").unwrap();
}
