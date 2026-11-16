# Saqta: notes for coding agents

Saqta is an undo layer for AI coding agents on Linux and WSL2. Each agent shell command runs under a small supervisor that uses seccomp user notification. Before any system call that can destroy data, the supervisor keeps the old version (a hard link for deletes and rename-overs, a copy for in-place writes), then lets the call run on the real filesystem. Any agent command can be undone byte for byte.

**The bar: Saqta must never lose data itself.** When in doubt, keep both versions and say so.

## Read first
- `spike/` is a working C prototype. `spike/wsc.c` is the process model (wrapper, supervisor, filtered command) that `saqta exec` must follow; `spike/prewrite2.c` is the filter and capture logic; `spike/restore.py` is the undo order. Port the ideas, not the shortcuts (the spike's known flaws are listed in `spike/README.md`).
- `docs/decisions/` explains past decisions. Don't reverse one without writing a new decision record.

## Layout
```
src/main.rs        CLI (clap)
src/exec.rs        the wrapper (W) and process setup
src/supervisor.rs  the supervisor (S): notification loop, capture, tree kill
src/filter.rs      seccomp BPF program (x86_64)
src/sys/           every unsafe call, as thin wrappers (seccomp, ioctls, fork, signals, sockets)
src/journal.rs     effect log, SQLite index, quarantine store
src/restore.rs     undo
src/doctor.rs      environment checks
tests/             integration tests (they run the real binary)
scripts/           review-bundle.sh and measurement scripts
docs/decisions/    one short record per real design decision
```

## Commands
```
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
scripts/review-bundle.sh <milestone>      # at the end of every milestone
```
If a new dependency needs downloading and the network is blocked, stop and ask the user to run `cargo fetch`.

## Code rules
- Rust 2024 edition, stable toolchain, no async runtime.
- Dependencies: `libc`, `clap`, `anyhow`, `serde`, `serde_json`, `rusqlite` (bundled), and for tests `tempfile` and `proptest`. Anything else needs a reason in the report and the user's approval.
- All `unsafe` lives in `src/sys/`. Every `unsafe` block has a `// SAFETY:` comment saying why it is sound. No `unsafe` anywhere else.
- After `fork()`, the child does only async-signal-safe work: no allocation, no locks, no printing. Build every `CString`, argv and env before forking.
- Every file descriptor Saqta creates is close-on-exec. The filtered command must never inherit the listener or any Saqta socket.
- Paths are bytes. Use `OsStr`, `OsString`, `Path` and `OsStrExt`; never assume UTF-8; store paths as SQLite BLOBs.
- On sockets, use `read`, `write`, `sendmsg` and `recvmsg` only. Never `send` or `sendto`: Codex's sandbox blocks them.
- Reading another process's memory: try `process_vm_readv`, fall back to `pread` on `/proc/<pid>/mem` when it fails with EPERM or ENOSYS (Codex's sandbox blocks `process_vm_readv`). Don't read across a page boundary in one call. Check `SECCOMP_IOCTL_NOTIF_ID_VALID` after reading and before acting on what you read.
- The supervisor must never panic or block forever. A failed capture still gets an answer for the waiting process (continue, or an errno when the deny floor applies), plus a journal entry saying what wasn't protected.
- The supervisor never writes to stdout or stderr (it closes them). With `SAQTA_DEBUG=1` it logs to `$SAQTA_HOME/logs/`.
- Run git as `git --no-optional-locks`, so Saqta never takes `index.lock`.
- Error handling: `anyhow` with context in the CLI layer; `io::Error` with context in the core. No `unwrap` or `expect` outside tests unless a comment proves the invariant.
- Keep functions small and named for what they do. Someone must be able to explain every module in an interview.

## Safety rules for tests and for you
- Destructive commands run only inside a temp directory the test just created. Never `rm -rf` a path built from a variable that could be empty. Never touch the real home directory, `~/.codex`, `~/.claude` or any agent settings on this machine.
- Tests create temp dirs under `target/saqta-tests/` (same filesystem as the repo). Not `/tmp`: it can be tmpfs, and Saqta skips tmpfs on purpose.
- Every test that runs `saqta` sets `HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `SAQTA_HOME` to folders inside its temp dir.
- A test that needs something the machine lacks (a terminal, `python3`, a 32-bit compiler, a second filesystem, root) prints `SKIP: <reason>` and returns. It never fails or passes silently.
- Timing assertions are generous (CI machines are slow). Print measured numbers; assert only sanity bounds.
- For raw system calls in tests (x32 numbers, `openat2`, `io_uring_setup`), use small `python3 -c` programs with `ctypes`, and skip when `python3` is missing.

## How we work
- One branch per milestone (`m1-exec`, `m2-capture`, ...). Small commits that each build and pass tests, with messages like `exec: forward SIGTERM to the whole tree`. In Codex's sandbox `.git` is read-only, so ask before each commit.
- Each real design decision gets `docs/decisions/NNNN-short-title.md`: context, decision, alternatives considered, consequences. Under 300 words.
- A milestone is done when every acceptance item in its prompt passes. Then write `review/REPORT.md` (template below) and run `scripts/review-bundle.sh <milestone>`. If something can't be fixed, say so plainly in the report. Never weaken or delete a test to make it pass.

## REPORT.md template
```
# <milestone>: report
## What works now (5 lines max)
## How it works
The core mechanism in plain words, naming the key functions and files, so the user can explain it in an interview.
## Acceptance checklist
| # | Item | Result | Proven by (test name or command) |
## Decisions
Links to the new docs/decisions/ files, one line each.
## Measurements
Numbers with the machine and kernel they came from.
## Known gaps and risks
## Questions for review
```

## Writing style for docs, messages and reports
Plain, natural language and short sentences. No em dashes. Messages that reach an agent are one to five lines, quote file paths, and say what to do next.
