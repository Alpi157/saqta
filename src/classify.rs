//! Compare retained versions with the command's post-state and Git's reachable copies.
use crate::{
    journal::{self, Effect, Record},
    rules::{Policy, Rule},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

pub fn git_database(path: &Path) -> bool {
    path.iter().any(|p| p == ".git")
}

fn git_binary() -> Option<&'static Path> {
    [Path::new("/usr/bin/git"), Path::new("/bin/git")]
        .into_iter()
        .find(|p| p.is_file())
}

pub fn git_process(pid: u32) -> bool {
    let identity = |p: &Path| fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let Some(actual) = identity(&PathBuf::from(format!("/proc/{pid}/exe"))) else {
        return false;
    };
    [
        "/usr/bin/git",
        "/bin/git",
        "/usr/lib/git-core/git",
        "/usr/libexec/git-core/git",
    ]
    .iter()
    .any(|p| identity(Path::new(p)) == Some(actual))
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Birth {
    dev: u64,
    ino: u64,
    created: SystemTime,
}

impl Birth {
    pub fn read(path: &Path) -> Option<Self> {
        let st = fs::symlink_metadata(path).ok()?;
        Some(Self {
            dev: st.dev(),
            ino: st.ino(),
            created: st.created().ok()?,
        })
    }
}

struct Change<'a> {
    path: PathBuf,
    effect: &'a Effect,
    retained: PathBuf,
    removed: bool,
    discarded: bool,
    agent: bool,
    tracked: bool,
    ignored: bool,
    clean: bool,
    git_has: Option<bool>,
    in_stash: bool,
}

pub struct Decision {
    pub groups: BTreeMap<Rule, BTreeSet<PathBuf>>,
    pub valuable: usize,
}

impl Decision {
    pub fn rule(&self) -> Option<Rule> {
        self.groups.keys().next().copied()
    }
    pub fn names(&self) -> BTreeSet<PathBuf> {
        self.groups.values().flatten().cloned().collect()
    }
}

pub fn applied(record: &Record, e: &Effect) -> bool {
    record.outcomes.get(&e.seq) == Some(&true)
        && !matches!(e.op.as_str(), "blocked" | "not protected" | "expired")
}

fn map_name(path: &mut PathBuf, from: &Path, to: &Path) -> bool {
    if let Ok(tail) = path.strip_prefix(from) {
        *path = if tail.as_os_str().is_empty() {
            to.to_owned()
        } else {
            to.join(tail)
        };
        true
    } else {
        false
    }
}

fn move_name(path: &mut PathBuf, e: &Effect, reverse: bool) -> io::Result<()> {
    if matches!(e.op.as_str(), "rename" | "exchange") {
        let first = journal::path(&e.path)?;
        let second = journal::path(&e.path2)?;
        let (from, to) = if reverse {
            (&second, &first)
        } else {
            (&first, &second)
        };
        if !map_name(path, from, to) && e.op == "exchange" {
            map_name(path, to, from);
        }
    }
    Ok(())
}

fn open_regular(path: &Path) -> io::Result<File> {
    let fd = crate::sys::open_at(
        libc::AT_FDCWD,
        &std::ffi::CString::new(path.as_os_str().as_bytes())?,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        0,
    )?;
    let file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    Ok(file)
}

fn same_prefix(saved: &Path, current: &Path, old: &journal::State, new: &journal::State) -> bool {
    if old.mode & libc::S_IFMT != new.mode & libc::S_IFMT || old.size > new.size {
        return false;
    }
    if old.same_inode(new) && old == new {
        return true;
    }
    match old.mode & libc::S_IFMT {
        libc::S_IFLNK => fs::read_link(saved)
            .ok()
            .zip(fs::read_link(current).ok())
            .is_some_and(|(a, b)| a == b),
        libc::S_IFREG => {
            let (Ok(mut a), Ok(mut b)) = (open_regular(saved), open_regular(current)) else {
                return false;
            };
            let (mut aa, mut bb) = ([0; 32768], [0; 32768]);
            let mut remaining = old.size.max(0) as usize;
            while remaining > 0 {
                let n = remaining.min(aa.len());
                if a.read_exact(&mut aa[..n]).is_err()
                    || b.read_exact(&mut bb[..n]).is_err()
                    || aa[..n] != bb[..n]
                {
                    return false;
                }
                remaining -= n;
            }
            true
        }
        _ => false,
    }
}

pub fn evaluate(
    record: &Record,
    folder: &Path,
    quarantine: &Path,
    policy: &Policy,
    breaker: bool,
    after: u64,
) -> Result<Decision> {
    let posts = journal::post_states(folder)?;
    let effects: Vec<_> = record
        .effects
        .iter()
        .filter(|e| applied(record, e))
        .collect();
    let moves = effects
        .iter()
        .copied()
        .filter(|e| matches!(e.op.as_str(), "rename" | "exchange"))
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut changes = Vec::new();
    for e in &effects {
        let Some(before) = &e.note.before else {
            continue;
        };
        if before.directory()
            || e.quarantine.is_empty()
            || !matches!(e.op.as_str(), "delete" | "modify")
        {
            continue;
        }
        let path = journal::path(&e.path)?;
        let mut original = path.clone();
        for m in moves.iter().rev().filter(|m| m.seq < e.seq) {
            move_name(&mut original, m, true)?;
        }
        if e.seq <= after && !breaker || !seen.insert(original.clone()) {
            continue;
        }
        let mut current = path;
        for m in moves.iter().filter(|m| m.seq > e.seq) {
            move_name(&mut current, m, false)?;
        }
        let retained = quarantine.join(&e.quarantine);
        let new = posts
            .get(&journal::display(&current))
            .and_then(Option::as_ref);
        let prefix = new.is_some_and(|st| same_prefix(&retained, &current, before, st));
        if prefix && new.is_some_and(|st| st.mode == before.mode && st.size == before.size) {
            continue;
        }
        let removed = new.is_none();
        changes.push(Change {
            path: original,
            effect: e,
            retained,
            removed,
            discarded: !prefix,
            agent: e.note.agent_created,
            tracked: false,
            ignored: false,
            clean: false,
            git_has: None,
            in_stash: false,
        });
    }
    let mut decision = Decision {
        groups: BTreeMap::new(),
        valuable: 0,
    };
    if breaker {
        decision.groups.insert(
            Rule::R1,
            changes
                .into_iter()
                .filter(|c| c.discarded)
                .map(|c| c.path)
                .collect(),
        );
        return Ok(decision);
    }

    let mut repositories: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        if !c.effect.note.regenerable
            && !policy.regenerable(&c.path)
            && !git_database(&c.path)
            && let Some(root) = c
                .path
                .parent()
                .into_iter()
                .flat_map(|p| crate::rules::ancestors(p, policy.user_home.as_deref()))
                .find(|p| p.join(".git").exists())
        {
            repositories.entry(root.to_owned()).or_default().push(i);
        }
    }
    for (root, entries) in repositories {
        if let Err(error) = git_classify(&root, folder, &mut changes, &entries) {
            let _ = writeln!(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(folder.join("rules-diagnostics"))?,
                "{error:#}"
            );
        }
    }

    let untracked = changes
        .iter()
        .filter(|c| {
            c.git_has == Some(false)
                && !c.tracked
                && !c.ignored
                && c.removed
                && !c.agent
                && !c.effect.note.regenerable
                && !policy.regenerable(&c.path)
        })
        .count();
    for c in changes {
        if c.effect.note.regenerable || policy.regenerable(&c.path) {
            continue;
        }
        let database = git_database(&c.path);
        let candidates = [
            (Rule::R2, c.tracked && c.git_has == Some(false)),
            (
                Rule::R3,
                c.git_has == Some(false)
                    && !c.tracked
                    && !c.ignored
                    && c.removed
                    && !c.agent
                    && untracked > policy.max_untracked,
            ),
            (
                Rule::R4,
                !c.agent && !c.in_stash && policy.precious(&c.path),
            ),
            (Rule::R5, database && !c.effect.note.git),
            (Rule::R6, policy.irreplaceable(&c.path)),
        ];
        if let Some((rule, _)) = candidates
            .into_iter()
            .find(|(_, applies)| c.discarded && *applies)
        {
            decision.groups.entry(rule).or_default().insert(c.path);
        } else if !c.agent
            && !c.in_stash
            && !database
            && (!c.clean || c.removed)
            && (!c.ignored || policy.config_file(&c.path))
        {
            decision.valuable += 1;
        }
    }
    Ok(decision)
}

fn git(root: &Path, folder: &Path, args: &[&str], input: &[u8]) -> Result<(i32, Vec<u8>)> {
    let binary = git_binary().ok_or_else(|| io::Error::other("system git is unavailable"))?;
    let scratch = folder.join("git-query");
    let mut data = journal::private_file(&scratch)?;
    data.set_len(0)?;
    data.write_all(input)?;
    data.rewind()?;
    let result = folder.join("git-result");
    let output = journal::private_file(&result)?;
    output.set_len(0)?;
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    let mut cmd = command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::from(data))
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let code = loop {
        if let Some(status) = cmd.try_wait()? {
            break status.code().unwrap_or(125);
        }
        if Instant::now() >= deadline || fs::metadata(&result)?.len() > 64 * 1024 * 1024 {
            let _ = cmd.kill();
            cmd.wait()?;
            bail!("git query exceeded its time or output limit");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let mut output = Vec::new();
    File::open(&result)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut output)?;
    fs::remove_file(&scratch)?;
    fs::remove_file(&result)?;
    if output.len() > 64 * 1024 * 1024 {
        bail!("git query exceeded its output limit");
    }
    Ok((code, output))
}

fn git_classify(
    root: &Path,
    folder: &Path,
    changes: &mut [Change<'_>],
    entries: &[usize],
) -> Result<()> {
    let names: Vec<_> = entries
        .iter()
        .map(|&i| {
            changes[i]
                .path
                .strip_prefix(root)
                .map(|p| p.as_os_str().as_bytes())
        })
        .collect::<Result<_, _>>()?;
    let (code, index) = git(root, folder, &["ls-files", "-sz"], b"")?;
    if code != 0 {
        bail!("git index query failed");
    }
    let index = git_entries(&index);
    let index_blobs: HashSet<_> = index.values().flatten().copied().collect();
    let (code, head) = git(root, folder, &["ls-tree", "-rz", "HEAD"], b"")?;
    if code != 0
        && git(
            root,
            folder,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            b"",
        )?
        .0 != 1
    {
        bail!("git HEAD query failed");
    }
    let head = git_entries(&head);
    let head_blobs: HashSet<_> = head.values().flatten().copied().collect();
    let stdin: Vec<_> = names
        .iter()
        .flat_map(|p| p.iter().copied().chain([0]))
        .collect();
    let (code, ignored) = git(root, folder, &["check-ignore", "-z", "--stdin"], &stdin)?;
    if code > 1 {
        bail!("git ignore query failed");
    }
    let ignored: HashSet<_> = ignored.split(|&b| b == 0).collect();
    let (code, stashes) = git(
        root,
        folder,
        &["reflog", "show", "--format=%H %P", "refs/stash"],
        b"",
    )?;
    if code != 0
        && git(
            root,
            folder,
            &["show-ref", "--verify", "--quiet", "refs/stash"],
            b"",
        )?
        .0 != 1
    {
        bail!("git stash reflog query failed");
    }
    let mut trees = Vec::new();
    for stash in stashes.split(|&b| b == b'\n').filter(|row| !row.is_empty()) {
        // Parents 2 and 3 retain the index and untracked files. The first is
        // the old base commit, which isn't itself a stash snapshot.
        for hash in stash
            .split(|&b| b == b' ')
            .enumerate()
            .filter_map(|(i, h)| (i != 1).then_some(h))
        {
            if !matches!(hash.len(), 40 | 64) || !hash.iter().all(u8::is_ascii_hexdigit) {
                bail!("invalid git stash reflog output");
            }
            trees.extend(hash);
            trees.extend_from_slice(b"^{tree}\n");
        }
    }
    let stash_objects = if trees.is_empty() {
        Vec::new()
    } else {
        let (code, objects) = git(
            root,
            folder,
            &["rev-list", "--objects", "--no-object-names", "--stdin"],
            &trees,
        )?;
        if code != 0 {
            bail!("git stash tree query failed");
        }
        objects
    };
    let stash_blobs: HashSet<_> = stash_objects.split(|&b| b == b'\n').collect();
    let attributes = git_attributes(root, folder, &stdin, names.len())?;
    let mut hashes_input = Vec::new();
    let mut staged = Vec::new();
    let mut queries = Vec::with_capacity(entries.len());
    let mut next = 0;
    for (position, &i) in entries.iter().enumerate() {
        let c = &changes[i];
        let before = c
            .effect
            .note
            .before
            .as_ref()
            .ok_or_else(|| io::Error::other("missing Git pre-image state"))?;
        let symlink = before.mode & libc::S_IFMT == libc::S_IFLNK;
        let mut source = c.retained.clone();
        if symlink {
            source = stage_hash(
                folder,
                &mut staged,
                fs::read_link(&c.retained)?.as_os_str().as_bytes(),
            )?;
        }
        if !fs::symlink_metadata(&source)?.is_file() {
            bail!("git pre-image is not a regular file");
        }
        hash_path(&mut hashes_input, &source)?;
        let attributes = &attributes[position];
        let mut unsafe_to_compare = !symlink && attributes.external;
        let normalize = !symlink && attributes.crlf;
        let index = next;
        next += 1;
        let mut normalized = None;
        if normalize && before.size <= 16 * 1024 * 1024 {
            let bytes = fs::read(&source)?;
            if bytes.windows(2).any(|w| w == b"\r\n") && !bytes.contains(&0) {
                let mut unix = Vec::with_capacity(bytes.len());
                for (i, &byte) in bytes.iter().enumerate() {
                    if byte != b'\r' || bytes.get(i + 1) != Some(&b'\n') {
                        unix.push(byte);
                    }
                }
                let normalized_source = stage_hash(folder, &mut staged, &unix)?;
                hash_path(&mut hashes_input, &normalized_source)?;
                normalized = Some(next);
                next += 1;
            }
        } else if normalize {
            unsafe_to_compare = true;
        }
        queries.push((index, normalized, unsafe_to_compare));
    }
    let (code, hashes) = git(
        root,
        folder,
        &["hash-object", "--no-filters", "--stdin-paths"],
        &hashes_input,
    )?;
    for file in staged {
        fs::remove_file(file)?;
    }
    if code != 0 {
        bail!("git pre-image hash query failed");
    }
    let hashes: Vec<_> = hashes
        .split(|&b| b == b'\n')
        .filter(|s| !s.is_empty())
        .collect();
    if hashes.len() != next {
        bail!("git returned incomplete classification");
    }
    let names: Vec<_> = names.into_iter().map(<[u8]>::to_vec).collect();
    for ((&i, name), (raw, normalized, uncertain)) in entries.iter().zip(names).zip(queries) {
        let candidates = std::iter::once(hashes[raw]).chain(normalized.map(|i| hashes[i]));
        let name = name.as_slice();
        let head = head.get(name);
        let indexed = index.get(name);
        let current = &mut changes[i];
        current.tracked = indexed.is_some() || head.is_some();
        current.clean = candidates.clone().any(|hash| {
            head.is_some_and(|v| v.contains(&hash))
                && indexed.is_some_and(|v| v.len() == 1 && v.contains(&hash))
        });
        current.ignored = ignored.contains(name);
        current.in_stash = candidates.clone().any(|hash| stash_blobs.contains(hash));
        let known = candidates
            .clone()
            .any(|hash| index_blobs.contains(hash) || head_blobs.contains(hash))
            || current.in_stash;
        current.git_has = (known || !uncertain).then_some(known);
    }
    Ok(())
}

// Both Git outputs are NUL-terminated and put the pathname after the first tab.
fn git_entries(input: &[u8]) -> BTreeMap<&[u8], Vec<&[u8]>> {
    let mut entries: BTreeMap<&[u8], Vec<&[u8]>> = BTreeMap::new();
    for entry in input.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if let Some(tab) = entry.iter().position(|&b| b == b'\t')
            && let Some(hash) = entry[..tab].split(|&b| b == b' ').find(|word| {
                matches!(word.len(), 40 | 64) && word.iter().all(u8::is_ascii_hexdigit)
            })
        {
            entries.entry(&entry[tab + 1..]).or_default().push(hash);
        }
    }
    entries
}

struct Attributes {
    crlf: bool,
    external: bool,
}

fn git_attributes(
    root: &Path,
    folder: &Path,
    stdin: &[u8],
    count: usize,
) -> Result<Vec<Attributes>> {
    let (code, config) = git(root, folder, &["config", "--get", "core.autocrlf"], b"")?;
    if code > 1 {
        bail!("git line-ending config query failed");
    }
    let autocrlf = matches!(
        config.trim_ascii(),
        b"true" | b"yes" | b"on" | b"1" | b"input"
    );
    let attrs = ["text", "eol", "filter", "working-tree-encoding", "ident"];
    let mut args = vec!["check-attr", "-z", "--stdin"];
    args.extend(attrs);
    let (code, res) = git(root, folder, &args, stdin)?;
    let Some(res) = res.strip_suffix(b"\0") else {
        bail!("git attributes query failed");
    };
    let fields: Vec<_> = res.split(|&b| b == 0).collect();
    if code != 0 || fields.len() != count * attrs.len() * 3 {
        bail!("git returned incomplete worktree attributes");
    }
    let enabled = |value: &[u8]| !matches!(value, b"unspecified" | b"unset");
    Ok(fields
        .as_chunks::<15>()
        .0
        .iter()
        .map(|row| Attributes {
            crlf: row[2] != b"unset" && (enabled(row[2]) || enabled(row[5]) || autocrlf),
            external: [row[8], row[11], row[14]].into_iter().any(enabled),
        })
        .collect())
}

fn stage_hash(folder: &Path, staged: &mut Vec<PathBuf>, bytes: &[u8]) -> io::Result<PathBuf> {
    let path = folder.join(format!("git-hash-{}-{}", staged.len(), journal::now()));
    let mut file = journal::private_file(&path)?;
    file.write_all(bytes)?;
    staged.push(path.clone());
    Ok(path)
}

fn hash_path(input: &mut Vec<u8>, path: &Path) -> io::Result<()> {
    // hash-object accepts Git's C-quoted filenames, including newlines and non-UTF-8 bytes.
    input.push(b'"');
    for b in path.as_os_str().as_bytes() {
        write!(input, "\\{b:03o}")?;
    }
    input.extend_from_slice(b"\"\n");
    Ok(())
}
