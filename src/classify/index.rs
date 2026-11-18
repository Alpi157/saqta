//! The first index seen in each repository supplies the pre-command Git view.
use super::{git, repository};
use crate::{
    journal::{self, State},
    rules::Policy,
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashMap},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Indexes {
    roots: HashMap<PathBuf, Option<PathBuf>>,
    indexes: BTreeMap<PathBuf, Option<Index>>,
}

impl Indexes {
    pub fn get(&self, root: &Path) -> Option<&Index> {
        self.indexes.get(root)?.as_ref()
    }

    pub fn observe(
        &mut self,
        path: &Path,
        folder: &Path,
        policy: &Policy,
    ) -> Option<(PathBuf, Option<&IndexEntry>)> {
        let directory = path.parent()?;
        let root = self
            .roots
            .entry(directory.to_owned())
            .or_insert_with(|| repository(path, policy))
            .as_ref()?
            .clone();
        let index = self
            .indexes
            .entry(root.clone())
            .or_insert_with(|| Index::load(&root, folder).ok());
        let name = path.strip_prefix(&root).ok()?.as_os_str().as_bytes();
        let entry = index.as_ref()?.entry(name);
        Some((root, entry))
    }

    pub fn is_known(&self, path: &Path) -> bool {
        path.parent().is_some_and(|p| {
            self.roots.get(p).is_some_and(|r| {
                r.is_none()
                    || r.as_ref()
                        .is_some_and(|p| self.indexes.get(p).is_some_and(Option::is_some))
            })
        })
    }
}

pub struct Index {
    entries: BTreeMap<Vec<u8>, Vec<IndexEntry>>,
    state: Option<State>,
}

pub struct IndexEntry {
    pub hash: Vec<u8>,
    stage: u8,
    mtime: i64,
    mtime_ns: i64,
    size: i64,
}

impl IndexEntry {
    pub fn stat_matches(&self, state: &State) -> bool {
        self.stage == b'0'
            && (self.size, self.mtime, self.mtime_ns) == (state.size, state.mtime, state.mtime_ns)
    }
}

impl Index {
    fn load(root: &Path, folder: &Path) -> Result<Self> {
        let before = index_state(root);
        let (status, output) = git(root, folder, &["ls-files", "-sz", "--debug"], b"")?;
        let state = before.filter(|st| index_state(root).as_ref() == Some(st));
        if status != 0 {
            bail!("cannot read Git index for the circuit breaker");
        }
        let mut entries: BTreeMap<Vec<u8>, Vec<IndexEntry>> = BTreeMap::new();
        let mut pending = output.as_slice();
        while !pending.is_empty() {
            let (header, rest) = split(pending, b'\0')?;
            let (fields, name) = split(header, b'\t')?;
            let words: Vec<_> = fields.split(|&b| b == b' ').collect();
            let [_, hash, [stage]] = words.as_slice() else {
                bail!("invalid Git index header")
            };
            let mut stat = [b"".as_slice(); 5];
            pending = rest;
            for line in &mut stat {
                (*line, pending) = split(pending, b'\n')?;
            }
            let (_, timestamp) = split(stat[1], b':')?;
            let (mtime, mtime_ns) = split(timestamp, b':')?;
            let (_, size) = split(stat[4], b':')?;
            let (size, _) = split(size, b'\t')?;
            let number = |s: &[u8]| -> Result<i64> { Ok(std::str::from_utf8(s)?.trim().parse()?) };
            entries.entry(name.to_vec()).or_default().push(IndexEntry {
                hash: hash.to_vec(),
                stage: *stage,
                mtime: number(mtime)?,
                mtime_ns: number(mtime_ns)?,
                size: number(size)?,
            });
        }
        Ok(Self { entries, state })
    }

    pub fn current_hashes<'a>(&'a self, root: &Path) -> Option<BTreeMap<&'a [u8], Vec<&'a [u8]>>> {
        // Reuse the initial query while the same index file is still in place.
        // A changed index, or a worktree with no local .git/index, needs a
        // separate final view for deciding whether the current bytes are a revert.
        if self.state.is_none() || index_state(root) != self.state {
            return None;
        }
        Some(
            self.entries
                .iter()
                .map(|(name, entries)| {
                    (
                        name.as_slice(),
                        entries.iter().map(|e| e.hash.as_slice()).collect(),
                    )
                })
                .collect(),
        )
    }

    pub fn entry(&self, name: &[u8]) -> Option<&IndexEntry> {
        self.entries.get(name).and_then(|entries| entries.first())
    }

    pub fn has_at(&self, name: &[u8], hash: &[u8]) -> bool {
        self.entries
            .get(name)
            .is_some_and(|entries| entries.iter().any(|e| e.hash == hash))
    }
}

fn index_state(root: &Path) -> Option<State> {
    journal::state(&root.join(".git/index"))
        .ok()
        .flatten()
        .filter(|state| state.mode & libc::S_IFMT == libc::S_IFREG)
}

fn split(bytes: &[u8], separator: u8) -> Result<(&[u8], &[u8])> {
    let at = bytes
        .iter()
        .position(|&b| b == separator)
        .ok_or_else(|| anyhow::anyhow!("incomplete Git index record"))?;
    Ok((&bytes[..at], &bytes[at + 1..]))
}
