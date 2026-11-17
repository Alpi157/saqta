//! Bounded, read-only diagnostics before killing a timed-out test's processes.
use super::test_support;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Process {
    pid: i32,
    parent: i32,
    start: u64,
    ticks: u64,
    state: String,
    wchan: String,
    args: Vec<u8>,
}
fn process(pid: i32) -> Option<Process> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
    Some(Process {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
        ticks: fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?,
        state: fields.first()?.to_string(),
        wchan: fs::read_to_string(format!("/proc/{pid}/wchan")).unwrap_or_default(),
        args: fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default(),
    })
}

pub struct Watch {
    root: i32,
    start: u64,
    pub home: Option<PathBuf>,
    command: String,
    undo_id: Option<u64>,
    pipes: Vec<PathBuf>,
}
impl Watch {
    pub fn home(command: &Command) -> Option<PathBuf> {
        let variable = |name| {
            command.get_envs().find_map(|(key, value)| {
                (key == name).then_some(value).flatten().map(PathBuf::from)
            })
        };
        let home = variable("SAQTA_HOME")
            .or_else(|| variable("XDG_DATA_HOME").map(|p| p.join("saqta")))
            .or_else(|| variable("HOME").map(|p| p.join(".local/share/saqta")))?;
        // Diagnostics must never write into a real user's home.
        home.starts_with(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/saqta-tests"))
            .then_some(home)
    }
    pub fn new(command: &Command, pid: i32, pipes: Vec<PathBuf>) -> Self {
        Self {
            root: pid,
            start: process(pid).map_or(0, |p| p.start),
            home: Self::home(command),
            // Command's Debug also prints its environment. Never dump that.
            command: format!(
                "{:?} {:?}",
                command.get_program(),
                command.get_args().collect::<Vec<_>>()
            ),
            undo_id: {
                let mut args = command.get_args();
                (args.next() == Some(OsStr::new("undo")))
                    .then(|| args.next()?.to_str()?.parse().ok())
                    .flatten()
            },
            pipes,
        }
    }
    fn tree(&self) -> BTreeMap<i32, Process> {
        let all: BTreeMap<_, _> = fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .filter_map(|e| process(e.ok()?.file_name().to_str()?.parse().ok()?))
            .map(|p| (p.pid, p))
            .collect();
        let mut owned = BTreeMap::new();
        for p in all.values() {
            if p.pid == std::process::id() as i32 || p.pid <= 1 || p.start < self.start {
                continue;
            }
            let args: Vec<_> = p.args.split(|&b| b == 0).collect();
            let supervisor = args.contains(&b"__supervisor".as_slice())
                && self
                    .home
                    .as_ref()
                    .is_some_and(|h| args.contains(&h.as_os_str().as_bytes()));
            // An orphan may keep stdout open after the wrapper exits. Match its
            // inherited pipe inode, without reading any process's environment.
            let pipe_holder = !self.pipes.is_empty()
                && fs::read_dir(format!("/proc/{}/fd", p.pid))
                    .into_iter()
                    .flatten()
                    .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
                    .any(|link| self.pipes.contains(&link));
            if (p.pid == self.root && p.start == self.start) || supervisor || pipe_holder {
                owned.insert(p.pid, p.clone());
            }
        }
        loop {
            let children: Vec<_> = all
                .values()
                .filter(|p| {
                    p.pid != std::process::id() as i32
                        && p.start >= self.start
                        && !owned.contains_key(&p.pid)
                        && owned.contains_key(&p.parent)
                })
                .cloned()
                .collect();
            if children.is_empty() {
                break;
            }
            owned.extend(children.into_iter().map(|p| (p.pid, p)));
        }
        owned
    }
    fn commands(&self) -> Vec<PathBuf> {
        let Some(home) = &self.home else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = fs::read_dir(home.join("cmd"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        ids.sort_unstable();
        if let Some(id) = self.undo_id {
            return vec![home.join("cmd").join(id.to_string())];
        }
        ids.into_iter()
            .rev()
            .take(3)
            .map(|id| home.join("cmd").join(id.to_string()))
            .collect()
    }
    pub fn diagnose(&self, elapsed: Duration, setup: Duration, limit: Duration, phase: &str) {
        let mut report = format!(
            "TEST COMMAND TIMEOUT: elapsed={elapsed:.3?}, limit={limit:?}, phase={phase}, pid={}\ncommand: {}\n",
            self.root, self.command
        );
        let _ = writeln!(
            report,
            "setup={setup:.3?}, timeout window={:.3?}",
            elapsed.saturating_sub(setup)
        );
        let folders = self.commands();
        let first_tree = self.tree();
        let first: Vec<_> = folders.iter().map(|p| progress(p)).collect();
        let sampled = Instant::now();
        thread::sleep(Duration::from_secs(1));
        let second_tree = self.tree();
        let _ = writeln!(
            report,
            "Process samples {:.3?} apart; CPU deltas are clock ticks:",
            sampled.elapsed()
        );
        for (label, tree) in [("before", &first_tree), ("after", &second_tree)] {
            let _ = writeln!(
                report,
                "{label}: pid ppid state wchan cpu_ticks delta cmdline"
            );
            for p in tree.values() {
                let delta = first_tree
                    .get(&p.pid)
                    .filter(|old| old.start == p.start)
                    .map_or(0, |old| p.ticks.saturating_sub(old.ticks));
                let args = p
                    .args
                    .split(|&b| b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| format!("{:?}", OsStr::from_bytes(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = writeln!(
                    report,
                    "{} {} {} {} {} +{} {}",
                    p.pid,
                    p.parent,
                    p.state,
                    p.wchan.trim(),
                    p.ticks,
                    delta,
                    args
                );
            }
        }
        if !second_tree.contains_key(&self.root) {
            let _ = writeln!(
                report,
                "wrapper exited; inspecting remaining descendants/pipe holders"
            );
        }
        if folders.is_empty() {
            let _ = writeln!(report, "No supervisor journal found for this fixture.");
        }
        for (folder, before) in folders.iter().zip(first) {
            let after = progress(folder);
            let _ = writeln!(report, "Journal {}:", folder.display());
            match (&before.notifications, &after.notifications) {
                (Ok(a), Ok(b)) => {
                    let _ = writeln!(
                        report,
                        "notifications: received={} (+{}), answered={} (+{} successful ioctl replies), last_id={}, tracee_pid={}, monotonic_ms={}",
                        b[0],
                        b[0].saturating_sub(a[0]),
                        b[1],
                        b[1].saturating_sub(a[1]),
                        b[2],
                        b[3],
                        b[4]
                    );
                    let status = if b[1] > a[1] {
                        "YES, answering notifications"
                    } else if folder.join("end").exists() {
                        "command ended; no active listener expected during undo"
                    } else {
                        "NO replies observed in sample; inspect process state/wchan and journal progress"
                    };
                    let _ = writeln!(report, "supervisor activity: {status}");
                }
                _ => {
                    let _ = writeln!(
                        report,
                        "notifications unavailable at {}: before={:?}, after={:?}",
                        folder.join("notifications").display(),
                        before.notifications.as_ref().err(),
                        after.notifications.as_ref().err()
                    );
                }
            }
            let _ = writeln!(
                report,
                "undo progress: before={} after={}",
                before.undo, after.undo
            );
            for file in [
                "meta",
                "effects.log",
                "outcomes.log",
                "post.log",
                "error",
                "end",
            ] {
                append_tail(&mut report, &folder.join(file));
            }
            if let Some(home) = &self.home {
                if let Some(undo) = undo_folder(folder) {
                    append_tail(&mut report, &undo.join("recovery.log"));
                }
                append_tail(
                    &mut report,
                    &home.join("logs").join(format!("{}.log", self.root)),
                );
            }
        }
        if let Some(path) = self.save(&report) {
            let _ = writeln!(report, "Diagnostic artifact: {}", path.display());
        }
        // Print before cleanup, even if a subsequent kill/reap cannot complete.
        let _ = std::io::stderr().write_all(report.as_bytes());
    }
    fn save(&self, report: &str) -> Option<PathBuf> {
        let logs = self.home.as_ref()?.join("logs");
        fs::create_dir_all(&logs).ok()?;
        let path = logs.join(format!("timeout-{}.log", self.root));
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .ok()?
            .write_all(report.as_bytes())
            .ok()?;
        Some(path)
    }
    pub fn kill_tree(&self) {
        let mut stopped = BTreeMap::new();
        // Freeze parents before the final rescan so they cannot keep forking.
        // Each signal validates start time to avoid hitting a recycled pid.
        for _ in 0..3 {
            let tree = self.tree();
            for p in tree.values() {
                signal_same(p, libc::SIGSTOP);
            }
            stopped.extend(tree);
        }
        for p in stopped.values().rev() {
            signal_same(p, libc::SIGKILL);
        }
    }
}
fn signal_same(p: &Process, signal: i32) {
    if process(p.pid).is_some_and(|now| now.start == p.start) {
        let _ = test_support::signal(p.pid, signal);
    }
}
struct Progress {
    notifications: io::Result<[u64; 5]>,
    undo: String,
}
fn progress(folder: &Path) -> Progress {
    let notifications = notification_counts(folder);
    let undo = undo_folder(folder)
        .and_then(|p| fs::read(p.join("state.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .map(|v| {
            format!(
                "effect={} completed_through={} cursor={} steps={}",
                v["effect"], v["completed_through"], v["cursor"], v["steps"]
            )
        })
        .unwrap_or_else(|| "no recovery state".into());
    Progress {
        notifications,
        undo,
    }
}
pub fn notification_counts(folder: &Path) -> io::Result<[u64; 5]> {
    let mut bytes = [0u8; 40];
    File::open(folder.join("notifications"))?.read_exact(&mut bytes)?;
    Ok(std::array::from_fn(|i| {
        u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
    }))
}
fn undo_folder(folder: &Path) -> Option<PathBuf> {
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(folder.join("meta")).ok()?).ok()?;
    Some(folder.parent()?.parent()?.join("q/main").join(format!(
        "undo-{}-{}",
        meta["id"].as_u64()?,
        meta["start_ns"].as_u64()?
    )))
}
fn append_tail(report: &mut String, path: &Path) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return;
    };
    let offset = size.saturating_sub(8192);
    let _ = file.seek(SeekFrom::Start(offset));
    let mut bytes = Vec::new();
    let _ = file.take(8192).read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = text.lines().collect();
    if offset > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let _ = writeln!(report, "{} ({} bytes), last lines:", path.display(), size);
    for line in lines.iter().skip(lines.len().saturating_sub(6)) {
        let _ = writeln!(report, "  {line}");
    }
}
