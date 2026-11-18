# M6 rules R2 report

## What works now

- Dirty rewrites stay edited. Large clean Git transitions stay quiet.
- R1 stops destructive processes during the command. R2 restores dirty tracked deletions and Git reversions.
- R1 through R6 retain agent versions for human release, which is resumable and undoable.
- Release rejects supervised descendants and accepts a human terminal under an unrelated filter.
- Codex session IDs track creations and route background notices.

## How it works

Capture keeps the first Git index observed in each repository. It reuses
that index at command end if the index file is still the same.
`Capture::complete` counts confirmed deletions and changes to precious or
outside-project paths. Matching index size and mtime exclude clean tracked
paths. Near the threshold, the supervisor hashes uncertain pre-images in
batches and checks the Git index, HEAD, and stashes before counting them.
The supervisor stops and kills the tree on a trip.

At command end, `classify::evaluate` compares retained and current blobs
with Git. R2 applies if a dirty tracked file was deleted or returned to an
index or HEAD version and its old content is missing from Git. R3 applies
only to untracked deletion. R4 through R6 protect precious, repository,
and irreplaceable home data. Ordinary edits remain in the workspace.

`hold::start` checks post-states and saves the selected effect numbers and agent
versions in `holds/ID/`. It applies the existing undo plan and checkpoints through
`restore::apply`, restricted to those effects and their structural dependencies. R1 moves
a surviving Git lock it saw the interrupted command create into recovery.
The supervisor reports only after the hold is published. A live background tree
can continue in a linked journal after a foreground hold. Background-only holds
publish a one-time notice for the same session.

`supervisor::in_ancestry` follows PPids through pinned proc directories. It
checks each ancestor's command line and verifies the executable for a
supervisor invocation. Release also checks `SAQTA_SUPERVISED` and stdin.
The protected devcontainer override relaxes only the stdin check. Release
rechecks the restored paths and uses the undo engine to apply the agent's
versions. It writes an inverse journal with a new ID.

`Capture` tracks the latest effect that might need a rule. Commands without
a candidate or a breaker trip finish the capture journal without rereading
it for rules. The M4 property harness uses `SAQTA_RULES=off` to isolate explicit
undo. R0 still applies. Every review bundle runs forced proc-memory capture
and the 256-case harness. Measurement evidence is gated by milestone name.

## Acceptance checklist

| # | Item | Result | Proof |
| --- | --- | --- | --- |
| 1 | Checkout discards uncommitted edits | Pass | `checkout_restores_edits_and_records_hold` checks the bytes, exit 77, exact stash advice, log, show, and status. |
| 2 | Stash with and without `-u`, commit, and soft reset | Pass | `stash_commit_and_soft_reset_keep_git_versions`, `index_and_head_copies_prevent_a_hold`, `clean_git_line_endings_and_dirty_crlf_content_are_distinguished` |
| 3 | Circuit breaker stops a decoy home wipe early | Pass | `circuit_breaker_stops_home_wipe_and_restores_files`, `byte_and_irreplaceable_thresholds_are_configurable` |
| 4 | Clean holds precious and user files, skips generated files | Pass | `git_clean_holds_user_files_and_leaves_generated_files_gone` |
| 5 | Deploy deletion and installer overwrite | Pass | `deploy_restores_dirty_source_and_preserves_unrelated_effects`, `install_restores_precious_and_project_overrides_apply` |
| 6 | Agent-created files have session scope | Pass | `same_session_uses_file_identity_and_missing_session_does_not_share`, Codex hook integration tests |
| 7 | Delayed undo, release, and undo of release | Pass | `release_applies_agent_version_and_can_be_undone_after_a_delayed_undo`, `release_from_a_terminal_under_unrelated_seccomp_preserves_aliases_and_checks_conflicts` |
| 8 | Release gate uses ancestry | Pass | `supervisor_ancestry_refuses_release_with_a_pty_and_without_the_marker`, `supervisor_adopts_a_double_fork_and_refuses_release_after_wrapper_exit`, `release_is_refused_without_terminal_and_inside_supervision`, and the unrelated-filter terminal test |
| 9 | Background stays live, holds finish, and notices arrive once | Pass | `config_deny_floor_and_background_notice`, `foreground_hold_precedes_return_with_a_live_background_job`, `background_hold_keeps_foreground_changes_on_the_same_path`, `pid_one_seals_hold_before_the_namespace_exits` |
| 10 | Benign builds, installs, formatters and atomic saves | Pass where tools are available | Offline npm install, Cargo build, rustfmt, editor-style saves, and simulated generated paths passed. pnpm and pytest were unavailable and explicitly skipped. |
| 11 | Bounded real path examples, rule messages, R0 and R7 | Pass | `paths_are_bounded_and_controls_are_stripped_from_stderr`, `repository_database_and_irreplaceable_home_have_distinct_messages`, `config_deny_floor_and_background_notice`, `benign_build_outputs_and_tracked_atomic_saves_are_silent` |
| 12 | Automatic restore and release are resumable, and holds survive GC | Pass | `interrupted_automatic_restore_resumes_and_gc_protects_the_hold`, `interrupted_release_resumes_with_a_single_undo_id` |
| 13 | Forced proc-memory capture and existing undo harness | Pass | Review bundle checks `proc-mem` and `harness-256`. The 38 rules tests also passed with forced proc memory. |
| 14 | Rules documentation and review | Pass | `docs/rules.md`, `README.md`, ADR 0023, and the M6 R2 review bundle |
| 15 | Release-build rules cost | Pass | Three timings per variant using defaults. Both workloads without valuable changes added less than 20 ms. |
| 16 | Dirty rustfmt, sed, and 300 Python rewrites | Pass | `dirty_rustfmt_sed_and_bulk_python_rewrites_remain_edits` |
| 17 | Clean 1,000-file switch and 500-file fast-forward merge | Pass | `clean_switch_and_fast_forward_merge_do_not_trip_the_breaker` |
| 18 | Exact R2 for checkout, hard reset, restore, and dirty rm | Pass | `checkout_hard_reset_restore_and_removal_have_the_exact_r2_message` |
| 19 | Dirty moves and clean 500-file src deletion | Pass | `moving_a_dirty_file_or_its_parent_keeps_the_content`, `deleting_500_clean_tracked_files_with_stale_stats_is_undoable` |
| 20 | Bulk default breaker, stash, and unrelated edits | Pass | `bulk_dirty_checkout_uses_default_limits_and_keeps_stash_advice`, `git_clean_of_300_untracked_user_files_trips_the_breaker`, `bulk_stash_preserves_the_git_contents_and_does_not_trip`, `breaker_restores_deleted_paths_and_keeps_an_unrelated_dirty_edit`, `breaker_checks_a_short_git_batch_when_the_tracee_goes_idle` |
| 21 | R3 leaves untracked rewrites as R7 edits | Pass | `rewriting_untracked_user_files_does_not_trigger_r3` |

## Decisions

- [ADR 0023](../docs/decisions/0023-rules-and-auto-restore.md)

## Measurements

AMD Ryzen 9 9900X, Ubuntu 24.04.1 on WSL2 kernel
`6.18.58.1-microsoft-standard-WSL2`, Rust 1.100.0. Release build, unrestricted
CPU affinity, and three fresh fixtures per variant. Wall-clock medians
in milliseconds. Setup and verification were untimed.

| Workload | Rules off | Rules on | Added |
| --- | ---: | ---: | ---: |
| `saqta exec -- true` | 7.133 | 6.781 | -0.352 |
| Delete 5,000 files in `node_modules` | 234.664 | 224.572 | -10.092 |
| `git checkout -- .`, 600 dirty files in a 20,000-file repo | 132.977 | 1,004.506 | +871.529 |

For the two quiet workloads, on and off samples overlapped.

With default thresholds, checkout tripped R1 in all three rules-on runs.
Git issued unlink calls. Saqta restored 201 dirty versions, left the other
399 intact, exited 77, and printed the stash advice. The integration test ran
`git stash -u` after the breaker hold and verified it could acquire the
index lock. With rules off, Git
replaced all 600 and exited zero. Both variants checked every file's
bytes. The earlier R1 benchmark set a higher limit and restored all 600
through R2, so its timing covers a different amount of work.

The default decoy-home and 300-file Git clean tests each stopped at 201
deletions. The timing driver and raw samples are in the bundle's
`evidence/` folder.

## Known gaps and risks

- M5 capture and conflict limits still apply. For example, an in-place write
  beyond the configured copy limit can leave a capture gap. The message points
  to `saqta show` when captured restoration and gaps occur together.
- Git classification skips confident R2 and R3 conclusions when Git queries
  fail or a content comparison needs an external filter. Clean CRLF text up to
  16 MiB can be compared with normalized hashes.
- After a Git hash batch leaves the breaker below its limit, a mixed
  workload can accumulate up to 31 additional uncertain deletions before
  the next batch. An idle poll checks a potentially decisive batch so a
  sleeping tracee cannot postpone the trip. Known losses count immediately.
- A foreground hold can rotate the journal for surviving background work. A
  later R1 trip counts earlier observed effects, but the second restore is
  limited to its background journal. Earlier foreground effects outside the
  first hold remain available to explicit undo.
- The collector retains hold records and their recovery data, including after
  release. The store can therefore remain above its configured size limit.
- The local environment had neither pnpm nor pytest. The Rust cases detect the
  missing executables and report a skip.

## Questions for review

Review the mixed-rule precedence and retention of hold recovery data after
release.

## Commit plan

1. `rules: add automatic restores and undoable holds`
2. `release: check supervisor ancestry and reduce rule overhead`
3. `rules: distinguish Git reverts and destructive deletes from edits`
4. `docs: describe M6 rules and record review results`

The three implementation commits follow the original M6, R1, and R2 review
snapshots with their tests. The branch started from `main` at `5972930`.
