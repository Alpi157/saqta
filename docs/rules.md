# Automatic protection

Saqta checks captured changes after an agent command. Nothing prints when there
is no valuable change. A change kept for manual undo prints one line with its
command ID. An automatic restore prints a reason, up to three example paths, and
a suggested action on stderr. The foreground command exits with code 77 after
restoration. On a mixed hold, the advice follows the highest-priority rule.

| Rule | When it applies | Example |
| --- | --- | --- |
| R0 | Refuse operations on the Saqta store, Saqta config directory, and agent settings protected by the deny floor. | A script tries to replace `~/.config/saqta/config.toml`. |
| R1 | Stop the tree after it destroys more than 200 non-regenerable files, 1,000,000,000 bytes, or 20 files in irreplaceable folders. Count deletions of data Git cannot recover and overwrites or truncations of precious files or files outside the project. Exclude clean tracked versions. | A recursive delete starts removing a home directory. A large checkout that discards mostly dirty files also suggests `git stash -u`. |
| R2 | Restore a dirty tracked file if it is deleted or reverted to its index or HEAD version, and Git has no copy of the prior content. | `git checkout -- .` discards unstaged work. Use `git stash -u` to set changes aside. |
| R3 | Restore deleted untracked, non-ignored user files when the command deletes more than two. Overwrites follow R7 unless another rule applies. | `git clean -fd` removes three local drafts. |
| R4 | Restore a discarded precious file that the agent did not create in the current session. A copy kept by a stash remains available through Git. | An installer replaces `.env`, or `git clean -fdx` removes a local database. |
| R5 | Restore repository database files discarded by a process other than Git. | A script unlinks `.git/description`. |
| R6 | Restore discarded files in irreplaceable folders outside the project. | A script deletes a draft under `$HOME/Documents`. |
| R7 | Keep other valuable changes and print their undo ID. | A script updates an existing untracked project file. |

Formatters, `sed -i`, and other dirty-file rewrites that produce new content
remain edits. A move within the project also preserves the file. A clean
branch switch, pull, or merge can replace many files without tripping R1.
If R1 interrupts Git, Saqta moves surviving locks it observed that command
create into recovery storage. This lets the next Git command acquire its
own lock.

The supervisor loads a repository's initial index when it first needs it.
Matching size and mtime prove a clean indexed version. During the command,
uncertain deletions are checked in batches against Git as they approach a
breaker threshold. At command end, Saqta compares relevant prior and current
blobs with the index, HEAD, and stash snapshots. It hashes raw bytes and
eligible CRLF-normalized text without invoking executable filters. An
unavailable comparison can leave manual undo as the recovery path.

Plain `git stash` preserves tracked changes. `git stash -u` also preserves
untracked changes.

Regenerable paths include `node_modules`, `target`, `dist`, `build`, `.venv`,
`venv`, `__pycache__`, `.cache`, `.pytest_cache`, and directories marked by a
valid `CACHEDIR.TAG`. They are exempt from R2 through R6 and do not count toward
R1. Irreplaceable home folders include Documents, Desktop, Downloads, Pictures,
Music, Videos, Dropbox, OneDrive, and Google Drive. On WSL they also include
Documents, Desktop, Downloads, and Pictures under `/mnt/c/Users/<user>`. Other
home configuration files can be reported by R7.

Use `saqta exec --session ID -c 'command'` to associate commands with one
session. The Codex Bash hook supplies the tool payload's session ID. Without a
session ID each command has its own scope. A background job can outlive the
wrapper. A rule that fires after the wrapper returns is reported by the next
`saqta exec` in the same session after the job has been restored.

## Configuration

User defaults live in `~/.config/saqta/config.toml`. A `.saqta.toml` at the
project root can override thresholds and add or remove precious patterns.

```toml
max_files = 200
max_bytes = 1000000000
max_irreplaceable = 20
max_untracked = 2
precious = ["*.vault", "secrets/*.json"]
precious_exclude = [".env.example"]
```

Thresholds are exceeded only when the count is strictly greater. The byte
threshold counts the retained sizes of distinct inodes. Precious patterns
support `*` and `?`. A pattern containing a slash matches the relative path.
Other patterns match the filename. Defaults are `.env*`, `*.pem`, `*.key`,
`id_*`, `*.sqlite*`, `*.db`, and `*.kdbx`. Project patterns are evaluated
after user patterns, and exclusions after additions in each file.

Use `SAQTA_RULES=off` in a caller-controlled environment to run capture and
manual undo without R1 through R7. The M4 property harness uses this mode. R0
still applies.

## Holds and release

Use `saqta status` to list open holds and `saqta log` or `saqta show ID` to
inspect them. The held versions are in `$SAQTA_HOME/holds/ID/`. If automatic
restore stops partway, resume it with `saqta undo ID --resume`. A later
`saqta undo ID` reverses the original command's remaining effects.

A person can run `saqta release ID` from their own terminal to apply the held
versions. Release refuses when a restored path changed again and retains both
versions. It prints a new command ID that can be undone. Rerun the same release
command to resume an interrupted release.

Release checks the caller's ancestors in `/proc`. A Saqta supervisor anywhere
in the chain blocks release, including after a background job detaches. Saqta
also refuses if it cannot verify the chain or if `SAQTA_SUPERVISED` is set.
An unrelated seccomp filter does not affect release.

In a devcontainer, set `release_without_terminal = true` in the protected user
config to allow non-terminal stdin. The supervisor ancestry and environment
marker checks still apply.

For a false hold, report the command, the rule message, and a redacted
`saqta show ID --json`. Include the relevant pattern or threshold and whether
the discarded bytes were available in Git. The store can contain secrets and
other private data. Share a redacted journal and a decoy reproduction.
