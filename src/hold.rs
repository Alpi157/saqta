//! Durable hold and release plans. Filesystem changes go through the M5 undo engine.
use crate::{
    classify::{self, Birth, Decision},
    journal::{self, Detail, Effect, Journal, Record, State},
    paths,
    restore::{self, Apply},
    rules::Rule,
    sys,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::{CString, OsString},
    fs::{self, OpenOptions},
    io::{self, IsTerminal},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

type Posts = BTreeMap<String, Option<State>>;

#[derive(Serialize, Deserialize)]
struct Hold {
    rule: Rule,
    names: BTreeSet<String>,
    effects: BTreeSet<u64>,
    posts: Posts,
    agent: Vec<Effect>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    interrupted_locks: BTreeSet<String>,
    detached: bool,
}

pub(crate) fn directory(home: &Path, id: u64) -> PathBuf {
    home.join("holds").join(id.to_string())
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(journal::read_json(path)?)
}

pub fn pending(home: &Path, id: u64) -> bool {
    let folder = directory(home, id);
    folder.join("plan").exists() && !folder.join("ready").exists()
}

pub fn protected(home: &Path, id: u64) -> bool {
    directory(home, id).join("plan").exists()
}

pub fn rule_for(home: &Path, id: u64) -> Result<Option<Rule>> {
    let folder = directory(home, id);
    if folder.join("ready").exists() {
        return Ok(Some(read::<Hold>(&folder.join("plan"))?.rule));
    }
    Ok(None)
}

pub fn restored_sequences(home: &Path, id: u64) -> Result<BTreeSet<u64>> {
    let folder = directory(home, id);
    if folder.join("ready").exists() {
        return Ok(read::<Hold>(&folder.join("plan"))?.effects);
    }
    Ok(BTreeSet::new())
}

pub fn exclude_restored(home: &Path, record: &mut Record, posts: &mut Posts) -> Result<()> {
    let folder = directory(home, record.meta.id);
    if folder.join("ready").exists() {
        let plan: Hold = read(&folder.join("plan"))?;
        record.effects.retain(|e| !plan.effects.contains(&e.seq));
        let restored: Posts = read(&folder.join("restored-posts"))?;
        for recorded in record.effects.iter().flat_map(|e| [&e.path, &e.path2]) {
            if let Some(state) = restored.get(recorded) {
                posts.insert(recorded.clone(), state.clone());
            }
        }
    }
    Ok(())
}

fn session_commands(home: &Path, session: &str, before: u64) -> Result<Vec<(u64, PathBuf)>> {
    let mut commands = Vec::new();
    for entry in fs::read_dir(home.join("cmd"))? {
        let folder = entry?.path();
        let Some(id) = folder
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if id < before
            && let Ok(meta) = read::<journal::Meta>(&folder.join("meta"))
            && meta.session.as_deref() == Some(session)
        {
            commands.push((id, folder));
        }
    }
    commands.sort_by_key(|(id, _)| *id);
    Ok(commands)
}

pub fn prior(home: &Path, session: &str, id: u64) -> Result<HashSet<Birth>> {
    Ok(session_commands(home, session, id)?
        .into_iter()
        .filter_map(|(_, p)| read::<HashSet<Birth>>(&p.join("created")).ok())
        .flatten()
        .collect())
}

pub fn notices(home: &Path, session: &str) {
    if let Ok(commands) = session_commands(home, session, u64::MAX) {
        for (id, _) in commands {
            let folder = directory(home, id);
            if folder.join("ready").exists()
                && read::<Hold>(&folder.join("plan")).is_ok_and(|p| p.detached)
                && OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(folder.join("notice-read"))
                    .is_ok()
            {
                sys::write(2, format!("[saqta {id}] Restored files discarded by a background job (details: saqta show {id})\n").as_bytes());
            }
        }
    }
}

pub fn open_holds(home: &Path) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    match fs::read_dir(home.join("holds")) {
        Ok(entries) => {
            for entry in entries {
                let folder = entry?.path();
                if folder.join("ready").exists()
                    && !folder.join("released").exists()
                    && let Some(id) = folder
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.parse().ok())
                {
                    ids.push(id);
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    ids.sort_unstable();
    Ok(ids)
}

fn root() -> io::Result<std::os::fd::OwnedFd> {
    sys::open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)
}

fn open_dir(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    sys::open_at(
        libc::AT_FDCWD,
        &CString::new(path.as_os_str().as_bytes())?,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )
}

fn guarded_posts(root: i32, names: impl IntoIterator<Item = String>) -> Result<Posts> {
    let mut posts = Posts::new();
    let mut remaining: Vec<_> = names.into_iter().collect();
    while let Some(name) = remaining.pop() {
        if posts.contains_key(&name) {
            continue;
        }
        let path = journal::path(&name)?;
        let st = restore::current_secure(root, &path)?;
        if st.as_ref().is_some_and(State::directory) {
            // Pin the directory while enumerating immediate entries. Every child
            // also gets its own no-symlink post-state check before restoration.
            let relative = CString::new(path.strip_prefix("/")?.as_os_str().as_bytes())?;
            let dir = sys::open_restore_parent(root, &relative)?;
            for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
                remaining.push(journal::display(&path.join(entry?.file_name())));
            }
        }
        posts.insert(name, st);
    }
    Ok(posts)
}

pub fn seal_posts(root: i32, effects: &[Effect], output: &Path) -> Result<()> {
    if !output.exists() {
        let names = effects
            .iter()
            .flat_map(|e| [&e.path, &e.path2])
            .filter(|p| !p.is_empty())
            .cloned();
        let posts = guarded_posts(root, names)?;
        journal::synced_json_write(output, &posts)?;
    }
    Ok(())
}

fn snapshot_plan(
    root: i32,
    target: &Path,
    base: &BTreeSet<String>,
    posts: &Posts,
    in_place: &BTreeSet<String>,
) -> Result<Vec<Effect>> {
    let dir = open_dir(target)?;
    let mut ordered = base
        .iter()
        .map(|s| Ok((journal::path(s)?, s)))
        .collect::<Result<Vec<_>>>()?;
    ordered.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut parents = BTreeSet::new();
    let mut result = Vec::new();
    for (path, name) in ordered {
        if path.ancestors().skip(1).any(|p| parents.contains(p)) {
            continue;
        }
        let Some(expected) = posts.get(name) else {
            bail!("missing hold post-state for {name}");
        };
        if restore::current_secure(root, &path)? != *expected {
            bail!("path changed while preparing hold: {name}");
        }
        let present = expected.is_some();
        let mut e = Effect {
            seq: 0,
            time_ns: journal::now(),
            pid: sys::pid() as u32,
            op: if in_place.contains(name) {
                "modify"
            } else if present {
                "delete"
            } else {
                "create"
            }
            .into(),
            path: name.clone(),
            path2: String::new(),
            quarantine: String::new(),
            flags: 0,
            note: Detail::default(),
        };
        if present {
            e.quarantine = format!("version-{}", result.len() + 1);
            restore::snapshot_entry(root, &path, dir.as_raw_fd(), &e.quarantine)?;
            e.note.before = if in_place.contains(name) {
                expected.clone()
            } else {
                journal::state(&target.join(&e.quarantine))?
            };
            e.note.kept = "copy".into();
        }
        // A copied directory already contains every descendant. An absent
        // ancestor subsumes the descendants too.
        parents.insert(path);
        result.push(e);
    }
    result.reverse();
    for (i, e) in result.iter_mut().enumerate() {
        e.seq = i as u64 + 1;
    }
    Ok(result)
}

fn select(record: &Record, decision: &Decision, after: u64) -> Result<BTreeSet<u64>> {
    let all = decision.rule() == Some(Rule::R1);
    let mut paths = decision.names();
    let mut selected = BTreeSet::new();
    let mut subtrees = BTreeSet::new();
    let effects = record
        .effects
        .iter()
        .filter(|e| (all || e.seq > after) && classify::applied(record, e))
        .map(|e| {
            Ok((
                e,
                journal::path(&e.path)?,
                (!e.path2.is_empty())
                    .then(|| journal::path(&e.path2))
                    .transpose()?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    loop {
        let previous = selected.len();
        for (e, first, second) in &effects {
            if selected.contains(&e.seq) {
                continue;
            }
            let directory = e.note.before.as_ref().is_some_and(State::directory);
            let overlaps = |p: &Path| {
                paths.contains(p)
                    || p.ancestors().skip(1).any(|p| subtrees.contains(p))
                    || (directory
                        && paths
                            .range(p.to_owned()..)
                            .next()
                            .is_some_and(|child| child.starts_with(p)))
            };
            if (all && e.op == "rmdir")
                || overlaps(first)
                || second.as_ref().is_some_and(|p| overlaps(p))
            {
                selected.insert(e.seq);
                paths.insert(first.clone());
                paths.extend(second.iter().cloned());
                if directory && matches!(e.op.as_str(), "rename" | "exchange") {
                    subtrees.insert(first.clone());
                    subtrees.extend(second.iter().cloned());
                }
            }
        }
        if previous == selected.len() {
            break;
        }
    }
    Ok(selected)
}

fn names(record: &Record, effects: &BTreeSet<u64>) -> BTreeSet<String> {
    record
        .effects
        .iter()
        .filter(|e| effects.contains(&e.seq))
        .flat_map(|e| [&e.path, &e.path2])
        .filter(|p| !p.is_empty())
        .cloned()
        .collect()
}

pub fn start(
    j: &Journal,
    record: Record,
    decision: &Decision,
    detached: bool,
    after: u64,
) -> Result<()> {
    let Some(rule) = decision.rule() else {
        return Ok(());
    };
    let lock = journal::private_file(&j.folder.join("undo.lock"))?;
    sys::flock_exclusive(lock.as_raw_fd(), true)?;
    let mut effects = select(&record, decision, after)?;
    let agent_names = names(&record, &effects);
    // Reverse surviving Git lock creations without following old renames into
    // the index. The undo engine keeps the locks in the recovery directory.
    let interrupted_locks = names(&record, &decision.interrupted_locks);
    effects.extend(&decision.interrupted_locks);
    let root = root()?;
    let posts = guarded_posts(
        root.as_raw_fd(),
        agent_names.union(&interrupted_locks).cloned(),
    )?;
    let observed = journal::post_states(&j.folder)?;
    for (name, state) in &posts {
        if observed.get(name).is_some_and(|p| p != state) {
            bail!("changed after command: {name}");
        }
    }
    let dir = directory(&j.home, j.meta.id);
    journal::private_dir(&j.home.join("holds"))?;
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let agent_dir = dir.join("agent");
    journal::private_dir(&agent_dir)?;
    let mut in_place = BTreeSet::new();
    let mut replaced = BTreeSet::new();
    for e in record.effects.iter().filter(|e| effects.contains(&e.seq)) {
        if matches!(e.op.as_str(), "modify" | "size")
            && e.note
                .before
                .as_ref()
                .zip(posts.get(&e.path).and_then(Option::as_ref))
                .is_some_and(|(before, post)| {
                    post.same_inode(before) && post.mode & libc::S_IFMT == libc::S_IFREG
                })
        {
            in_place.insert(e.path.clone());
        } else {
            replaced.insert(e.path.clone());
            replaced.insert(e.path2.clone());
        }
    }
    in_place.retain(|name| !replaced.contains(name));
    let agent = snapshot_plan(
        root.as_raw_fd(),
        &agent_dir,
        &agent_names,
        &posts,
        &in_place,
    )?;
    journal::synced_json_write(
        &dir.join("plan"),
        &Hold {
            rule,
            names: decision
                .names()
                .iter()
                .map(|p| journal::display(p))
                .collect(),
            effects,
            posts,
            agent,
            interrupted_locks,
            detached,
        },
    )?;
    resume(&j.home, record)
}

pub fn resume(home: &Path, mut record: Record) -> Result<()> {
    let id = record.meta.id;
    let dir = directory(home, id);
    let plan: Hold = read(&dir.join("plan"))?;
    record.effects.retain(|e| plan.effects.contains(&e.seq));
    restore::apply(
        record,
        Apply {
            home,
            folder: &dir,
            backup_path: dir.join("restore"),
            qpath: home.join("q/main").join(id.to_string()),
            posts: plan.posts,
            dry_run: false,
            force: false,
            verbose: false,
            resume: true,
            automatic: true,
            seal: Some(&dir.join("restored-posts")),
        },
    )?;
    journal::synced_json_write(&dir.join("ready"), &journal::now())?;
    Ok(())
}

pub fn release(id: u64) -> Result<i32> {
    if std::env::var_os("SAQTA_SUPERVISED").is_some()
        || crate::supervisor::in_ancestry().unwrap_or(true)
        || (!io::stdin().is_terminal() && !crate::rules::release_without_terminal()?)
    {
        bail!("release must be run by a person in their own terminal");
    }
    let home = paths::home()?;
    let folder = directory(&home, id);
    let original = home.join("cmd").join(id.to_string());
    let lock = journal::private_file(&original.join("undo.lock"))?;
    sys::flock_exclusive(lock.as_raw_fd(), true).context("another restore is in progress")?;
    if !folder.join("ready").exists() || folder.join("released").exists() {
        bail!("command {id} has no open hold");
    }
    let plan: Hold = read(&folder.join("plan"))?;
    let mut restored: Posts = read(&folder.join("restored-posts"))?;
    restored.retain(|name, _| !plan.interrupted_locks.contains(name));
    let mut record = journal::load(&home, id)?;
    let names = plan.agent.iter().map(|e| e.path.clone()).collect();
    let root = root()?;
    let release = folder.join("release");
    journal::private_dir(&release)?;
    let release_id: u64 = if release.join("id").exists() {
        read(&release.join("id"))?
    } else {
        for (name, state) in &restored {
            if restore::current_secure(root.as_raw_fd(), &journal::path(name)?)? != *state {
                bail!("changed after automatic restore: {name}. Hold kept.");
            }
        }
        let args = [OsString::from("saqta release"), id.to_string().into()];
        let mut j = Journal::start(home.clone(), &args, &std::env::current_dir()?, -1)?;
        let in_place = plan
            .agent
            .iter()
            .filter(|e| e.op == "modify")
            .map(|e| e.path.clone())
            .collect();
        for e in snapshot_plan(
            root.as_raw_fd(),
            &j.quarantine,
            &names,
            &restored,
            &in_place,
        )? {
            j.append(e)?;
        }
        journal::synced_json_write(&j.folder.join("release-from"), &id)?;
        journal::synced_json_write(&release.join("id"), &j.meta.id)?;
        j.meta.id
    };
    let j = Journal::reopen(home.clone(), release_id)?;
    record.effects = plan.agent;
    record.outcomes.clear();
    restore::apply(
        record,
        Apply {
            home: &home,
            folder: &release,
            backup_path: release.join("recovery"),
            qpath: folder.join("agent"),
            posts: restored,
            dry_run: false,
            force: false,
            verbose: false,
            resume: true,
            automatic: true,
            seal: Some(&release.join("released-posts")),
        },
    )?;
    let posts: Posts = read(&release.join("released-posts"))?;
    let posts: Vec<_> = posts
        .into_iter()
        .map(|(path, state)| journal::Post { path, state })
        .collect();
    journal::synced_json_write(&j.folder.join("recovered-post.json"), &posts)?;
    j.end(0)?;
    journal::synced_json_write(&folder.join("released"), &release_id)?;
    crate::output::Progress::new(io::stdout()).line(format_args!(
        "released command {id} (undo: saqta undo {release_id})"
    ));
    Ok(0)
}
