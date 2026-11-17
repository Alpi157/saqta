# M4 harness: report

## What works now (5 lines max)
- Three filesystem properties exercise sequential commands, concurrent subtrees and interrupted undo.
- Undo resumes from explicit recovery actions and preserves hard-link alias lineage across commands.
- GC applies retention in the requested order and labels collected content during undo.
- Log, show and one-file undo avoid importing unrelated effects on repeated calls.
- Supervised doctor uses informational listener checks and keeps its actual capture probe.

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

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
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
- The GitHub workflow is authored but has not executed remotely. No files were committed or published.

## Questions for review

Review the replay identities, growth-prefix handling, alias lineage, expiry-before-unlink order and conservative GC refusal. Check that the documented process-crash and retention-size boundaries match the intended contract.

### Commit plan after review passes

1. `capture: protect appends and preserve moved hard-link origins`
2. `undo: resume reverse actions and retain alias lineage`
3. `gc: collect expired versions under command locks`
4. `journal: import changed commands and polish CLI output`
5. `test: add model harness, regressions and nightly artifacts`

The first bundle caught the partial-log recovery bug above. Its audit also hit
a sandbox read-only advisory-cache lock. `cargo deny --offline check` passed
outside the sandbox. The final bundle is rerun with that cache lock available.

Stage related CLI and journal hunks with each change so every commit builds and
passes its checks. Include each decision with its implementation. Commit only
after the user says review passed, and request the required permission before
each commit.
