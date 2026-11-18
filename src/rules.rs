//! Policy, circuit-breaker accounting and bounded agent messages.
use crate::{gc, journal, paths};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(default)]
struct Settings {
    max_files: Option<usize>,
    max_bytes: Option<u64>,
    max_irreplaceable: Option<usize>,
    max_untracked: Option<usize>,
    precious: Vec<String>,
    precious_exclude: Vec<String>,
    release_without_terminal: bool,
}

fn read(path: &Path) -> io::Result<Settings> {
    use toml_edit::{DocumentMut, Item};
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "Saqta configuration must be a regular file",
        ));
    }
    let mut input = String::new();
    file.take(65537).read_to_string(&mut input)?;
    if input.len() > 65536 {
        return Err(io::Error::other("Saqta configuration exceeds 64 KiB"));
    }
    let toml = input.parse::<DocumentMut>().map_err(io::Error::other)?;
    let invalid = |key| io::Error::other(format!("invalid Saqta setting {key}"));
    let number = |key| {
        toml.get(key)
            .map(|v| {
                v.as_integer()
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or_else(|| invalid(key))
            })
            .transpose()
    };
    let patterns = |key| {
        toml.get(key)
            .map(|v: &Item| {
                v.as_array()
                    .ok_or_else(|| invalid(key))?
                    .iter()
                    .map(|p| p.as_str().map(str::to_owned).ok_or_else(|| invalid(key)))
                    .collect::<io::Result<Vec<_>>>()
            })
            .transpose()
            .map(|p| p.unwrap_or_default())
    };
    Ok(Settings {
        max_files: number("max_files")?.map(|n| n as usize),
        max_bytes: number("max_bytes")?,
        max_irreplaceable: number("max_irreplaceable")?.map(|n| n as usize),
        max_untracked: number("max_untracked")?.map(|n| n as usize),
        precious: patterns("precious")?,
        precious_exclude: patterns("precious_exclude")?,
        release_without_terminal: toml
            .get("release_without_terminal")
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| invalid("release_without_terminal"))
            })
            .transpose()?
            .unwrap_or(false),
    })
}

pub fn release_without_terminal() -> io::Result<bool> {
    Ok(match paths::user_config() {
        Some(p) => read(&p.join("config.toml"))?.release_without_terminal,
        None => false,
    })
}

#[derive(Clone)]
pub struct Policy {
    pub project: PathBuf,
    pub user_home: Option<PathBuf>,
    max_files: usize,
    max_bytes: u64,
    max_irreplaceable: usize,
    pub max_untracked: usize,
    precious: Vec<(String, bool)>,
}

impl Policy {
    pub fn load(cwd: &Path) -> (Self, Vec<io::Error>) {
        let user_home = paths::user_home().map(|p| fs::canonicalize(&p).unwrap_or(p));
        let project = ancestors(cwd, user_home.as_deref())
            .find(|p| p.join(".git").exists() || p.join(".saqta.toml").is_file())
            .unwrap_or(cwd)
            .to_owned();
        let mut policy = Self {
            project,
            user_home,
            max_files: 200,
            max_bytes: 1_000_000_000,
            max_irreplaceable: 20,
            max_untracked: 2,
            precious: [
                ".env*",
                "*.pem",
                "*.key",
                "id_*",
                "*.sqlite*",
                "*.db",
                "*.kdbx",
            ]
            .into_iter()
            .map(|s| (s.into(), true))
            .collect(),
        };
        let mut errors = Vec::new();
        for path in [
            paths::user_config().map(|p| p.join("config.toml")),
            Some(policy.project.join(".saqta.toml")),
        ]
        .into_iter()
        .flatten()
        {
            match read(&path) {
                Ok(s) => policy.apply(s),
                Err(e) => errors.push(e),
            }
        }
        (policy, errors)
    }

    fn apply(&mut self, s: Settings) {
        self.max_files = s.max_files.unwrap_or(self.max_files);
        self.max_bytes = s.max_bytes.unwrap_or(self.max_bytes);
        self.max_irreplaceable = s.max_irreplaceable.unwrap_or(self.max_irreplaceable);
        self.max_untracked = s.max_untracked.unwrap_or(self.max_untracked);
        self.precious
            .extend(s.precious.into_iter().map(|s| (s, true)));
        self.precious
            .extend(s.precious_exclude.into_iter().map(|s| (s, false)));
    }

    pub fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.project)
            .or_else(|_| {
                self.user_home
                    .as_ref()
                    .map_or(Ok(path), |p| path.strip_prefix(p))
            })
            .unwrap_or(path)
    }

    pub fn regenerable(&self, path: &Path) -> bool {
        let local = self.relative(path);
        gc::regenerable(local)
            || path.file_name().is_some_and(|p| p == "CACHEDIR.TAG")
            || path
                .ancestors()
                .skip(1)
                .take(local.components().count())
                .any(gc::cache_marker)
    }

    pub fn precious(&self, path: &Path) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        self.precious
            .iter()
            .rev()
            .find(|(pattern, _)| {
                let value = if pattern.contains('/') {
                    self.relative(path).as_os_str()
                } else {
                    name
                };
                wildcard(pattern.as_bytes(), value.as_bytes())
            })
            .is_some_and(|(_, precious)| *precious)
    }

    pub fn irreplaceable(&self, path: &Path) -> bool {
        if path.starts_with(&self.project) {
            return false;
        }
        let user = self
            .user_home
            .as_ref()
            .and_then(|p| path.strip_prefix(p).ok());
        if let Some(p) = user
            && p.iter().next().is_some_and(|part| {
                matches!(
                    part.as_bytes(),
                    b"Documents"
                        | b"Desktop"
                        | b"Downloads"
                        | b"Pictures"
                        | b"Music"
                        | b"Videos"
                        | b"Dropbox"
                        | b"OneDrive"
                        | b"Google Drive"
                        | b"GoogleDrive"
                ) || part.as_bytes().starts_with(b"OneDrive - ")
            })
        {
            return true;
        }
        path.strip_prefix("/mnt/c/Users")
            .ok()
            .and_then(|p| p.iter().nth(1))
            .is_some_and(|part| {
                matches!(
                    part.as_bytes(),
                    b"Documents" | b"Desktop" | b"Downloads" | b"Pictures"
                )
            })
    }

    pub fn config_file(&self, path: &Path) -> bool {
        !path.starts_with(&self.project)
            && self
                .user_home
                .as_ref()
                .and_then(|p| path.strip_prefix(p).ok())
                .and_then(|p| p.iter().next())
                .is_some_and(|p| p.as_bytes().starts_with(b"."))
    }
}

pub fn ancestors<'a>(path: &'a Path, home: Option<&Path>) -> impl Iterator<Item = &'a Path> {
    let boundary = home.filter(|p| path.starts_with(p)).map(Path::to_owned);
    path.ancestors()
        .take_while(move |p| boundary.as_ref().is_none_or(|home| p.starts_with(home)))
}

fn wildcard(mut pattern: &[u8], mut value: &[u8]) -> bool {
    let mut retry = None;
    while let Some((&first, tail)) = value.split_first() {
        match pattern.split_first() {
            Some((&b'*', rest)) => {
                pattern = rest;
                retry = Some((rest, value));
            }
            Some((&p, rest)) if p == first || p == b'?' => {
                pattern = rest;
                value = tail;
            }
            _ => match retry {
                Some((rest, [_, next @ ..])) => {
                    pattern = rest;
                    value = next;
                    retry = Some((rest, next));
                }
                _ => return false,
            },
        }
    }
    pattern.iter().all(|&b| b == b'*')
}

#[derive(Default)]
pub struct Breaker {
    // Keep file counts by pathname and avoid counting a shared inode's bytes twice.
    files: BTreeSet<PathBuf>,
    bytes: BTreeMap<(u64, u64), u64>,
    total: u64,
    irreplaceable: usize,
}

impl Breaker {
    pub fn observe(&mut self, policy: &Policy, path: &Path, state: &journal::State) {
        if self.files.insert(path.to_owned()) && policy.irreplaceable(path) {
            self.irreplaceable += 1;
        }
        let size = state.size.max(0) as u64;
        let counted = self.bytes.entry((state.dev, state.ino)).or_default();
        self.total = self.total.saturating_add(size.saturating_sub(*counted));
        *counted = (*counted).max(size);
    }

    pub fn fired(&self, policy: &Policy) -> bool {
        self.files.len() > policy.max_files
            || self.total > policy.max_bytes
            || self.irreplaceable > policy.max_irreplaceable
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialOrd, Ord, PartialEq, Eq)]
pub enum Rule {
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
}

impl Rule {
    pub fn message(
        self,
        id: u64,
        names: &BTreeSet<PathBuf>,
        project: &Path,
        mixed: bool,
    ) -> String {
        let count = names.len();
        let files = if count == 1 { "file" } else { "files" };
        let title = if mixed {
            format!("Restored {count} {files} that this command discarded:")
        } else {
            match self {
                Self::R1 => format!(
                    "Stopped this command and restored {count} {files} after too much data was discarded:"
                ),
                Self::R2 => format!(
                    "Restored {count} {files} with uncommitted edits that this command discarded:"
                ),
                Self::R3 => {
                    format!("Restored {count} untracked user {files} that this command discarded:")
                }
                Self::R4 => {
                    format!("Restored {count} precious {files} that this command discarded:")
                }
                Self::R5 => format!(
                    "Restored {count} repository database {files} that a non-git process discarded:"
                ),
                Self::R6 => format!(
                    "Restored {count} {files} in irreplaceable folders that this command discarded:"
                ),
            }
        };
        let mut listed = names
            .iter()
            .take(3)
            .map(|p| message_path(p.strip_prefix(project).unwrap_or(p)))
            .collect::<Vec<_>>()
            .join(", ");
        if count > 3 {
            listed.push_str(&format!(" (+{} more)", count - 3));
        }
        let advice = match self {
            Self::R1 => "\n  Review the command and narrow the paths it changes.",
            Self::R2 => {
                ". Git doesn't have these changes.\n  To set changes aside, use: git stash -u"
            }
            Self::R3 => "\n  Move the files aside or ask the user before discarding them.",
            Self::R4 => "\n  Ask the user before replacing or deleting these files.",
            Self::R5 => "\n  Use git to change the repository database.",
            Self::R6 => "\n  Ask the user before changing files in these folders.",
        };
        format!(
            "[saqta {id}] {title}\n  {listed}{advice}\n  If the user really wants them discarded, they can run: saqta release {id}\n"
        )
    }
}

fn message_path(path: &Path) -> String {
    let visible: String = path.to_string_lossy().chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        .collect();
    let mut chars = visible.chars();
    let mut shortened: String = chars.by_ref().take(77).collect();
    if chars.next().is_some() {
        shortened.push_str("...");
    }
    if !shortened.is_empty()
        && shortened
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        shortened
    } else {
        format!("'{}'", shortened.replace('\'', "'\\''"))
    }
}
