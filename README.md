# Saqta

Saqta is an undo layer for AI coding agents on Linux and WSL2. Saqta preserves files before deletion, replacement and in-place writes, records creations, and restores a command's effects in reverse order.

```sh
cargo build --release --locked
saqta doctor
saqta exec -c 'sed -i s/before/after/ file'
saqta log
saqta show 1
saqta undo 1 --dry-run
saqta undo 1
```

By default Saqta restores high-confidence damage such as discarded uncommitted
edits, deleted secrets, and unusually large recursive deletes. It retains the
agent versions in a hold. An automatic restore exits with code 77 and prints a
reason and next action on stderr. Use `saqta status` to find open holds and
`saqta release ID` from your own terminal to apply the held versions. Release
is itself undoable. See [rules and configuration](docs/rules.md).

Undo runs in your own terminal. It refuses while a command's background tree is running, after a previous undo, or when a touched path has changed since the command. `saqta undo 1 --force` restores despite newer paths and keeps displaced entries under the printed undo folder with a recovery map. `--dry-run` lists conflicts and the proposed changes. A refreshed `.git/index` or `.git/worktrees/*/index` never blocks undo: Saqta keeps the newer index and restores the matching pre-command index. Existing M2 journals need no migration. `log` shows local time and duration; `log` and `show` also accept `--json` with raw nanoseconds.

Undo prints category counts, elapsed time, up to ten example paths, every unprotected item and the location of displaced versions. `--verbose` lists every effect, including with `--dry-run`. Closing stdout, for example with `saqta undo 1 | head -1`, silences further output and lets the full restore finish.

An interrupted M4 undo finishes with `saqta undo 1 --resume`. Its recovery plan
makes each move, exchange and copy safe to replay. The recovery folder retains
both pre-command and displaced versions. A partial undo from an older release
still needs manual inspection of its recovery map.

`saqta gc --dry-run` previews retention. `saqta gc --older-than 7d --max-size 5GB`
collects regenerable content after one day, then old commands, then oldest
commands until under the size limit. It protects the newest three commands,
running or incomplete commands, and interrupted undos. `--json` gives the plan
and byte totals. Durations accept s, m, h, d and w; sizes accept decimal KB, MB
and GB or binary KiB, MiB and GiB. Store size counts logical bytes in retained
commands and versions, once per inode. Protected entries can exceed the limit.
Undo labels collected paths as `expired (collected by gc)` and restores the rest.
Log, show, status and undo can schedule GC after output, at most once per day.
`saqta status` shows retained size and the last successful GC.

The default store is `$XDG_DATA_HOME/saqta` or `$HOME/.local/share/saqta`. `SAQTA_HOME` overrides it. `SAQTA_MAX_COPY` limits individual copies in bytes, defaulting to 256 MiB. Hard-linked captures have no size limit. Failed captures are reported in the journal as not protected. Kernel and volatile filesystems are skipped. Append opens are captured too.

Two hard links modified in place may return as separate files. Capture does not promise power-loss durability or protect against hostile filesystem races. Status: pre-alpha. Don't rely on it yet.

Rename-over capture can exceed the 25 µs added-cost target, especially when the
command and supervisor share two SMT threads of one core. On the Ryzen 9 9900X
under WSL2, the release build with synchronous wake-up added 88.75 µs with
`taskset -c 0,1`, compared with 25.31 µs on unrestricted CPUs. These are medians
of three runs of 20,000 operations. Review accepted this performance limit;
capture and undo guarantees remain unchanged. See the M5 review bundle for the full measurement.

Run `cargo test --locked` and `SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture` for the normal and forced memory fallback suites. `scripts/overhead.sh` measures a release build on fresh disposable fixtures. Its defaults are 20,000 operations and three repetitions; `N` and `REPS` override them. It is not run in CI.

The design is described in [capture](docs/decisions/0004-capture.md), [journaling](docs/decisions/0005-journal.md), and [undo safety](docs/decisions/0006-undo-safety.md).

The model harness runs 64 cases per property in `cargo test --locked`.
`SAQTA_HARNESS_CASES=5000 cargo test --locked --test harness` runs the nightly
workload. Saved seeds in `tests/harness.proptest-regressions` replay automatically.
Failures keep fixtures, JSON operations, syscall results and journals under
`target/saqta-tests/harness-*`. The harness uses `SAQTA_RULES=off` for explicit undo. The nightly and manual GitHub workflow uploads them
on failure. The fixed-namespace syscall helper and `SAQTA_TEST_ABORT_AFTER` undo
hook exist only in debug builds. Metadata-only changes are outside the model.

M4 decisions cover [resuming undo](docs/decisions/0009-resumable-undo.md),
[retention](docs/decisions/0010-retention.md) and the
[incremental index](docs/decisions/0011-incremental-index.md).
