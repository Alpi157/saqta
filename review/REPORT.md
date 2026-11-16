# M2b undo R1: report

## What works now (5 lines max)

Undo finishes through closed stdout in both default and verbose modes.
Default output summarizes counts, elapsed time, ten examples at most and all unprotected items.
--verbose lists every effect; dry-run uses the same summary and verbose options.
Git index retention, --force, deterministic undo folders and M2 journal compatibility remain intact.
Log, show and doctor keep default SIGPIPE. No dependencies were added.

## How it works

`src/restore.rs::run` checks all post-states and retained pre-images before restoring. It separates exact `.git/index` and `.git/worktrees/*/index` paths from other conflicts. The exception does not apply to index.lock, ordinary files named index or deeper paths. Unsafe parent traversal remains a refusal even with --force.

`Undo::save_current` moves a displaced entry into the private undo folder after syncing its recovery map. It prints the required explanation when retaining a newer Git index. Forced undo uses this same path. A conflicting recreated directory is retained before its original directory is recreated. `keep_move_input` retains conflicting rename/exchange inputs and builds independent working copies using directory fds, so reversing the move cannot consume the retained version. Existing shared-inode restoration keeps independent backups.

The folder is `q/main/undo-<id>-<command start_ns>`. Its deterministic name makes the path offered by a refusal or dry-run match the subsequent forced undo. Creation is exclusive; an existing folder is never reused. Completion prints the folder containing displaced versions and recovery.log. Partial failure leaves the existing in-progress marker and recovery instructions.

`src/main.rs` explicitly ignores SIGPIPE for undo, leaves exec handling intact and restores default SIGPIPE for log, show and doctor. Internal supervisors keep their existing socket signal handling. `sys::local_time` calls localtime_r. Log reads end_ns from the existing end record, displays duration in seconds and leaves raw start_ns, end_ns and duration_ns in JSON. Human commands are shortened to the terminal width or COLUMNS, defaulting to 120 columns; narrow tables omit secondary columns. Show displays effect/retention counts and uses a dash for absent retained versions. The journal files and SQLite schema are unchanged.

`src/output.rs::Progress` writes and flushes each line without propagating output errors. After the first write or flush error it disables further output. Undo uses this helper for progress, Git index retention messages, dry-run, summaries and recovery locations. Its fatal error message also uses the helper on stderr. A closed stdout cannot panic, change undo status or stop restoration.

`restore::Summary` counts successfully reversed effects in five categories: deleted files, modified files (including size-only effects), created paths, directories and renames (including exchanges). Failed syscall intents are excluded. It retains only ten distinct example paths. `--verbose` replaces examples with every effect while keeping the final counts and elapsed time. Dry-run reports the corresponding planned counts without restoring. Every recorded unprotected item is reported before restoration, so a later filesystem failure cannot hide the remaining diagnostics. Filesystem errors still stop undo with recovery instructions; output errors do not.

An unrestricted packaging run also exposed transient EAGAIN in the unchanged renamed-cwd test during parallel capture. The documented openat2 retry case matches its `..` paths. `src/sys/files.rs::open_parent` now retries only EAGAIN, at most eight attempts, using identical root, path and restrictions. Capture and restore share this thin wrapper. Exhaustion still returns an error; no checks were relaxed.

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
|---|---|---|---|
| 1 | Git status refresh does not block undo; original tree and index restored, refreshed index retained | PASS | `git_status_after_command_does_not_block_undo` |
| 1a | Worktree indexes get the same rule; exact component boundaries only | PASS | `worktree_git_index_conflict_keeps_newer_version`, `restore::tests::git_index_exception_has_exact_component_boundaries` |
| 1b | Existing M2 journal works with the fixed binary without migration | PASS | `python3 target/m2b-legacy-check.py`; baseline binary built from main e21a2b0; details below |
| 2 | Other conflicts refuse before changes, list at most ten paths, explain --force and retention | PASS | `user_edit_after_command_refuses_then_force_keeps_both`, `dry_run_reports_conflicts_without_changes` |
| 2a | Force restores saved bytes and retains the user's newer bytes with a printed recovery location | PASS | `user_edit_after_command_refuses_then_force_keeps_both` |
| 2b | Dry-run and dry-run --force report conflicts without restoring | PASS | `dry_run_reports_conflicts_without_changes`, existing dry-run test |
| 2c | Forced directory/rename conflicts retain newer entries; parent symlinks remain forbidden | PASS | `force_keeps_recreated_directory_and_its_new_children`, `force_keeps_edited_rename_input_before_moving_it_back`, `symlink_parent_substitution_refuses_undo` |
| 3 | Piped log/show terminate without panic text, including show piped to head -20 | PASS | `piped_log_and_show_exit_quietly`; existing `sigpipe_is_default_in_command` |
| 4 | Local time, aligned columns, duration, shortened single-line commands and raw JSON nanoseconds | PASS | `log_shows_local_time_and_json_keeps_nanoseconds`, including 120- and 80-column output |
| 4a | Show has one-line counts and dashes when no version was kept | PASS | `piped_log_and_show_exit_quietly` checks a creation-only record |
| 5 | All existing tests still pass; forced proc-memory run included | PASS | `cargo test --locked`: 98 tests; `SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture`: 56 tests |
| 6 | Report, 2 to 4 commit plan, ADR under 300 words and review bundle | PASS | This report; ADR 0006 is 297 words; `scripts/review-bundle.sh m2b-undo-r1` |
| R1.1 | Closed stdout cannot interrupt undo or cause an output panic; default and verbose producer exit 0 | PASS | `undo_survives_closed_stdout`: 500 files per mode, Bash pipefail, byte-exact trees, undone marker and no in-progress marker |
| R1.2 | All output errors stop further writing without escaping, including flush errors | PASS | `output::tests::output_stops_after_write_or_flush_failure`: BrokenPipe and Other errors, both write and flush failures |
| R1.3 | Five counts, elapsed time, ten examples at most, retained location and full verbose effects in real/dry-run modes | PASS | `undo_prints_summary_by_default_and_everything_with_verbose`: 19 effects across all five categories |
| R1.4 | Every unprotected item is printed beyond the ten-example limit | PASS | `undo_reports_every_unprotected_item_in_summary`: 12 unprotected files, both dry-run and real undo |


The review-bundle script includes the forced proc-memory suite for m2b-undo and m2b-undo-r1. Formatting, clippy with warnings denied, the full test suite, the forced suite, doctor and dependency checks are recorded in the bundle. Conditional environment skips are printed rather than hidden. The initial M2b sandbox bundle passed the functional checks but cargo-deny could not lock its read-only advisory cache. An offline audit with cache access passed advisories, bans, licenses and sources. An unrestricted packaging run then exposed the transient parent-resolution failure described above. That bounded-retry fix remains in R1. The R1 bundle includes the full and forced suites after the output changes. No dependencies changed. The existing dry-run conflict test now checks the exact planned deletion count instead of the old per-effect output text; all conflict and no-change assertions remain.

## Decisions

[ADR 0006: undo safety](../docs/decisions/0006-undo-safety.md) specifies the Git index exception, force retention, stable exclusive undo folders and compatibility with M2 journals. R1 adds the SIGPIPE/output policy for undo and future release/gc commands, summaries and verbose mode. It is 297 words. [ADR 0007: bounded parent resolution retry](../docs/decisions/0007-bounded-parent-resolution-retry.md) records the incidental EAGAIN fix found by review checks. No prior capture or journal decisions were reversed.

## Measurements

Machine: x86_64 WSL2, Ubuntu 24.04.1 LTS, kernel `6.18.58.1-microsoft-standard-WSL2`, rustc/cargo 1.100.0.

In the initial M2b compatibility check, a binary built from unchanged main e21a2b0 captured a dirty Git fixture with 1,000 untracked files, staged and unstaged changes, and an ignored .env. Git status outside Saqta replaced the index. The baseline binary refused undo with exit 125 and changed no project entries. The fixed binary restored the full tree with exit 0. File bytes, modes and mtimes matched, including .git/index. The refreshed index matched its retained copy byte for byte. Meta, child, end, effects.log, outcomes.log and post.log were byte-identical before and after fixed undo.

That baseline capture took 0.210 s; fixed undo took 0.138 s. These are single debug-build sanity measurements, not performance targets. The driver and evidence remain in `target/m2b-legacy-check.py` and `target/m2b-legacy-check.json`; the disposable fixture remains under target/saqta-tests for inspection. The log/show pipe regression uses 600 effects and 999 command records so output exceeds pipe capacity.

R1 adds two 500-file restore runs through head -1. In the full debug suite, default undo finished in 77.2 ms and verbose undo in 64.3 ms, with byte-exact trees and producer exit 0 in both cases. The initial progress line is flushed before restoration; verbose output would exceed pipe capacity. These are sanity measurements, not performance limits. The summary fixture checks 15 deleted files, one modified file, one creation, one removed directory and one rename. A separate fixture verifies that all 12 unprotected paths are reported, rather than stopping at ten.

## Known gaps and risks

- The user's actual 65,899-file journal is not available here. Compatibility was checked against a real journal produced by main, with its original journal files left unchanged.
- --force authorizes restoring despite newer work. Inspect the printed folder and recovery.log to retrieve displaced versions. Missing pre-images, unsafe parent traversal, live trees and damaged journals remain refusals.
- Rename-only effects do not contain an independent snapshot of their source bytes. Forced undo reverses the move with the available source and retains an independent version. Size-only growth effects restore the recorded length using the available prefix. Force cannot reconstruct bytes that M2 never captured.
- Existing M2 limits remain: post-state checks compare metadata rather than content hashes, concurrent external writers can race undo, and hard-link topology may change. Nonempty directories and special entries cannot be copied across filesystems by the displaced-entry fallback; a failure retains the available versions and requires manual recovery.
- Power-loss durability is not promised. Power loss, filesystem errors or explicit process termination can still require reviewing the in-progress marker and recovery map. Closing stdout no longer interrupts undo; further output is discarded after its first error.
- This sandbox cannot mount tmpfs or bind the Unix socket fixture. The M1 bare i386 probe is terminated by the outer sandbox. Those existing scenarios print explicit SKIP reasons; FIFO and the remaining checks run.

## Questions for review

- Check the narrow Git path exception and the ordering of recovery-map writes, retention and replacement.
- Review retention of forced rename/exchange inputs and the exclusive deterministic undo folder.
- Check that every undo output path uses Progress, including fatal stderr, and that summary examples never suppress unprotected diagnostics.
- Validate the fixed binary against the user's preserved command 1 journal before using it for the real rescue.

## Commit plan

Review passed. The user authorized the three commits below. Each staged tree was validated before committing, with shared CLI/test files split between the first two commits. Nothing was pushed.

1. `restore: preserve both versions and finish undo after output failure`: undo and --force/--verbose CLI, Progress helper, undo SIGPIPE policy, summaries, retention/output tests, bounded parent-resolution retries and ADRs 0006/0007.
2. `cli: format command history and handle read-only broken pipes`: local-time and terminal-width wrappers, read-only SIGPIPE setup, log/show formatting and output regression tests.
3. `docs: report M2b R1 validation and package the forced memory run`: README, review-bundle update and this report.
