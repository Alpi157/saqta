//! Compare retained versions with the command's post-state and Git's reachable copies.
mod index;
use crate::{
    journal::{self, Effect, Record},
    rules::{Breaker, Policy, Rule},
};
use anyhow::{Result, bail};
use index::Index;
pub use index::Indexes;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

pub fn git_database(path: &Path) -> bool {
    path.iter().any(|p| p == ".git")
}

fn repository(path: &Path, policy: &Policy) -> Option<PathBuf> {
    path.parent()
        .into_iter()
        .flat_map(|p| crate::rules::ancestors(p, policy.user_home.as_deref()))
        .find(|p| p.join(".git").exists())
        .map(Path::to_owned)
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
    replaced: bool,
    current: Option<(PathBuf, journal::State)>,
    git_post_matches: bool,
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
    pub interrupted_locks: BTreeSet<u64>,
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
    indexes: &Indexes,
    breaker: Option<&Breaker>,
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
    let root = crate::sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
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
        if e.seq <= after && breaker.is_none() || !seen.insert(original.clone()) {
            continue;
        }
        let mut current = path;
        for m in moves.iter().filter(|m| m.seq > e.seq) {
            move_name(&mut current, m, false)?;
        }
        let retained = quarantine.join(&e.quarantine);
        let new = match posts.get(&journal::display(&current)) {
            Some(state) => state.clone(),
            None => crate::restore::current_secure(root.as_raw_fd(), &current)?,
        };
        let prefix = new
            .as_ref()
            .is_some_and(|st| same_prefix(&retained, &current, before, st));
        if prefix
            && new
                .as_ref()
                .is_some_and(|st| st.mode == before.mode && st.size == before.size)
        {
            continue;
        }
        changes.push(Change {
            path: original,
            effect: e,
            retained,
            removed: new.is_none(),
            current: new.map(|state| (current, state)),
            replaced: !prefix,
            git_post_matches: false,
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
        interrupted_locks: BTreeSet::new(),
    };
    if breaker.is_some() {
        decision.groups.insert(Rule::R1, BTreeSet::new());
    }

    let mut repositories: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        if !c.effect.note.regenerable
            && !policy.regenerable(&c.path)
            && !git_database(&c.path)
            && let Some(root) = repository(&c.path, policy)
        {
            repositories.entry(root.to_owned()).or_default().push(i);
        }
    }
    for (root, entries) in repositories {
        if let Err(error) = git_classify(&root, folder, &mut changes, &entries, indexes.get(&root))
        {
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
        let regenerable = c.effect.note.regenerable || policy.regenerable(&c.path);
        let database = git_database(&c.path);
        let candidates = [
            (
                Rule::R2,
                c.tracked && c.git_has == Some(false) && (c.removed || c.git_post_matches),
            ),
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
                (c.replaced
                    || c.effect.op == "delete"
                    || c.effect.flags & libc::O_TRUNC as u64 != 0)
                    && !c.agent
                    && !c.in_stash
                    && policy.precious(&c.path),
            ),
            (Rule::R5, c.replaced && database && !c.effect.note.git),
            (Rule::R6, c.replaced && policy.irreplaceable(&c.path)),
        ];
        let rule = candidates
            .into_iter()
            .find_map(|(rule, applies)| (!regenerable && applies).then_some(rule));
        if let Some(breaker) = breaker {
            if (c.removed && !c.agent && c.git_has != Some(true))
                || breaker.counted(&c.path)
                || rule.is_some()
            {
                decision.groups.entry(Rule::R1).or_default().insert(c.path);
            }
        } else if let Some(rule) = rule {
            decision.groups.entry(rule).or_default().insert(c.path);
        } else if !regenerable
            && !c.agent
            && !c.in_stash
            && !database
            && !c.clean
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
    initial: Option<&Index>,
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
    let final_index;
    let index = if let Some(current) = initial.and_then(|index| index.current_hashes(root)) {
        current
    } else {
        let (code, data) = git(root, folder, &["ls-files", "-sz"], b"")?;
        if code != 0 {
            bail!("git index query failed");
        }
        final_index = data;
        git_entries(&final_index)
    };
    let index_blobs: HashSet<_> = index.values().flatten().copied().collect();
    let head = head_tree(root, folder)?;
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
    let stash_objects = stash_objects(root, folder)?;
    let stash_blobs: HashSet<_> = stash_objects.split(|&b| b == b'\n').collect();
    let attributes = git_attributes(root, folder, &stdin, names.len())?;
    let mut batch = HashBatch::new(folder);
    let mut queries = Vec::with_capacity(entries.len());
    for (position, &i) in entries.iter().enumerate() {
        let c = &changes[i];
        let before = c
            .effect
            .note
            .before
            .as_ref()
            .ok_or_else(|| io::Error::other("missing Git pre-image state"))?;
        let attributes = &attributes[position];
        let name = names[position];
        let stat_match = initial
            .and_then(|v| v.entry(name))
            .filter(|v| v.stat_matches(before));
        let old = if stat_match.is_some() {
            None
        } else {
            Some(batch.add(&c.retained, before, attributes)?)
        };
        let tracked = initial.is_some_and(|v| v.entry(name).is_some())
            || index.contains_key(name)
            || head.contains_key(name);
        let new = c
            .current
            .as_ref()
            .filter(|(_, s)| {
                tracked
                    && stat_match.is_none()
                    && matches!(s.mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK)
            })
            .map(|(p, s)| batch.add(p, s, attributes))
            .transpose()?;
        queries.push((old, new));
    }
    let hashes = batch.run(root)?;
    let names: Vec<_> = names.into_iter().map(<[u8]>::to_vec).collect();
    for ((&i, name), (old, new)) in entries.iter().zip(names).zip(queries) {
        let name = name.as_slice();
        let head = head.get(name);
        let indexed = index.get(name);
        let previous = initial.and_then(|index| index.entry(name));
        let current = &mut changes[i];
        current.tracked = previous.is_some() || indexed.is_some() || head.is_some();
        let mut old_hashes = old.iter().flat_map(|query| query.hashes(&hashes)).chain(
            previous
                .filter(|_| old.is_none())
                .map(|v| v.hash.as_slice()),
        );
        let previous_clean = old_hashes
            .clone()
            .any(|hash| initial.is_some_and(|v| v.has_at(name, hash)));
        let index_changed = previous
            .is_some_and(|entry| indexed.is_none_or(|v| !v.contains(&entry.hash.as_slice())));
        current.clean = (previous_clean && index_changed)
            || old_hashes.clone().any(|hash| {
                head.is_some_and(|v| v.contains(&hash))
                    && indexed.is_some_and(|v| v.len() == 1 && v.contains(&hash))
            });
        current.ignored = ignored.contains(name);
        current.in_stash = old_hashes.clone().any(|hash| stash_blobs.contains(hash));
        let known = previous_clean
            || old_hashes.any(|hash| index_blobs.contains(hash) || head_blobs.contains(hash))
            || current.in_stash;
        current.git_has = (known || !old.as_ref().is_some_and(|v| v.uncertain)).then_some(known);
        current.git_post_matches = new.is_some_and(|query| {
            query.hashes(&hashes).any(|hash| {
                indexed.is_some_and(|v| v.contains(&hash))
                    || head.is_some_and(|v| v.contains(&hash))
            })
        });
    }
    Ok(())
}

/// A deletion whose index stat could not prove that Git has the retained bytes.
pub struct GitDeletion {
    pub root: PathBuf,
    pub path: PathBuf,
    pub before: journal::State,
    pub retained: PathBuf,
    pub tracked: bool,
}

pub fn confirm_deletions(
    deletes: Vec<GitDeletion>,
    indexes: &Indexes,
    folder: &Path,
) -> Vec<GitDeletion> {
    let mut repos: BTreeMap<PathBuf, Vec<GitDeletion>> = BTreeMap::new();
    for deletion in deletes {
        repos
            .entry(deletion.root.clone())
            .or_default()
            .push(deletion);
    }
    let mut confirmed = Vec::new();
    for (root, candidates) in repos {
        let result = (|| -> Result<Vec<bool>> {
            let index = indexes
                .get(&root)
                .ok_or_else(|| io::Error::other("missing initial Git index"))?;
            let names = candidates
                .iter()
                .map(|c| c.path.strip_prefix(&root).map(|p| p.as_os_str().as_bytes()))
                .collect::<Result<Vec<_>, _>>()?;
            let input = names
                .iter()
                .flat_map(|p| p.iter().copied().chain([0]))
                .collect::<Vec<_>>();
            let attrs = git_attributes(&root, folder, &input, names.len())?;
            let mut batch = HashBatch::new(folder);
            let queries = candidates
                .iter()
                .zip(&attrs)
                .map(|(c, attrs)| batch.add(&c.retained, &c.before, attrs))
                .collect::<Result<Vec<_>>>()?;
            let hashes = batch.run(&root)?;
            let objects = stash_objects(&root, folder)?;
            let stash: HashSet<_> = objects.split(|&b| b == b'\n').collect();
            let head = if candidates.iter().any(|c| c.tracked) {
                head_tree(&root, folder)?
            } else {
                Vec::new()
            };
            let head = git_entries(&head);
            let head_blobs: HashSet<_> = head.values().flatten().copied().collect();
            Ok(queries
                .iter()
                .zip(names)
                .zip(&candidates)
                .map(|((query, name), c)| {
                    !query.uncertain
                        && !query.hashes(&hashes).any(|hash| {
                            (c.tracked && (index.has_at(name, hash) || head_blobs.contains(hash)))
                                || stash.contains(hash)
                        })
                })
                .collect())
        })();
        match result {
            Ok(removed) => confirmed.extend(
                candidates
                    .into_iter()
                    .zip(removed)
                    .filter_map(|(deletion, removed)| removed.then_some(deletion)),
            ),
            Err(error) => {
                if let Ok(mut log) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(folder.join("rules-diagnostics"))
                {
                    let _ = writeln!(log, "{error:#}");
                }
            }
        }
    }
    confirmed
}

fn head_tree(root: &Path, folder: &Path) -> Result<Vec<u8>> {
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
    Ok(head)
}

fn stash_objects(root: &Path, folder: &Path) -> Result<Vec<u8>> {
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
    if trees.is_empty() {
        Ok(Vec::new())
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
        Ok(objects)
    }
}
struct HashQuery {
    raw: usize,
    normalized: Option<usize>,
    uncertain: bool,
}

impl HashQuery {
    fn hashes<'a>(&self, values: &'a [Vec<u8>]) -> impl Iterator<Item = &'a [u8]> + Clone {
        std::iter::once(values[self.raw].as_slice())
            .chain(self.normalized.map(|i| values[i].as_slice()))
    }
}

struct HashBatch<'a> {
    folder: &'a Path,
    input: Vec<u8>,
    staged: Vec<PathBuf>,
    next: usize,
}

impl<'a> HashBatch<'a> {
    fn new(folder: &'a Path) -> Self {
        Self {
            folder,
            input: Vec::new(),
            staged: Vec::new(),
            next: 0,
        }
    }
    fn add(
        &mut self,
        path: &Path,
        state: &journal::State,
        attrs: &Attributes,
    ) -> Result<HashQuery> {
        let symlink = state.mode & libc::S_IFMT == libc::S_IFLNK;
        let source = if symlink {
            stage_hash(
                self.folder,
                &mut self.staged,
                fs::read_link(path)?.as_os_str().as_bytes(),
            )?
        } else {
            open_regular(path)?;
            path.to_owned()
        };
        let raw = self.next;
        self.next += 1;
        hash_path(&mut self.input, &source)?;
        let mut uncertain = !symlink && attrs.external;
        let mut normalized = None;
        if !symlink && attrs.crlf {
            if state.size <= 16 * 1024 * 1024 {
                let mut bytes = Vec::new();
                open_regular(&source)?
                    .take(16 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > 16 * 1024 * 1024 {
                    uncertain = true;
                } else if bytes.windows(2).any(|w| w == b"\r\n") && !bytes.contains(&0) {
                    let unix = bytes
                        .iter()
                        .enumerate()
                        .filter_map(|(i, &b)| {
                            (b != b'\r' || bytes.get(i + 1) != Some(&b'\n')).then_some(b)
                        })
                        .collect::<Vec<_>>();
                    let source = stage_hash(self.folder, &mut self.staged, &unix)?;
                    hash_path(&mut self.input, &source)?;
                    normalized = Some(self.next);
                    self.next += 1;
                }
            } else {
                uncertain = true;
            }
        }
        Ok(HashQuery {
            raw,
            normalized,
            uncertain,
        })
    }
    fn run(self, root: &Path) -> Result<Vec<Vec<u8>>> {
        if self.input.is_empty() {
            return Ok(Vec::new());
        }
        let (code, hashes) = git(
            root,
            self.folder,
            &["hash-object", "--no-filters", "--stdin-paths"],
            &self.input,
        )?;
        if code != 0 {
            bail!("git blob hash query failed");
        }
        let hashes = hashes
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        if hashes.len() != self.next {
            bail!("git returned incomplete classification");
        }
        Ok(hashes)
    }
}

impl Drop for HashBatch<'_> {
    fn drop(&mut self) {
        for file in &self.staged {
            let _ = fs::remove_file(file);
        }
    }
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
