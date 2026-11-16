//! S writes plain files. Only the CLI opens SQLite, which is a rebuildable index.
use crate::{paths, sys};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
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
    let mut out = String::new();
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
    let mut out = Vec::new();
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
    for name in ["cmd", "q/main", "run", "logs"] {
        private_dir(&home.join(name))?;
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
        }
    }
    pub fn directory(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }
    pub fn same_inode(&self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}
pub fn state(p: &Path) -> io::Result<Option<State>> {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)?;
    sys::stat_at(libc::AT_FDCWD, &c).map(|s| s.as_ref().map(State::from_stat))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: u64,
    pub command: String,
    pub cwd: String,
    pub start_ns: u64,
    pub supervisor: i32,
    pub child: i32,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Detail {
    pub before: Option<State>,
    pub before2: Option<State>,
    pub kept: String,
    pub message: String,
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
    fn parse(line: &str) -> Result<Self> {
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
            note: serde_json::from_slice(&decode(f[8])?)?,
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
    let temp = p.with_extension(format!("tmp-{}", sys::pid()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    Ok((temp, file))
}
fn json_write(p: &Path, value: &impl Serialize) -> io::Result<()> {
    let (temp, _file) = json_temp(p, value)?;
    fs::rename(temp, p)
}
fn synced_json_write(p: &Path, value: &impl Serialize) -> io::Result<()> {
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
    pub touched: BTreeSet<PathBuf>,
    seq: u64,
    debug_fd: i32,
}
impl Journal {
    pub fn start(home: PathBuf, args: &[OsString], cwd: &Path, debug_fd: i32) -> io::Result<Self> {
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
        Ok(Self {
            home,
            folder,
            quarantine,
            meta,
            effects,
            outcomes,
            touched: BTreeSet::new(),
            seq: 0,
            debug_fd,
        })
    }
    pub fn reopen(home: PathBuf, id: u64) -> io::Result<Self> {
        let folder = home.join("cmd").join(id.to_string());
        let meta =
            serde_json::from_slice(&fs::read(folder.join("meta"))?).map_err(io::Error::other)?;
        let effects = read_effects(&folder).map_err(io::Error::other)?;
        let mut touched = BTreeSet::new();
        for e in &effects {
            for p in [&e.path, &e.path2] {
                if !p.is_empty() {
                    touched.insert(path(p)?);
                }
            }
        }
        Ok(Self {
            quarantine: home.join("q/main").join(id.to_string()),
            home,
            meta,
            effects: OpenOptions::new()
                .append(true)
                .open(folder.join("effects.log"))?,
            outcomes: OpenOptions::new()
                .append(true)
                .open(folder.join("outcomes.log"))?,
            seq: effects.len() as u64,
            folder,
            touched,
            debug_fd: -1,
        })
    }
    pub fn child(&mut self, pid: i32) -> io::Result<()> {
        self.meta.child = pid;
        json_write(&self.folder.join("child"), &pid)
    }
    pub fn next_seq(&self) -> u64 {
        self.seq + 1
    }
    pub fn append(&mut self, mut e: Effect) -> io::Result<u64> {
        self.seq += 1;
        e.seq = self.seq;
        for p in [&e.path, &e.path2] {
            if !p.is_empty() {
                self.touched.insert(path(p)?);
            }
        }
        write_once(&self.effects, e.line()?.as_bytes())?;
        Ok(e.seq)
    }
    pub fn outcome(&self, seq: u64, applied: bool) -> io::Result<()> {
        write_once(
            &self.outcomes,
            format!("{seq}\t{}\n", u8::from(applied)).as_bytes(),
        )
    }
    pub fn snapshot(&self) -> io::Result<()> {
        sys::debug_timestamp(self.debug_fd, b"snapshot_begin");
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.folder.join("post.log"))?;
        for p in &self.touched {
            let st = match state(p) {
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
        sys::debug_timestamp(self.debug_fd, b"snapshot_end");
        Ok(())
    }
    pub fn finish(&self, status: i32) -> io::Result<()> {
        self.snapshot()?;
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
    pub effects: Vec<Effect>,
    pub warning: Option<String>,
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
pub fn read_effects(folder: &Path) -> Result<Vec<Effect>> {
    let text = match fs::read_to_string(folder.join("effects.log")) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    // A trailing partial line is never an acknowledged intent. Refuse undo rather than guess.
    if !text.is_empty() && !text.ends_with('\n') {
        bail!("partial effects.log in {}", folder.display());
    }
    text.lines().map(Effect::parse).collect()
}
pub fn load(home: &Path, id: u64) -> Result<Record> {
    let folder = home.join("cmd").join(id.to_string());
    let mut meta: Meta =
        serde_json::from_slice(&fs::read(folder.join("meta")).context("command not found")?)?;
    match fs::read(folder.join("child")) {
        Ok(bytes) => meta.child = serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let effects = read_effects(&folder)?;
    let end = match fs::read(folder.join("end")) {
        Ok(b) => Some(serde_json::from_slice::<End>(&b)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let active = running(home, &meta);
    let status = if end.is_some() {
        "finished"
    } else if active {
        "running"
    } else {
        "incomplete"
    }
    .to_owned();
    if !active {
        let _ = fs::remove_file(home.join("run").join(meta.supervisor.to_string()));
    }
    let mut outcomes = BTreeMap::new();
    for line in fs::read_to_string(folder.join("outcomes.log"))
        .unwrap_or_default()
        .lines()
    {
        if let Some((seq, applied)) = line.split_once('\t') {
            outcomes.insert(seq.parse()?, applied == "1");
        }
    }
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
        effects,
        warning: fs::read_to_string(folder.join("error")).ok(),
        outcomes,
    })
}
pub fn post_states(folder: &Path) -> Result<BTreeMap<String, Option<State>>> {
    let mut states = BTreeMap::new();
    if let Ok(bytes) = fs::read(folder.join("recovered-post.json")) {
        for p in serde_json::from_slice::<Vec<Post>>(&bytes)? {
            states.insert(p.path, p.state);
        }
    }
    if let Ok(text) = fs::read_to_string(folder.join("post.log")) {
        for line in text.lines() {
            let p: Post = serde_json::from_str(line)?;
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
        let st = match state(&p) {
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
pub fn import(home: &Path) -> Result<Vec<Record>> {
    prepare(home)?;
    let mut db = database(home)?;
    let mut records = Vec::new();
    for e in fs::read_dir(home.join("cmd"))? {
        let e = e?;
        let Some(id) = e.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        if !e.path().join("meta").exists() {
            continue;
        }
        let record = load(home, id)?;
        recover_posts(&e.path(), &record)?;
        records.push(record);
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.meta.id));
    let tx = db.transaction()?;
    for r in &records {
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
    }
    tx.commit()?;
    Ok(records)
}
pub fn log(n: usize, json: bool) -> Result<i32> {
    let home = paths::home()?;
    let records = import(&home)?;
    let values: Vec<_> = records.iter().take(n).map(|r| {
        let count = |op: &str| r.effects.iter().filter(|e| e.op == op).count();
        serde_json::json!({"id":r.meta.id,"start_ns":r.meta.start_ns,"end_ns":r.end_ns,"duration_ns":r.end_ns.map(|end| end.saturating_sub(r.meta.start_ns)),"exit_status":r.exit_status,"status":r.status,"undone":r.undone,"cwd":r.meta.cwd,"command":r.meta.command,"counts":{"deleted":count("delete")+count("rmdir"),"modified":count("modify")+count("size")+count("rename")+count("exchange"),"created":count("create"),"blocked":count("blocked"),"not_protected":count("not protected")+usize::from(r.warning.is_some())}})
    }).collect();
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
            let status = if v["undone"] == true {
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
    import(&home)?;
    let r = load(&home, id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        let count = |op: &str| r.effects.iter().filter(|e| e.op == op).count();
        println!(
            "{id}: {} ({}) | {} effects, {} kept, {} blocked, {} not protected",
            one_line(&r.meta.command, 60),
            r.status,
            r.effects.len(),
            r.effects
                .iter()
                .filter(|e| !e.quarantine.is_empty())
                .count(),
            count("blocked"),
            count("not protected") + usize::from(r.warning.is_some())
        );
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
    fn every_path_byte_round_trips() {
        let bytes: Vec<u8> = (1..=255).collect();
        let encoded = encode(&bytes);
        assert!(!encoded.contains(['\t', '\n']));
        assert!(encoded.is_ascii());
        assert_eq!(decode(&encoded).unwrap(), bytes);
    }
}
