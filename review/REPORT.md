# M4 harness, R2: report

## What works now (5 lines max)
- Three filesystem properties exercise sequential commands, concurrent subtrees and interrupted undo.
- Undo resumes from explicit recovery actions and preserves hard-link alias lineage across commands.
- GC applies retention in the requested order and labels collected content during undo.
- Log, show and one-file undo avoid importing unrelated effects on repeated calls.
- Timeout tests validate build provenance and establish fault states before the 350 ms deadline.

## How it works

`tests/harness.rs` generates 1 to 40 direct operations over six directories and
twelve file names. The fixture starts with ordinary files, empty names, an empty
directory and a pre-existing hard-link pair. The debug-only `__harness` driver in
`src/harness.rs` reads JSON, pins parents without following symlinks, checks final
symlink targets stay inside the fixture, and calls thin syscall wrappers. Its
concurrent workers each own a subtree. Syscall failures are allowed inputs and
are printed with the process, fixture, operation and errno.

The sequential property uses one to three exact consecutive commands. It checks
every intermediate snapshot while undoing newest first. The hash covers type,
mode, file size, bytes, symlink target and file mtime. Directory allocation size
and directory mtime are excluded. The interruption property first obtains an
uninterrupted result, repeats the operations, then aborts after a mutation and
before its completion checkpoint. It checks pre-command and post-command byte
versions in that command's tree, quarantine and undo folder only. Earlier undo
folders cannot mask a missing version. Resume must match the uninterrupted tree.

`src/restore.rs` plans reverse effects and handles conflicts, original prefixes
and alias lineage. `src/restore/steps.rs` executes moves, staged copies, inode
repairs, directory recreation and exchanges. Plans and checkpoints use atomic
replacement. Move and exchange identities distinguish a completed step from one
that still needs execution. The recovery map remains synced before displacement. Numeric snapshots are
independent copies when an entry shares an inode. Separate mutable inode handles
allow older aliases to be repaired without changing those snapshots.
Original data and independent post versions remain available before inode repair.
Plain alias observations connect identities across consecutive undos. The
`SAQTA_TEST_ABORT_AFTER` hook exists only in debug builds.

`src/gc.rs` validates retained pre-images before collection. It takes a global
collector lock and the same per-command locks as undo. It first collects
regenerable content after one day, then commands past the age limit, then oldest
commands until under the size limit. The newest three ids, live commands and incomplete journals are protected. Any
interrupted undo defers collection, including collection of borrowed inode handles
in older undo folders. Every reference to a
shared pre-image must be regenerable. CACHEDIR.TAG signatures are recognized in
current directories and retained deleted tags. Expiry records precede unlink;
whole-command metadata is removed last. Undo reports expired paths and restores
other effects. Dry-run writes no locks, markers or content.

Interactive commands reserve one GC attempt per 24 hours under a lock and launch
the collector after printing with closed standard streams. New stores begin the
first interval at creation. Status reports retained size, last successful GC and
any prior collection refusal.

`journal::import` fingerprints each command and caches summaries in SQLite. It
reimports only changed journals. `journal::refresh` lets show and undo load only
the requested command. First import and explicit database rebuild still read the
source effects. No dependencies were added.

R1 combines an effect's completion checkpoint with the next effect's plan in
`restore_effects`. The final completion is still flushed. Each mutation retains
its existing identity checks and recovery map. `Journal::notification_progress`
overwrites a fixed observation record only with debug logging enabled.
`tests/common/timeout.rs` samples the owned process tree, successful kernel
replies and undo state one second apart, then prints bounded journal tails.
Cleanup stops and kills descendants, orphan supervisors and inherited pipe
holders, checking process start times before signalling. Reaping is bounded.
Fixtures enable debug logging consistently for direct and supervised commands.
Each helper command has its own process group. This prevents job-control HUP
signals from reaching parallel tests while cleanup freezes a process tree.

R2 checks the debug executable's compiled manifest path and observation protocol
once per test binary with `verify_build`. `__harness --build-info` returns static
metadata without opening a store. Review builds use `target/review-build`, and
each validation snapshot must have its own Cargo target directory. Timeout
tests use `wait_ready` before `output_after_setup`. A stdin barrier establishes
the blocked tracee; `SAQTA_TEST_STOP_UNDO=1` stops a debug undo before mutation.
Reports show total elapsed, setup and fault-window time, plus actual observation
read errors. Capture behavior is unchanged. A separate saved-seed fix relocates
directory observations through reverse moves and freezes their input map once
per undo, so resume cannot apply an exchange twice.

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
| --- | --- | --- | --- |
| 1 | All requested operation kinds, colliding namespace, direct debug helper | PASS | `consecutive_commands_restore_every_snapshot`, `syscall_helper_exercises_creation_and_write_through_symlink` |
| 2 | One to three commands; newest-only and complete undo match snapshots | PASS | `consecutive_commands_restore_every_snapshot` |
| 3 | Two to four concurrent processes on separate subtrees | PASS | `concurrent_subtrees_restore_exactly` |
| 4 | Interrupted undo retains both boundary versions and resumes exactly | PASS | `interrupted_undo_preserves_versions_and_resumes` |
| 5 | Every action boundary for shared inode repair and exchange | PASS | `resume_replays_every_step_of_shared_inode_repair_and_exchange` |
| 6 | 64 cases by default; override; saved seeds; failing fixtures | PASS | `config` in `tests/harness.rs`; regression file; fixture preservation |
| 7 | Nightly and manual 5,000-case Ubuntu 24.04 workflow; failure artifacts | AUTHORED | `.github/workflows/harness.yml`; not run on GitHub during this session |
| 8 | Minimized product failures become named regressions | PASS | Bug table below |
| 9 | Age and size limits; regenerable content first; newest three protected | PASS | `age_size_dry_run_and_newest_three` |
| 10 | Shared pre-images and tagged caches | PASS | `cachedir_tag_and_shared_preimages_keep_nonregenerable_references` |
| 11 | Running commands and interrupted undos protected | PASS | `running_and_interrupted_undo_are_protected` |
| 12 | Expired paths reported; remaining content restored | PASS | `regenerable_content_expires_first_and_undo_restores_other_content` |
| 13 | Dry-run changes nothing | PASS | Full store hash in `age_size_dry_run_and_newest_three` |
| 14 | Opportunistic GC at most once daily; status size and timestamp | PASS | `opportunistic_gc_runs_after_output_at_most_once_per_day` |
| 15 | Incremental import with 70,000-effect history; loose timing bound | PASS | `incremental_history_does_not_reparse_seventy_thousand_effects`; SQLite trigger rejects reimport |
| 16 | Non-zero undo categories and preferred source examples | PASS | `undo_and_dry_run_print_only_nonzero_categories`, `summary_examples_prefer_source_paths` |
| 17 | Supervised doctor gives info, no blocked store writes, real capture probe | PASS | `supervised_doctor_reports_info_without_store_write_attempts`; existing two Codex doctor tests |
| 18 | GC preserves recovery copies when an older hard link changed | PASS | `gc_keeps_a_newer_copy_when_an_older_retained_hard_link_was_changed` |
| 19 | A dead incomplete exec recovers a partial trailing intent; raw log retained | PASS | `killed_command_recovers_only_complete_intents_and_keeps_the_raw_tail` |
| 20 | A retained post version stays immutable after later alias writes | PASS | `preserved_created_hard_link_is_immutable_after_later_write` |
| 21 | Formatting, clippy, full suite, forced proc-memory fallback, release configuration | PASS | `scripts/review-bundle.sh m4-harness`; `cargo check --release --locked` |
| R1.1 | Three M3 and three M4 runs on CPUs 0,1 without synchronous wakeups; phase timings | PASS | Table below; raw logs and M3 instrumentation patch in the R1 bundle's `evidence/` |
| R1.2 | Diagnose timeouts before killing the full owned tree | PASS | Four fault-injection tests in `tests/timeouts.rs`, including an older undo and an orphan holding stdout |
| R1.3 | Explicit sixty-second bulk timeout; byte-exact restore unchanged | PASS | `separate_bind_mount_five_thousand_deletes_restore`; ordinary helper deadline remains ten seconds |
| R1.4 | Investigate >25% regression and fix cheap recovery overhead | PASS, residual recorded | Checkpoint count and before/after table below; `resume_replays_deletes_with_deferred_completion_checkpoints` |
| R1.5 | Repeat constrained checks on both CI operating systems; retain timeout artifacts | AUTHORED | `.github/workflows/ci.yml`; remote rerun awaits publication |
| R1.6 | Full checks and revised review bundle | PASS | `scripts/review-bundle.sh m4-harness-r1`: fmt, clippy, 156 tests, proc-memory fallback, release build, constrained checks, doctor and cargo-deny |
| R2.1 | Reproduce the user's terminal failure outside the sandbox and compare inside | PASS | `evidence/r2/r2-outside-before.log`, `r2-inside-before.log`, `stale-build.json`: the same three failures in both |
| R2.2 | Reject stale or foreign binaries before fault injection; isolate validation builds | PASS | `foreign_build_is_rejected_before_fault_injection`; `verify_build`; review script and AGENTS.md |
| R2.3 | Explicit readiness, including startup exceeding 350 ms; assertions retained | PASS | All four original timeout tests; stopped-supervisor setup includes a deliberate 500 ms delay |
| R2.4 | Loaded 24-CPU and 2-CPU runs | PASS | Three runs per configuration, all five tests pass; `loaded-checks.json` and individual logs |
| R2.5 | Whole suite inside and outside the sandbox, with approval for outside | PASS | 160 tests in each: `evidence/r2/full-inside.log` and `full-outside.log` |
| R2.6 | Revised review bundle | PASS | `scripts/review-bundle.sh m4-harness-r2`: fmt, clippy, 160 tests, proc-memory fallback, release configuration, constrained checks, doctor and cargo-deny |
| R2.7 | New directory-observation harness regression, exact resume and later-edit refusal | PASS | `regression_directory_observations_follow_exchanges_of_ancestors`, `resumed_directory_observations_are_not_exchanged_twice`, `restored_directory_observations_reject_later_user_edits`; saved seed |

### Bugs found and fixed

Persisted failure seeds remain in `tests/harness.proptest-regressions`. Names
below are in `tests/harness.rs` unless another file is named. Several minimized
sequences share one underlying bug; all remain as separate regressions.

| Failure | Fix | Named regression |
| --- | --- | --- |
| A pure append survived undo | Notify on append-capable write opens | `regression_pure_append_is_undone` |
| A failed directory rename caused a conflict after undoing a later child write | Ignore failed intents; compare restored file metadata and recorded directory observations | `regression_failed_directory_rename_then_child_write` |
| Exchange could not reverse after a created input was removed | Move the surviving input back when the other side is absent | `regression_exchange_then_remove_a_created_input` |
| Growth between repeated writes restored an impossible intermediate size | Use the existing full pre-image once one exists | `regression_write_then_grow_then_write_does_not_restore_an_intermediate_size` |
| Original bytes existed only as a prefix during interrupted growth undo | Materialize complete original prefix files before mutation | `regression_grown_prefix_survives_interrupted_undo` |
| A grown file disappeared from its recorded pathname after a directory exchange | Follow later successful moves to find its prefix | `regression_grown_file_moves_with_exchanged_directory` |
| Undo of a newer command changed an inode and blocked an older growth undo | Use restored metadata and alias observations | `regression_growth_then_a_later_command_replaces_the_inode` |
| Snapshot followed a removed directory replaced by a symlink | Snapshot without traversing symlink parents | `regression_removed_directory_replaced_by_a_symlink` |
| A moved parent replaced by a symlink loop damaged post-state recording | Record inaccessible children as absent and verify the recorded ancestor before accepting that absence | `regression_moved_parent_replaced_by_a_symlink_loop` |
| Exchanging a symlink over a touched parent made safe undo look inaccessible | Apply the same recorded-ancestor check | `regression_exchange_a_symlink_over_a_touched_parent` |
| An exchanged hard-link source acquired a trailing slash, so capture treated an original alias as created | Preserve the exact basename when a moved suffix is empty | `regression_write_new_hard_link_after_original_name_is_exchanged` |
| Growth through a new alias restored the wrong name after the original name was replaced | Record growth against its surviving original name | `regression_growth_through_new_hard_link_then_replace_original_name` |
| Growth, deletion and a write through a pre-existing alias left the other name wrong | Preserve inode identity and repair recorded aliases | `regression_grow_delete_and_write_a_preexisting_alias` |
| Repeated writes around an alias deletion left a frozen restored alias at an intermediate version | Track original and actual identities during recovery | `regression_repeated_write_after_alias_deletion_repairs_both_names` |
| The same frozen-alias failure occurred after a rename | Relocate recovery lineage with names | `regression_repair_alias_after_rename_and_later_write` |
| Equal-length truncate after an earlier copy omitted an alias and its changed mtime | Record a modify effect using the existing full pre-image | `regression_rename_over_alias_then_equal_length_truncate` |
| Undoing a newer command detached an alias needed by the older command | Retain validated lineage and inode handles across undo folders | `regression_alias_lineage_survives_undo_of_a_newer_command` |
| GC could remove the last good copy when a later command changed an older retained hard link | Refuse collection before mutation when retained pre-images changed; preserve all recovery copies and report why | `tests/retention.rs`: `gc_keeps_a_newer_copy_when_an_older_retained_hard_link_was_changed` |
| R2 recheck: nested directory exchanges left undo observations at old paths and blocked an older undo | Relocate observations and record final names; freeze the input map for idempotent publication | `regression_directory_observations_follow_exchanges_of_ancestors`, `resumed_directory_observations_are_not_exchanged_twice`, `restored_directory_observations_reject_later_user_edits` |

The forced memory-fallback check also found a supervisor killed during a log
write. Only a dead incomplete exec may recover its complete prefix. The raw
log stays byte for byte. Completed or live journals still refuse that damage.
`tests/capture.rs`: `killed_command_recovers_only_complete_intents_and_keeps_the_raw_tail`
is the named regression. A related retained-version check now separates immutable
numeric snapshots from mutable inode handles, proved by
`preserved_created_hard_link_is_immutable_after_later_write`.

The harness itself also needed corrections. openat2 is intentionally blocked in
the filtered command, so the driver now walks pinned parents with openat and
O_NOFOLLOW. Truncate uses a checked fixture pathname instead of a proc magic
link. A smoke test proves actual creation and symlink writes occur. Fixture ids
include a short timestamp because sandbox PID namespaces can reuse process ids;
the short form also fits Unix socket path limits. The interruption oracle was
restricted to the current command's recovery locations.

## Decisions

- [0009: resumable undo](../docs/decisions/0009-resumable-undo.md): replayable actions, alias lineage and process-crash guarantees.
- [0010: retention](../docs/decisions/0010-retention.md): collection order, protection, expiry and daily scheduling.
- [0011: incremental index](../docs/decisions/0011-incremental-index.md): changed-journal imports and append interception.

- [0012: unacknowledged tail](../docs/decisions/0012-unacknowledged-tail.md): preserve a killed writer's raw log and qualify prefix recovery.
- [0013: bulk undo checkpoints and diagnostics](../docs/decisions/0013-bulk-undo-checkpoints-and-diagnostics.md): combine redundant checkpoints, retain short ordinary deadlines and measure actual replies on timeout.
- [0014: artifact provenance and fault readiness](../docs/decisions/0014-artifact-provenance-and-fault-readiness.md): isolate builds, verify the executable, and synchronize each injected fault.
- [0015: directory observation relocation](../docs/decisions/0015-relocate-directory-observations.md): follow reverse moves and reuse a frozen input map on resume.

## Measurements

Machine: AMD Ryzen 9 9900X, 24 logical CPUs, ext4 on WSL2.
Kernel: `6.18.58.1-microsoft-standard-WSL2`, x86_64.
Toolchain: rustc 1.100.0 and cargo 1.100.0. Measurements use debug builds.

- Recorded green full-suite run: 151 tests, 27.76 seconds across test binaries. The final bundle repeats the final source checks.
- Default harness with pre-existing hard links and immutable post snapshots: 24 tests, 15.96 seconds in that green run.
- Extended run with pre-existing hard links: 128 cases per property, all 22 tests passed in 27.52 seconds.
- Earlier extended run: 256 cases per property passed before the pre-existing alias fixture was added.
- 70,000-effect first import: 695 ms. Cached log: 4.55 ms; show: 2.42 ms; one-file undo: 8.69 ms. The test asserts only a generous two-second sanity bound and separately rejects any unchanged-effect reimport with a SQLite trigger.
- Scheduling background GC from log: 42.4 ms including normal log work.
- Existing 5,000-file cross-mount deletion and undo: 3.56 seconds.
- Existing 500-file undo after stdout closes: 359 ms normally and 317 ms with verbose output.

### R1: Ubuntu 22.04 timeout investigation

The [failed CI job](https://github.com/Alpi157/saqta/actions/runs/44053076770/job/133935447902)
reports only `command 5197 timed out`. It does not record the command arguments,
phase or process state. Local constrained runs reproduce a bulk undo slowdown;
none hangs. This supports slowness as the explanation, but cannot retrospectively
prove what that CI process was doing. The new diagnostics remove that ambiguity
on the next timeout.

M3 is `main` at `73eb644`, in the detached worktree `target/m3-r1`. M4 is
`663fb2c` before R1 edits. Both use debug builds on the machine and kernel above,
with `taskset -c 0,1` and `SAQTA_NO_SYNC_WAKE=1`. M3 received test instrumentation
only, preserved as `evidence/m3-timing.patch`. The first six runs alternated M3
and M4, with no overlapping benchmark processes. R1 then ran three times.
The fixtures originally cleared `SAQTA_NO_SYNC_WAKE`; this test now deliberately
passes it through. Every log confirms `no_sync=true` and CPU affinity `0-1`.
R1 also enables the new debug reply observations.

All values are seconds. Fixture includes the namespace probe, creating 5,000
files and hashing them. Inspect is `log` plus `show`. Verify hashes the restored
tree, including bytes, mode, size and file mtime. Total sums all five phases.

| Version | Run | Fixture | Delete under Saqta | Inspect | Undo | Verify | Total |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| M3 | 1 | 0.442 | 1.708 | 0.411 | 0.364 | 0.051 | 2.977 |
| M3 | 2 | 0.452 | 1.844 | 0.382 | 0.366 | 0.056 | 3.100 |
| M3 | 3 | 0.418 | 1.767 | 0.360 | 0.355 | 0.053 | 2.953 |
| M4 before R1 | 1 | 0.267 | 1.374 | 0.333 | 3.992 | 0.054 | 6.020 |
| M4 before R1 | 2 | 0.269 | 1.431 | 0.320 | 3.011 | 0.051 | 5.082 |
| M4 before R1 | 3 | 0.271 | 1.377 | 0.269 | 1.913 | 0.048 | 3.877 |
| M4 with R1 | 1 | 0.278 | 1.386 | 0.293 | 2.145 | 0.052 | 4.153 |
| M4 with R1 | 2 | 0.461 | 1.817 | 0.308 | 2.097 | 0.051 | 4.734 |
| M4 with R1 | 3 | 0.278 | 1.426 | 0.323 | 1.823 | 0.052 | 3.903 |

M4's median delete was 1.377 s versus M3's 1.767 s. Its median undo grew from
0.364 s to 3.011 s. R1 reduces that to 2.097 s, a 30% reduction. Storage and cache
variation are visible in the individual runs; three samples are not a precise
throughput estimate.

The bind-mounted store forces `EXDEV` during capture, so deletion keeps copies.
After the mount namespace exits, undo can rename those copies on the same
filesystem. M4 added replayable plans and inode checks that repeat secure parent
walks for each restored entry, plus directory and alias observations. M3 already
used secure parent walks, but kept the quarantine directory open and moved the
saved files without recovery checkpoints. Incremental import makes inspection
faster here; the measured regression is in undo.

For 5,000 simple deletions, M4 replaced `state.json` 10,001 times: an initial
checkpoint, then a plan and completion per file. R1 needs 5,002 replacements:
initial state, each plan incorporating the preceding completion, and one final
completion. No mutation runs before its plan is saved. Killing undo before the
next save replays the previous action using its persisted identity. The named
three-delete regression aborts at every restored file and checks exact resume;
the existing interruption properties and exchange regressions remain unchanged.

Median delete + inspect + undo remains about 54% slower than M3 after R1.
Per-effect atomic plan replacement and repeated secure pathname checks remain
the main structural costs. Further plan batching or directory descriptor reuse
needs more recovery and rename-race design; record this for the performance
milestone. The cheap redundant completion writes are removed now.

This reproduces two-CPU affinity and the no-sync listener path. It does not
emulate Azure storage latency, CPU speed or Ubuntu 22.04's older kernel. All
nine runs restored exactly. No product hang or data-loss bug was found in R1.

Reproduction after applying the included measurement-only patch to M3:

```sh
taskset -c 0,1 env SAQTA_NO_SYNC_WAKE=1 cargo test --locked --manifest-path target/m3-r1/Cargo.toml --target-dir target/r1-m3-build --test codex separate_bind_mount_five_thousand_deletes_restore -- --exact --nocapture --test-threads=1
taskset -c 0,1 env SAQTA_NO_SYNC_WAKE=1 cargo test --locked --test codex separate_bind_mount_five_thousand_deletes_restore -- --exact --nocapture --test-threads=1
```

The four diagnostic tests inject an answering supervisor, a stopped supervisor,
an orphan holding stdout, and a stopped resume of an older command. In the
constrained run, the active supervisor produced replies during the sample. The
stopped supervisor showed state `T`, a tracee waiting in seccomp and zero reply
progress. The older undo report found its real recovery checkpoint and map.
Every injected timeout killed its remaining owned processes. CI uploads these
private diagnostic files on failure. A zero reply delta alone is reported as
observed inactivity, since one long filesystem operation can also cause it.
Parallel fault injection initially exposed a cleanup-helper bug: stopped commands
in the test runner's shared group could trigger an orphan-group SIGHUP. Giving
each helper command its own group fixes it; both parallel and constrained serial
diagnostic runs now pass. No product signal behavior was changed.

R1 verification outside the sandbox: all 156 tests passed in 30.0 seconds across
the test binaries. Forced proc-memory fallback passed all 57 capture tests. The
two-CPU bulk and diagnostic checks, formatting, clippy, release configuration,
doctor and offline cargo-deny all passed. The first bundle's only check failure
was the advisory-cache lock on a read-only path; the outside-sandbox repeat passed.

### R2: missing observations in the normal terminal

The unmodified tests reproduced the same three failures both outside and inside
the sandbox: one passed, three failed, in 1.50 s and 1.47 s respectively. The
user's original `target/timeouts-local.log` is preserved in the bundle. The
earlier sandbox and CI passes used a fresh executable.

The failing executable was from the standalone R1 commit-validation snapshot.
That validation used `--manifest-path target/r1-commit-check-bj57wulg/Cargo.toml
--target-dir target`, which overwrote the branch's executable and build cache.
This was an agent validation mistake. Cargo reported a fresh test build in
0.02 s and kept running the older executable. Its SHA-256 was
`c32b1f413130a29d20c781a7be72b1ade50b90e555ea97cf9dc62157c2de28ec`.
The binary contained the snapshot's manifest path and lacked
`notification_progress`. There was no observation file in any failing command,
although effects, debug logs, process samples and undo checkpoints were present.
The helper was reading the correct journal path from the wrong executable.

`cargo clean -p saqta` removed the contaminated crate artifacts. Rebuilding the
branch restored observations. This did not require any change to capture or to
the observation writer. The permanent guard now rejects an older or foreign
executable before starting workloads and gives the clean/rebuild command.
`foreign_build_is_rejected_before_fault_injection` runs the real provenance
endpoint against an expected foreign checkout and verifies rejection. New
review builds and future snapshot checks use separate target directories.

The environment comparison below comes from the recorded executions. Fixture
variables have the same controlled values. Namespace differences are real but
did not account for the failure: both environments failed with the stale binary
and passed after rebuilding it.

| Detail | Sandboxed | Approved outside-sandbox run |
| --- | --- | --- |
| Test process PID in the context sample | 250 | 1752426 |
| PID namespace | `4026532473` | `4026532215` |
| Mount namespace | `4026532471` | `4026532216` |
| User namespace | `4026532533` | `4026531837` |
| NoNewPrivs / Seccomp / filter count | 1 / 2 / 1 | 0 / 0 / 0 |
| CPU affinity | 0-23 | 0-23 |
| Fixture SAQTA_DEBUG / SAQTA_NO_SYNC_WAKE | 1 / 1 for supervised cases | 1 / 1 for supervised cases |
| HOME, XDG and SAQTA_HOME | Unique fixture folders under `target/saqta-tests` | Same layout, distinct fixture ids |
| `/proc` state, wchan, cmdline and journal reads | Readable, PIDs match the supervisor | Readable, PIDs match the supervisor |
| Observation file before rebuilding | Absent | Absent |
| Observation file after rebuilding | Present, forty bytes, replies advance | Present, forty bytes, replies advance |

Readiness no longer depends on completing setup in 350 ms. Each readiness
condition has the ordinary ten-second bound and diagnoses failure before
cleanup. The actual injected timeout remains 350 ms. The answering supervisor
must first publish two successful replies. The stopped supervisor is confirmed
in state T before stdin releases its tracee, which must then reach seccomp wait.
The undo stops itself before changing data; the orphan case waits for wrapper
exit. Reports separate setup from the fault window while retaining total elapsed.
All original reply, process-state, recovery-map, cleanup and exact-restore
assertions remain. No missing-record condition counts as success.

Loaded checks used three independent runs per configuration with the default
parallel test runner. CPU workers were Python processes doing an infinite busy
loop, pinned to the same CPUs as Cargo and the tests, then terminated and reaped.
The bundle includes `evidence/r2/timeout-load.py`; run it from the repository root
to repeat the load check. The stopped-supervisor setup deliberately includes a
500 ms startup delay, followed by explicit handshakes. Recorded setup reached
520.6 to 534.3 ms under load, beyond the old spawn-relative deadline.

| R2 check | Runs / tests per run | Results | Wall time per Cargo invocation |
| --- | --- | --- | --- |
| CPUs 0-23 with 24 CPU workers | 3 / 5 | All pass | 2.933, 2.174, 2.172 s |
| CPUs 0-1 with 4 CPU workers | 3 / 5 | All pass | 3.673, 3.573, 3.623 s |
| Whole suite, sandboxed | 1 / 160 | All pass | 32.10 s across test binaries |
| Whole suite, approved outside run | 1 / 160 | All pass | 32.21 s across test binaries |

Both full runs used `cargo test --locked`. The sandbox explicitly skipped the
Unix socket fixture and bare i386 comparison; those checks ran outside. Both
runs explicitly skipped the privileged tmpfs mount. Source, release configuration,
forced proc-memory fallback, constrained tests and audit are also checked by the
R2 review bundle. No notification-writer or data-loss bug was found. The final
recheck did find and fix the independent product refusal below.

### R2: additional saved-seed regression found during final verification

The final bundle rerun found seed
`26546bdf3fd3d1ff372af8e24943c20281dcfb1ffc02479370ed72ed4abab538`.
The retained seed has twelve operations; the named regression reduces it to four:
exchange `d1` with `d0/f0`, exchange `d4` with `d0`, exchange `d5` with `d4/f0`,
then unlink `d5/f0`. Splitting these across three commands and undoing newest
first restored each newer tree but falsely refused the oldest undo.

Directory mtime changes caused by restoring the deleted child were recorded at
an intermediate path. Moving its ancestor left that observation behind.
`reverse_names` now supplies the same path mapping for directory observations
and inode origins. Unchanged observation values move with their names. Newly
restored directories are observed at their final names. Conflict comparisons
remain strict; a regression creates later user work and proves refusal preserves
it byte for byte.

Undo saves the original observation map once in its private folder. Replaying
finalization derives from that frozen map, including after publishing the home
map. `resumed_directory_observations_are_not_exchanged_twice` aborts at exactly
that boundary using a debug hook, resumes, then successfully undoes the older
command. Both the two-command and three-command exact-tree cases remain as a
named regression. The initial failing log is in `evidence/r2/`.

## Known gaps and risks

- These finite properties are evidence within the generated model, not a mathematical proof over every syscall and filesystem race. Metadata-only operations, shared-subtree races between processes, skipped filesystems and failed captures remain outside that model.
- First index import or rebuilding journal.db still scales with effects. The steady-state target is met; a cold 70,000-effect import takes about 0.7 seconds here.
- Recovery checkpoints guarantee process-crash replay. They do not add power-loss durability. The existing synced displacement map remains. Older partial undos without an M4 plan require manual recovery.
- Hard links may return as separate files. Recorded lineage restores bytes across tested command boundaries. External edits during recovery can require manual inspection rather than automatic completion.
- Cross-filesystem moves of nonempty directories can refuse. Entries and recovery copies remain available.
- Retention size counts logical bytes in `cmd/` and `q/`, once per inode. It excludes the rebuildable index and diagnostic logs. Protected commands, incomplete journals and orphaned recovery content may keep retained size over the limit.
- Any in-progress undo defers GC to protect shared inode handles in older undo folders.
- GC refuses when an older retained hard link has changed. This preserves a good copy held by another command. Undo newer commands first or inspect the recovery copies. Background refusals appear in status.
- Ordinary retention tests run inside physical `target/` fixtures. The mixed source/cache test temporarily gives only its stored effect paths a synthetic prefix to test classification, then restores the real paths before undo. No syscall executes against that prefix.
- Sandboxed runs skipped Unix sockets, tmpfs mounting and the bare i386 comparison with explicit SKIP messages. The green bundle outside the sandbox ran the socket and i386 checks. Tmpfs mounting still requires privileges and was skipped. See the bundle for exact skips.
- The original five M4 commits and both R1 commits remain unchanged. R2 changes are uncommitted and have not run remotely.
- Bulk undo still exceeds M3 by more than 25% under the constrained setup. The remaining per-effect recovery cost is recorded above for the performance milestone.

## Questions for review

Review the executable provenance check, readiness handshakes and separate build
directories. The 350 ms fault deadline and every original assertion remain.
The existing recovery, retention and bulk-performance review remains relevant.

### Follow-up commit plan after R2 review passes

Keep the five M4 commits and both R1 commits unchanged. Add one commit:

1. `test: fix timeout provenance and directory observation regressions`

The first bundle caught the partial-log recovery bug above. Its audit also hit
a sandbox read-only advisory-cache lock. `cargo deny --offline check` passed
outside the sandbox. The final bundle is rerun with that cache lock available.

Include decisions 0014 and 0015 and the updated report. Commit only after the user says R2
review passed. Do not push.
