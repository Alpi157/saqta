//! S writes plain files. Only the CLI opens SQLite, which is a rebuildable index.
use crate::{paths, sys};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{DirBuilderExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
pub fn encode(bytes: &[u8]) -> String {
    if bytes
        .iter()
        .all(|&byte| (b' '..=b'~').contains(&byte) && byte != b'%')
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        return text.to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (b' '..=b'~').contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
pub fn decode(text: &str) -> io::Result<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.contains(&b'%') {
        return Ok(bytes.to_vec());
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| io::Error::other("short percent escape"))?;
            let hex = std::str::from_utf8(hex).map_err(io::Error::other)?;
            out.push(u8::from_str_radix(hex, 16).map_err(io::Error::other)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}
pub fn path(text: &str) -> io::Result<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(decode(text)?)))
}
pub fn display(p: &Path) -> String {
    encode(p.as_os_str().as_bytes())
}
pub fn parse_json<T: DeserializeOwned>(path: &Path, bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes)
        .map_err(|error| io::Error::other(format!("{}: {error}", display(path))))
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", display(path))))?;
    parse_json(path, &bytes)
}
pub fn read_json_optional<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => parse_json(path, &bytes).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", display(path)),
        )),
    }
}
pub fn private_dir(p: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}
pub fn private_file(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(p)
}
pub fn prepare(home: &Path) -> io::Result<()> {
    let fresh = !home.join("cmd").exists();
    for name in ["cmd", "q/main", "run", "logs"] {
        private_dir(&home.join(name))?;
    }
    if fresh {
        json_write(&home.join("gc-attempt"), &now())?;
    }
    private_file(&home.join("next-id"))?;
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub size: i64,
    pub mtime: i64,
    pub mtime_ns: i64,
    #[serde(default)]
    pub incarnation: sys::Incarnation,
}
impl State {
    pub fn from_stat(s: &libc::stat) -> Self {
        Self {
            mode: s.st_mode,
            dev: s.st_dev,
            ino: s.st_ino,
            size: s.st_size,
            mtime: s.st_mtime,
            mtime_ns: s.st_mtime_nsec,
            incarnation: sys::Incarnation::default(),
        }
    }
    pub fn directory(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }
    pub fn same_inode(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.incarnation.available()
            && self.incarnation == other.incarnation
    }
    pub fn at(base: i32, name: &std::ffi::CStr, s: &libc::stat) -> Self {
        let mut state = Self::from_stat(s);
        state.incarnation = sys::incarnation_at(base, name, s);
        state
    }
}
pub fn state(p: &Path) -> io::Result<Option<State>> {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)?;
    state_at(libc::AT_FDCWD, &c)
}
pub fn state_at(base: i32, name: &std::ffi::CStr) -> io::Result<Option<State>> {
    sys::stat_at(base, name).map(|s| s.as_ref().map(|s| State::at(base, name, s)))
}
pub fn capture_state_at(base: i32, name: &std::ffi::CStr) -> io::Result<Option<State>> {
    let (stat, _, birth) = sys::stat_capture_at(base, name)?;
    Ok(stat.as_ref().map(|stat| {
        let mut state = State::from_stat(stat);
        state.incarnation = sys::Incarnation::captured(base, name, stat, birth);
        state
    }))
}
pub fn capture_state(path: &Path) -> io::Result<Option<State>> {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    capture_state_at(libc::AT_FDCWD, &name)
}
fn snapshot_state(root: i32, p: &Path) -> io::Result<Option<State>> {
    let parent = p
        .parent()
        .ok_or_else(|| io::Error::other("missing snapshot parent"))?
        .strip_prefix("/")
        .map_err(io::Error::other)?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = std::ffi::CString::new(parent.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let fd = match sys::open_restore_parent(root, &parent) {
        Ok(fd) => fd,
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let name = std::ffi::CString::new(
        p.file_name()
            .ok_or_else(|| io::Error::other("missing snapshot name"))?
            .as_bytes(),
    )
    .map_err(io::Error::other)?;
    capture_state_at(fd.as_raw_fd(), &name)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: u64,
    pub command: String,
    pub cwd: String,
    pub start_ns: u64,
    pub supervisor: i32,
    pub child: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Detail {
    pub before: Option<State>,
    pub before2: Option<State>,
    pub kept: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub git: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub regenerable: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_created: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Effect {
    pub seq: u64,
    pub time_ns: u64,
    pub pid: u32,
    pub op: String,
    pub path: String,
    pub path2: String,
    pub quarantine: String,
    pub flags: u64,
    pub note: Detail,
}
impl Effect {
    pub fn line(&self) -> io::Result<String> {
        Ok(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.seq,
            self.time_ns,
            self.pid,
            self.op,
            self.path,
            self.path2,
            self.quarantine,
            self.flags,
            encode(
                serde_json::to_string(&self.note)
                    .map_err(io::Error::other)?
                    .as_bytes()
            )
        ))
    }
    fn parse(line: &str, source: &Path) -> Result<Self> {
        let f: Vec<_> = line.split('\t').collect();
        if f.len() != 9 {
            bail!("invalid effect line");
        }
        Ok(Self {
            seq: f[0].parse()?,
            time_ns: f[1].parse()?,
            pid: f[2].parse()?,
            op: f[3].into(),
            path: f[4].into(),
            path2: f[5].into(),
            quarantine: f[6].into(),
            flags: f[7].parse()?,
            note: parse_json(source, &decode(f[8])?)?,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Post {
    pub path: String,
    pub state: Option<State>,
}
#[derive(Serialize, Deserialize)]
pub struct End {
    pub status: i32,
    pub end_ns: u64,
}
fn json_temp(p: &Path, value: &impl Serialize) -> io::Result<(PathBuf, File)> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let temp = p.with_extension(format!("tmp-{}-{}", sys::pid(), now()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    Ok((temp, file))
}
pub fn json_write(p: &Path, value: &impl Serialize) -> io::Result<()> {
    let (temp, _file) = json_temp(p, value)?;
    fs::rename(temp, p)
}
pub fn synced_json_write(p: &Path, value: &impl Serialize) -> io::Result<()> {
    let (temp, file) = json_temp(p, value)?;
    file.sync_all()?;
    fs::rename(temp, p)
}
pub struct Journal {
    pub home: PathBuf,
    pub folder: PathBuf,
    pub quarantine: PathBuf,
    pub meta: Meta,
    effects: File,
    outcomes: File,
    positive_outcomes: Vec<u64>,
    effect_batch: String,
    pub touched: HashSet<PathBuf>,
    seq: u64,
    debug_fd: i32,
    notifications: Option<File>,
    pin_markers: BTreeMap<String, State>,
}
impl Journal {
    pub fn start(home: PathBuf, args: &[OsString], cwd: &Path, debug_fd: i32) -> io::Result<Self> {
        Self::start_session(home, args, cwd, debug_fd, None)
    }
    pub fn start_session(
        home: PathBuf,
        args: &[OsString],
        cwd: &Path,
        debug_fd: i32,
        session: Option<String>,
    ) -> io::Result<Self> {
        prepare(&home)?;
        sys::debug_timestamp(debug_fd, b"id_begin");
        let mut next = private_file(&home.join("next-id"))?;
        let deadline = sys::monotonic_ms() + 5000;
        loop {
            match sys::flock_exclusive(next.as_raw_fd(), true) {
                Ok(()) => break,
                Err(e)
                    if e.raw_os_error() == Some(libc::EWOULDBLOCK)
                        && sys::monotonic_ms() < deadline =>
                {
                    sys::poll(&mut [], 10);
                }
                Err(e) => return Err(e),
            }
        }
        let mut text = String::new();
        next.read_to_string(&mut text)?;
        let id = if text.is_empty() {
            1
        } else {
            text.trim().parse::<u64>().map_err(io::Error::other)?
        };
        let following = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("command id exhausted"))?;
        next.seek(SeekFrom::Start(0))?;
        next.write_all(format!("{following}\n").as_bytes())?;
        let length = next.stream_position()?;
        next.set_len(length)?;
        sys::unlock(next.as_raw_fd());
        sys::debug_timestamp(debug_fd, b"id_end");
        sys::debug_timestamp(debug_fd, b"folders_begin");
        let folder = home.join("cmd").join(id.to_string());
        // Never reuse an existing folder, even if next-id was damaged.
        fs::DirBuilder::new().mode(0o700).create(&folder)?;
        let quarantine = home.join("q/main").join(id.to_string());
        fs::DirBuilder::new().mode(0o700).create(&quarantine)?;
        sys::debug_timestamp(debug_fd, b"folders_end");
        sys::debug_timestamp(debug_fd, b"meta_begin");
        let meta = Meta {
            id,
            command: args
                .iter()
                .map(|a| encode(a.as_bytes()))
                .collect::<Vec<_>>()
                .join(" "),
            cwd: display(cwd),
            start_ns: now(),
            supervisor: sys::pid(),
            child: 0,
            session,
        };
        json_write(&folder.join("meta"), &meta)?;
        sys::debug_timestamp(debug_fd, b"meta_end");
        let effects = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(folder.join("effects.log"))?;
        let outcomes = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(folder.join("outcomes.log"))?;
        let notifications = (debug_fd >= 0)
            .then(|| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(folder.join("notifications"))
            })
            .and_then(Result::ok);
        Ok(Self {
            notifications,
            pin_markers: BTreeMap::new(),
            home,
            folder,
            quarantine,
            meta,
            effects,
            outcomes,
            positive_outcomes: Vec::new(),
            effect_batch: String::with_capacity(1024),
            touched: HashSet::new(),
            seq: 0,
            debug_fd,
        })
    }
    pub fn reopen(home: PathBuf, id: u64) -> io::Result<Self> {
        let folder = home.join("cmd").join(id.to_string());
        let meta = read_json(&folder.join("meta"))?;
        let effects = read_effects(&folder).map_err(io::Error::other)?;
        let mut touched = HashSet::new();
        for e in &effects {
            for p in [&e.path, &e.path2] {
                if !p.is_empty() {
                    touched.insert(path(p)?);
                }
            }
        }
        Ok(Self {
            pin_markers: read_json_optional(&folder.join("inode-pins.json"))?.unwrap_or_default(),
            quarantine: home.join("q/main").join(id.to_string()),
            home,
            meta,
            effects: OpenOptions::new()
                .append(true)
                .open(folder.join("effects.log"))?,
            outcomes: OpenOptions::new()
                .append(true)
                .open(folder.join("outcomes.log"))?,
            positive_outcomes: read_positive_outcomes(&folder)?,
            effect_batch: String::with_capacity(1024),
            seq: effects.len() as u64,
            folder,
            touched,
            debug_fd: -1,
            notifications: None,
        })
    }
    /// Debug-only observation data. Five LE u64s: received, answered, id, pid, monotonic ms.
    /// A failed observation must never prevent answering the tracee.
    pub fn notification_progress(&self, received: u64, answered: u64, id: u64, pid: u32) {
        use std::os::unix::fs::FileExt;
        if let Some(file) = &self.notifications {
            let fields = [
                received,
                answered,
                id,
                u64::from(pid),
                sys::monotonic_ms() as u64,
            ];
            let mut bytes = [0u8; 40];
            for (slot, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
                slot.copy_from_slice(&value.to_le_bytes());
            }
            let _ = file.write_at(&bytes, 0);
        }
    }
    pub fn child(&mut self, pid: i32) -> io::Result<()> {
        self.meta.child = pid;
        json_write(&self.folder.join("child"), &pid)
    }
    pub fn next_seq(&self) -> u64 {
        self.seq + 1
    }
    fn prepare_effect(&mut self, e: &mut Effect) -> io::Result<()> {
        self.seq += 1;
        e.seq = self.seq;
        for p in [&e.path, &e.path2] {
            if !p.is_empty() {
                self.touched.insert(path(p)?);
            }
        }
        Ok(())
    }
    pub fn append(&mut self, mut e: Effect) -> io::Result<u64> {
        self.prepare_effect(&mut e)?;
        write_once(&self.effects, e.line()?.as_bytes())?;
        Ok(e.seq)
    }
    pub fn record_pin(&mut self, name: &str, state: State) -> io::Result<()> {
        self.pin_markers.insert(name.to_owned(), state);
        // Publish the original incarnation before replacing its last quarantine
        // hard link. Atomic replacement preserves the process-crash guarantee.
        json_write(&self.folder.join("inode-pins.json"), &self.pin_markers)
    }
    pub fn append_batch(&mut self, mut effects: Vec<Effect>) -> io::Result<Vec<u64>> {
        if effects.len() == 1 {
            // The length check proves pop succeeds; keep the ordinary one-line path.
            if let Some(e) = effects.pop() {
                return Ok(vec![self.append(e)?]);
            }
        }
        let mut seqs = Vec::with_capacity(effects.len());
        self.effect_batch.clear();
        for mut e in effects {
            self.prepare_effect(&mut e)?;
            self.effect_batch.push_str(&e.line()?);
            seqs.push(e.seq);
        }
        if !self.effect_batch.is_empty() {
            write_once(&self.effects, self.effect_batch.as_bytes())?;
        }
        Ok(seqs)
    }
    pub fn outcome(&mut self, seq: u64, applied: bool) -> io::Result<()> {
        if applied {
            // Unknown outcomes already replay as applied. Losing only a positive
            // batch on supervisor death cannot change any recovery decision.
            self.positive_outcomes.push(seq);
            return Ok(());
        }
        write_once(
            &self.outcomes,
            format!("{seq}\t{}\n", u8::from(applied)).as_bytes(),
        )
    }
    pub fn snapshot(&self) -> io::Result<()> {
        sys::debug_timestamp(self.debug_fd, b"snapshot_begin");
        if !self.positive_outcomes.is_empty() {
            json_write(
                &self.folder.join("outcomes-positive.json"),
                &self.positive_outcomes,
            )?;
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.folder.join("post.log"))?;
        let mut file = BufWriter::with_capacity(256 * 1024, file);
        let root = sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
        for p in &self.touched {
            let st = match snapshot_state(root.as_raw_fd(), p) {
                Ok(s) => s,
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => None,
                Err(e) => return Err(e),
            };
            serde_json::to_writer(
                &mut file,
                &Post {
                    path: display(p),
                    state: st,
                },
            )
            .map_err(io::Error::other)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        sys::debug_timestamp(self.debug_fd, b"snapshot_end");
        Ok(())
    }
    pub fn finish(&self, status: i32) -> io::Result<()> {
        self.snapshot()?;
        self.end(status)
    }
    pub fn end(&self, status: i32) -> io::Result<()> {
        sys::debug_timestamp(self.debug_fd, b"end_begin");
        json_write(
            &self.folder.join("end"),
            &End {
                status,
                end_ns: now(),
            },
        )?;
        sys::debug_timestamp(self.debug_fd, b"end_end");
        Ok(())
    }
    pub fn error(&self, e: &io::Error) {
        let _ = fs::write(self.folder.join("error"), e.to_string());
    }
}
fn read_positive_outcomes(folder: &Path) -> io::Result<Vec<u64>> {
    Ok(read_json_optional(&folder.join("outcomes-positive.json"))?.unwrap_or_default())
}
fn write_once(file: &File, bytes: &[u8]) -> io::Result<()> {
    loop {
        let n = sys::write(file.as_raw_fd(), bytes);
        if n == bytes.len() as isize {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if n < 0 && e.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(if n < 0 {
            e
        } else {
            io::Error::other("short journal write")
        });
    }
}
#[derive(Serialize)]
pub struct Record {
    pub meta: Meta,
    pub status: String,
    pub exit_status: Option<i32>,
    pub end_ns: Option<u64>,
    pub undone: bool,
    pub held: bool,
    pub hold_rule: Option<crate::rules::Rule>,
    pub released: bool,
    pub rules_error: Option<String>,
    pub effects: Vec<Effect>,
    pub warning: Option<String>,
    pub recovery_note: Option<String>,
    #[serde(skip)]
    pub outcomes: BTreeMap<u64, bool>,
}
pub fn running(home: &Path, m: &Meta) -> bool {
    home.join("run").join(m.supervisor.to_string()).exists() && process_live(m.supervisor)
}
pub fn process_live(pid: i32) -> bool {
    if !sys::alive(pid) {
        return false;
    }
    fs::read(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|b| {
            b.iter()
                .rposition(|&v| v == b')')
                .and_then(|i| b.get(i + 2).copied())
        })
        .is_some_and(|s| s != b'Z' && s != b'X')
}
fn read_raw_effects(folder: &Path) -> Result<Vec<Effect>> {
    let text = match fs::read_to_string(folder.join("effects.log")) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    // A crash can interrupt a regular-file write. Only a separately qualified
    // dead-command recovery marker permits reading its acknowledged prefix.
    if !text.is_empty() && !text.ends_with('\n') {
        if let Ok(bytes) = fs::read(folder.join("tail-recovery.json")) {
            let recovery: TailRecovery = parse_json(&folder.join("tail-recovery.json"), &bytes)?;
            if text.as_bytes().get(recovery.cut..) == Some(recovery.tail.as_slice())
                && let Some(prefix) = text.get(..recovery.cut)
                && (prefix.is_empty() || prefix.ends_with('\n'))
            {
                return prefix
                    .lines()
                    .map(|line| Effect::parse(line, &folder.join("effects.log")))
                    .collect();
            }
        }
        bail!("partial effects.log in {}", folder.display());
    }
    text.lines()
        .map(|line| Effect::parse(line, &folder.join("effects.log")))
        .collect()
}
fn complete_pins(folder: &Path, effects: &mut [Effect]) -> Result<()> {
    let marker_file = folder.join("inode-pins.json");
    let mut markers: BTreeMap<String, State> =
        read_json_optional(&marker_file)?.unwrap_or_default();
    let id = folder
        .file_name()
        .ok_or_else(|| io::Error::other("missing command id"))?;
    let home = folder
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("missing store parent"))?;
    for effect in effects.iter_mut().filter(|effect| effect.note.pinned) {
        let name = Path::new(&effect.quarantine);
        if name.components().count() != 1 || name.file_name().is_none() {
            bail!(
                "{}: invalid quarantine pin name; kept both versions",
                display(&folder.join("effects.log"))
            );
        }
        let before = effect
            .note
            .before
            .as_mut()
            .ok_or_else(|| io::Error::other("missing pinned state"))?;
        if !markers.contains_key(&effect.quarantine) {
            let path = home.join("q/main").join(id).join(&effect.quarantine);
            let current = state(&path)?.ok_or_else(|| {
                io::Error::other(format!(
                    "missing retained inode {}; kept both versions",
                    display(&path)
                ))
            })?;
            if current.dev != before.dev
                || current.ino != before.ino
                || before.incarnation.birth.is_some()
                    && before.incarnation.birth != current.incarnation.birth
            {
                bail!(
                    "retained inode changed at {}; kept both versions",
                    display(&path)
                );
            }
            let mut original = before.clone();
            original.incarnation = current.incarnation;
            markers.insert(effect.quarantine.clone(), original);
        }
        let original = markers
            .get(&effect.quarantine)
            .ok_or_else(|| io::Error::other("missing completed pin marker"))?;
        if original.dev != before.dev
            || original.ino != before.ino
            || before.incarnation.birth.is_some()
                && before.incarnation.birth != original.incarnation.birth
        {
            bail!(
                "{}: contradictory retained inode marker; kept both versions",
                display(&marker_file)
            );
        }
        before.incarnation = original.incarnation;
    }
    Ok(())
}
pub fn persist_pins(folder: &Path, effects: &[Effect]) -> io::Result<()> {
    let file = folder.join("inode-pins.json");
    let mut markers: BTreeMap<String, State> = read_json_optional(&file)?.unwrap_or_default();
    let mut changed = false;
    for effect in effects.iter().filter(|effect| effect.note.pinned) {
        if let Some(state) = &effect.note.before
            && markers.get(&effect.quarantine) != Some(state)
        {
            markers.insert(effect.quarantine.clone(), state.clone());
            changed = true;
        }
    }
    if changed {
        synced_json_write(&file, &markers)?;
    }
    Ok(())
}
pub fn read_effects(folder: &Path) -> Result<Vec<Effect>> {
    let mut effects = read_raw_effects(folder)?;
    complete_pins(folder, &mut effects)?;
    Ok(effects)
}
#[derive(Serialize, Deserialize)]
struct TailRecovery {
    cut: usize,
    tail: Vec<u8>,
    note: String,
}
fn recover_unacknowledged_tail(folder: &Path) -> Result<Vec<Effect>> {
    let text = fs::read_to_string(folder.join("effects.log"))?;
    if text.ends_with('\n') {
        bail!("damaged complete effects.log in {}", folder.display());
    }
    let cut = text.rfind('\n').map_or(0, |i| i + 1);
    let effects = text[..cut]
        .lines()
        .map(|line| Effect::parse(line, &folder.join("effects.log")))
        .collect::<Result<Vec<_>>>()?;
    if effects.iter().any(|e| process_live(e.pid as i32)) {
        bail!("partial effects.log still has live command processes");
    }
    let recovery = TailRecovery {
        cut,
        tail: text.as_bytes()[cut..].to_vec(),
        note: format!(
            "recovered complete effect prefix; unacknowledged tail retained in {}",
            display(&folder.join("effects.log"))
        ),
    };
    synced_json_write(&folder.join("tail-recovery.json"), &recovery)?;
    Ok(effects)
}
pub fn load(home: &Path, id: u64) -> Result<Record> {
    let folder = home.join("cmd").join(id.to_string());
    let mut meta: Meta = read_json(&folder.join("meta")).context("command not found")?;
    if let Some(child) = read_json_optional(&folder.join("child"))? {
        meta.child = child;
    }
    let mut effects = match read_raw_effects(&folder) {
        Ok(effects) => effects,
        Err(_)
            if !folder.join("end").exists()
                && !folder.join("error").exists()
                && meta.child > 0
                && !running(home, &meta)
                && !process_live(meta.child) =>
        {
            recover_unacknowledged_tail(&folder)?
        }
        Err(e) => return Err(e),
    };
    complete_pins(&folder, &mut effects)?;
    let expired = crate::gc::expired(&folder)?;
    for e in &mut effects {
        if expired.contains(&e.quarantine) {
            e.op = "expired".into();
            e.note.message = "expired (collected by gc)".into();
        }
    }
    let end: Option<End> = read_json_optional(&folder.join("end"))?;
    let active = running(home, &meta);
    let status = if end.is_some() {
        "finished"
    } else if active {
        "running"
    } else {
        "incomplete"
    }
    .to_owned();
    let mut outcomes = BTreeMap::new();
    for line in fs::read_to_string(folder.join("outcomes.log"))
        .unwrap_or_default()
        .lines()
    {
        if let Some((seq, applied)) = line.split_once('\t') {
            outcomes.insert(seq.parse()?, applied == "1");
        }
    }
    for seq in read_positive_outcomes(&folder)? {
        if seq == 0
            || seq > effects.last().map_or(0, |e| e.seq)
            || outcomes.get(&seq) == Some(&false)
        {
            bail!("invalid or contradictory positive outcome {seq}");
        }
        outcomes.insert(seq, true);
    }
    let hold_rule = crate::hold::rule_for(home, id)?;
    Ok(Record {
        meta,
        status,
        end_ns: end.as_ref().map(|e| e.end_ns),
        exit_status: end.map(|e| {
            if libc::WIFEXITED(e.status) {
                libc::WEXITSTATUS(e.status)
            } else {
                128 + libc::WTERMSIG(e.status)
            }
        }),
        undone: folder.join("undone").exists(),
        held: hold_rule.is_some(),
        hold_rule,
        released: crate::hold::directory(home, id).join("released").exists(),
        rules_error: fs::read_to_string(folder.join("rules-error")).ok(),
        effects,
        warning: fs::read_to_string(folder.join("error")).ok(),
        recovery_note: read_json_optional::<TailRecovery>(&folder.join("tail-recovery.json"))?
            .map(|r| r.note),
        outcomes,
    })
}
pub fn post_states(folder: &Path) -> Result<BTreeMap<String, Option<State>>> {
    let mut states = BTreeMap::new();
    if let Some(posts) = read_json_optional::<Vec<Post>>(&folder.join("recovered-post.json"))? {
        for p in posts {
            states.insert(p.path, p.state);
        }
    }
    if let Ok(text) = fs::read_to_string(folder.join("post.log")) {
        for line in text.lines() {
            let p: Post = parse_json(&folder.join("post.log"), line.as_bytes())?;
            states.insert(p.path, p.state);
        }
    }
    Ok(states)
}
pub fn mark_undone(folder: &Path) -> io::Result<()> {
    synced_json_write(&folder.join("undone"), &now())
}
fn database(home: &Path) -> Result<Connection> {
    private_file(&home.join("journal.db"))?;
    let db = Connection::open(home.join("journal.db"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=1;
        CREATE TABLE IF NOT EXISTS commands(id INTEGER PRIMARY KEY, start_ns INTEGER NOT NULL, command BLOB NOT NULL, cwd BLOB NOT NULL, status TEXT NOT NULL, exit_status INTEGER, undone INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS effects(command_id INTEGER NOT NULL, seq INTEGER NOT NULL, time_ns INTEGER NOT NULL, pid INTEGER NOT NULL, op TEXT NOT NULL, path BLOB NOT NULL, path2 BLOB NOT NULL, quarantine TEXT NOT NULL, flags INTEGER NOT NULL, note TEXT NOT NULL, PRIMARY KEY(command_id,seq));")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS imports(id INTEGER PRIMARY KEY, stamp BLOB NOT NULL, summary BLOB NOT NULL);")?;
    Ok(db)
}
fn recover_posts(folder: &Path, r: &Record) -> Result<()> {
    if r.status != "incomplete"
        || folder.join("recovered-post.json").exists()
        || process_live(r.meta.child)
        || r.effects.iter().any(|e| process_live(e.pid as i32))
    {
        return Ok(());
    }
    let root = sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
    let recorded = post_states(folder)?;
    let mut missing = BTreeSet::new();
    for e in &r.effects {
        if e.op != "blocked" {
            for p in [&e.path, &e.path2] {
                if !p.is_empty() && !recorded.contains_key(p) {
                    missing.insert(p.clone());
                }
            }
        }
    }
    let mut posts = Vec::new();
    for name in missing {
        let p = path(&name)?;
        let st = match snapshot_state(root.as_raw_fd(), &p) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => None,
            Err(e) => return Err(e.into()),
        };
        posts.push(Post {
            path: name,
            state: st,
        });
    }
    synced_json_write(&folder.join("recovered-post.json"), &posts)?;
    Ok(())
}
fn summary(r: &Record) -> serde_json::Value {
    let count = |op: &str| r.effects.iter().filter(|e| e.op == op).count();
    serde_json::json!({"id":r.meta.id,"start_ns":r.meta.start_ns,"end_ns":r.end_ns,"duration_ns":r.end_ns.map(|end| end.saturating_sub(r.meta.start_ns)),"exit_status":r.exit_status,"status":r.status,"undone":r.undone,"held":r.held,"released":r.released,"cwd":r.meta.cwd,"command":r.meta.command,"counts":{"deleted":count("delete")+count("rmdir"),"modified":count("modify")+count("size")+count("rename")+count("exchange"),"created":count("create"),"blocked":count("blocked"),"not_protected":count("not protected")+usize::from(r.warning.is_some())}})
}
fn source_stamp(folder: &Path, hold: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let mut stamp = Vec::new();
    for name in [
        "meta",
        "child",
        "effects.log",
        "outcomes.log",
        "outcomes-positive.json",
        "post.log",
        "end",
        "undone",
        "error",
        "expired.json",
        "tail-recovery.json",
        "inode-pins.json",
    ]
    .into_iter()
    .map(|name| folder.join(name))
    .chain(
        ["ready", "released"]
            .into_iter()
            .map(|name| hold.join(name)),
    ) {
        match fs::symlink_metadata(name) {
            Ok(m) => stamp.push(Some((m.ino(), m.len(), m.mtime(), m.mtime_nsec()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => stamp.push(None),
            Err(e) => return Err(e),
        }
    }
    serde_json::to_vec(&stamp).map_err(io::Error::other)
}
pub fn refresh(home: &Path, id: u64) -> Result<Record> {
    prepare(home)?;
    let record = load(home, id)?;
    if !running(home, &record.meta) {
        let _ = fs::remove_file(home.join("run").join(record.meta.supervisor.to_string()));
    }
    recover_posts(&home.join("cmd").join(id.to_string()), &record)?;
    Ok(record)
}
pub fn import(home: &Path) -> Result<Vec<serde_json::Value>> {
    prepare(home)?;
    let mut db = database(home)?;
    let tx = db.transaction()?;
    let mut values = Vec::new();
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(home.join("cmd"))? {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let folder = entry.path();
        if !folder.join("meta").exists() {
            continue;
        }
        ids.insert(id);
        let stamp = source_stamp(&folder, &crate::hold::directory(home, id))?;
        let cached = tx
            .query_row(
                "SELECT stamp,summary FROM imports WHERE id=?1",
                [i64::try_from(id)?],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        if let Some((previous, bytes)) = cached {
            let value: serde_json::Value = parse_json(&home.join("journal.db"), &bytes)?;
            if stamp == previous && value["status"] != "running" {
                values.push(value);
                continue;
            }
        }
        let r = refresh(home, id)?;
        let value = summary(&r);
        tx.execute(
            "INSERT OR REPLACE INTO commands VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                i64::try_from(r.meta.id)?,
                i64::try_from(r.meta.start_ns)?,
                decode(&r.meta.command)?,
                decode(&r.meta.cwd)?,
                r.status,
                r.exit_status,
                r.undone
            ],
        )?;
        tx.execute(
            "DELETE FROM effects WHERE command_id=?1",
            [i64::try_from(r.meta.id)?],
        )?;
        for e in &r.effects {
            tx.execute(
                "INSERT INTO effects VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    i64::try_from(r.meta.id)?,
                    i64::try_from(e.seq)?,
                    i64::try_from(e.time_ns)?,
                    e.pid,
                    e.op,
                    decode(&e.path)?,
                    decode(&e.path2)?,
                    e.quarantine,
                    e.flags as i64,
                    serde_json::to_string(&e.note)?
                ],
            )?;
        }

        tx.execute(
            "INSERT OR REPLACE INTO imports VALUES (?1,?2,?3)",
            params![i64::try_from(id)?, stamp, serde_json::to_vec(&value)?],
        )?;
        values.push(value);
    }
    // A collected folder must never remain visible in the rebuildable index.
    let cached_ids = tx
        .prepare("SELECT id FROM commands")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in cached_ids
        .into_iter()
        .filter(|id| !ids.contains(&(*id as u64)))
    {
        tx.execute("DELETE FROM effects WHERE command_id=?1", [id])?;
        tx.execute("DELETE FROM commands WHERE id=?1", [id])?;
        tx.execute("DELETE FROM imports WHERE id=?1", [id])?;
    }
    tx.commit()?;
    values.sort_by_key(|v| std::cmp::Reverse(v["id"].as_u64()));
    Ok(values)
}
pub fn log(n: usize, json: bool) -> Result<i32> {
    let home = paths::home()?;
    let values: Vec<_> = import(&home)?.into_iter().take(n).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        let columns = std::env::var("COLUMNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .or_else(sys::terminal_columns)
            .unwrap_or(120)
            .clamp(40, 240);
        let header = if columns >= 110 {
            format!(
                "{:>6}  {:19}  {:>8}  {:>4}  {:10}  {:>5} {:>5} {:>5} {:>5} {:>5}  COMMAND",
                "ID", "STARTED", "DURATION", "EXIT", "STATUS", "DEL", "MOD", "NEW", "BLOCK", "GAPS"
            )
        } else if columns >= 70 {
            format!(
                "{:>6}  {:19}  {:>8}  {:>4}  {:10}  COMMAND",
                "ID", "STARTED", "DURATION", "EXIT", "STATUS"
            )
        } else {
            format!("{:>6}  {:19}  {:>8}  COMMAND", "ID", "STARTED", "DURATION")
        };
        println!("{}", one_line(&header, columns));
        for v in values {
            let start = sys::local_time(v["start_ns"].as_u64().unwrap_or(0))?;
            let duration = v["duration_ns"]
                .as_u64()
                .map(|ns| format!("{:.1} s", ns as f64 / 1e9))
                .unwrap_or_else(|| "-".into());
            let status = if v["held"] == true && v["released"] != true {
                "held"
            } else if v["undone"] == true {
                "undone"
            } else {
                v["status"].as_str().unwrap_or("")
            };
            let exit = v["exit_status"]
                .as_i64()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into());
            let prefix = if columns >= 110 {
                format!(
                    "{:>6}  {start}  {duration:>8}  {:>4}  {status:10}  {:>5} {:>5} {:>5} {:>5} {:>5}  ",
                    v["id"].to_string(),
                    exit,
                    v["counts"]["deleted"].to_string(),
                    v["counts"]["modified"].to_string(),
                    v["counts"]["created"].to_string(),
                    v["counts"]["blocked"].to_string(),
                    v["counts"]["not_protected"].to_string(),
                )
            } else if columns >= 70 {
                format!(
                    "{:>6}  {start}  {duration:>8}  {exit:>4}  {status:10}  ",
                    v["id"].to_string()
                )
            } else {
                format!("{:>6}  {start}  {duration:>8}  ", v["id"].to_string())
            };
            println!(
                "{prefix}{}",
                one_line(
                    v["command"].as_str().unwrap_or(""),
                    columns.saturating_sub(prefix.chars().count())
                )
            );
        }
    }
    Ok(0)
}
fn one_line(text: &str, width: usize) -> String {
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut value: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() && width >= 3 {
        value.truncate(
            value
                .char_indices()
                .nth(width - 3)
                .map_or(value.len(), |(i, _)| i),
        );
        value.push_str("...");
    }
    value
}
pub fn show(id: u64, json: bool) -> Result<i32> {
    let home = paths::home()?;
    let r = refresh(&home, id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        let count = |op: &str| r.effects.iter().filter(|e| e.op == op).count();
        println!(
            "{id}: {} ({}) | {} effects, {} kept, {} blocked, {} not protected",
            one_line(&r.meta.command, 60),
            if r.held && !r.released {
                "held"
            } else {
                &r.status
            },
            r.effects.len(),
            r.effects
                .iter()
                .filter(|e| !e.quarantine.is_empty())
                .count(),
            count("blocked"),
            count("not protected") + usize::from(r.warning.is_some())
        );
        if let Some(rule) = r.hold_rule {
            println!("automatic restore rule {rule:?}");
        }
        if let Some(error) = r.rules_error {
            println!("automatic restore: {error}");
        }
        if let Some(note) = r.recovery_note {
            println!("{note}");
        }
        if let Some(warning) = r.warning {
            println!("not protected: {warning}");
        }
        let path_width = r
            .effects
            .iter()
            .map(|e| {
                e.path.len()
                    + if e.path2.is_empty() {
                        0
                    } else {
                        e.path2.len() + 4
                    }
            })
            .max()
            .unwrap_or(4)
            .max(4);
        let q_width = r
            .effects
            .iter()
            .map(|e| e.quarantine.len())
            .max()
            .unwrap_or(7)
            .max(7);
        println!(
            "{:>6}  {:13}  {:path_width$}  {:8}  {:q_width$}  NOTE",
            "SEQ", "OPERATION", "PATH", "KEPT", "VERSION"
        );
        for e in r.effects {
            let path = if e.path2.is_empty() {
                e.path
            } else {
                format!("{} -> {}", e.path, e.path2)
            };
            println!(
                "{:>6}  {:13}  {:path_width$}  {:8}  {:q_width$}  {}",
                e.seq,
                e.op,
                path,
                if e.quarantine.is_empty() {
                    "-"
                } else {
                    &e.note.kept
                },
                if e.quarantine.is_empty() {
                    "-"
                } else {
                    &e.quarantine
                },
                one_line(&e.note.message, 160)
            );
        }
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incarnation_identity_requires_all_markers_and_ignores_written_metadata() {
        let legacy =
            serde_json::json!({"mode":33188,"dev":1,"ino":2,"size":3,"mtime":4,"mtime_ns":5});
        let mut first: State = serde_json::from_value(legacy.clone()).unwrap();
        assert!(!first.same_inode(&first));
        first.incarnation = sys::Incarnation {
            birth: Some((10, 20)),
            generation: Some(0),
            unverified: false,
        };
        let mut alias = first.clone();
        alias.size = 100;
        alias.mtime = 200;
        alias.mtime_ns = 300;
        assert!(first.same_inode(&alias));
        alias.incarnation.generation = Some(1);
        assert!(!first.same_inode(&alias));
        alias.incarnation.generation = Some(0);
        alias.incarnation.birth = Some((10, 21));
        assert!(!first.same_inode(&alias));
        alias.incarnation.birth = None;
        assert!(!first.same_inode(&alias));
        first.incarnation.birth = None;
        assert!(first.same_inode(&alias));
        assert!(!first.same_inode(&serde_json::from_value(legacy).unwrap()));
    }
    #[test]
    fn every_path_byte_round_trips() {
        let bytes: Vec<u8> = (1..=255).collect();
        let encoded = encode(&bytes);
        assert!(!encoded.contains(['\t', '\n']));
        assert!(encoded.is_ascii());
        assert_eq!(decode(&encoded).unwrap(), bytes);
    }
}
