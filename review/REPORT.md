# M3 Codex: report

## What works now (5 lines max)
Project-local Codex hooks wrap Bash and capture apply_patch pre-images.
Init, uninstall and status manage project settings and installation ownership.
PID 1 writes complete journals before returning foreground or detached status.
Separate bind mounts use bounded copies; doctor exercises real capture.
Changes are on m3-codex and remain uncommitted for review.

## How it works
`src/hook.rs` reads one JSON event. Bash commands become one shell argument to
an absolute Saqta executable. `quote` escapes single quotes without interpreting
the command. The login shell comes from `sys::login_shell` and getpwuid_r.
Errors go to hook.log. The hook returns zero and leaves the original call alone.

The patch hook parses the four pathname headers, resolves relative names from
payload cwd, and calls `Capture::patch_path` in `src/capture.rs`. It uses M2's
bounded copy and effect writer, without a supervisor or SQLite in the hook.
Updates use modify effects; removals and overwritten destinations use delete
effects; absent paths use create effects. A hook-reference sidecar holds session
and tool-use IDs. PostToolUse finds that sidecar, reopens the journal with
`Journal::reopen`, and writes post-state and end. Missing posts stay incomplete.
Deny-floor paths get a not-protected effect and a hook log reason.

`src/supervisor.rs` detects W as namespace PID 1 through S's parent PID. It calls
`Capture::finish` before sending even detached status. The existing
`sys::reraise` fallback handles PID 1 ignoring a default-action signal. A real
namespace test checks TERM becomes exit 143. `keep_link` labels EXDEV copy
fallbacks explicitly and uses the existing copy-size limit.

`src/codex.rs` manages project hooks, writable roots, excludes and the project
list. toml_edit is the only new runtime dependency, explicitly approved in the
milestone. proptest is a permitted test dependency, with only its std feature.
No deny.toml exception was needed. Installation receipts store original and
installed bytes. A flock serializes installations. Registry writes use atomic
rename. Uninstall validates every proposed change before applying any, restores
original bytes for unchanged files, and removes proved owned spans or hook
entries from subsequently edited files. It refuses ambiguous changes.

Doctor creates and deletes a small sample in `.saqta-doctor/`. It checks the
pre-image bytes when it launches a new supervisor. When already supervised, the
sample is new to that command, so it checks the actual creation and deletion
capture records instead. It reads plain journals without modifying the store.

## Acceptance checklist
| # | Item | Result | Proven by (test name or command) |
|---|---|---|---|
| 1 | Bash rewrite, exact output envelope, fail-open, off switch, repeated wrapping | PASS | hook_rewrite_and_fail_open; raw_garbage_hook_is_silent_and_successful |
| 2 | Patch add, update, delete, move, session matching and undo | PASS | patch_add_update_delete_move_and_undo |
| 3 | Patch deny floor and missing post become incomplete | PASS | patch_deny_floor_and_missing_post |
| 4 | PID 1 foreground, background and signal journals complete | PASS, real namespaces available | pid_one_namespace_finishes_journal |
| 5 | Cross-mount copies and 5,000-file byte-for-byte restore | PASS, real bind mount available | separate_bind_mount_five_thousand_deletes_restore |
| 6 | EROFS fails open with the specified single line | PASS | readonly_home_has_one_line_and_runs_command |
| 7 | Init merges hooks and commented config, backups, idempotence, exact uninstall | PASS | init_uninstall_exact_roundtrip_and_idempotence |
| 8 | Default git root, tracked warnings, excludes, broken output | PASS | default_project_tracked_settings_and_closed_output |
| 9 | Refuse outside git and in Saqta itself | PASS | init_refuses_non_repo_and_saqta |
| 10 | Preserve later settings and hooks at uninstall | PASS for proved ownership; ambiguous edits refuse | uninstall_preserves_later_comments; uninstall_keeps_new_foreign_hook_bytes |
| 11 | Status checks current hooks, writable root and recent commands | PASS | init_uninstall_exact_roundtrip_and_idempotence |
| 12 | Doctor checks actual capture when already supervised | PASS | doctor_inside_wrapped_command_checks_actual_capture; existing doctor tests |
| 13 | Quoting with arbitrary NUL-free bytes, Unicode, heredocs and shell syntax | PASS, 128 generated cases | hook::tests::quoting_matches_bash; real_hook_shell_cases |
| 14 | All existing tests and forced proc-memory fallback | PASS | cargo test --locked; SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture |
| 15 | Formatting, clippy, dependency policy, docs and bundle | See bundle checks | scripts/review-bundle.sh m3-codex |

## Decisions
[0008 Codex integration](../docs/decisions/0008-codex-integration.md): local hooks,
namespace finish ordering and settings ownership. Prior decisions remain in place.

## Measurements
WSL2 x86_64, kernel 6.18.58.1-microsoft-standard-WSL2, Rust and Cargo 1.100.0.
Bash hook wall time: 1.76 to 3.70 ms across local test runs.
5,000 cross-mount deletes plus undo: 1.60 to 2.27 seconds across local runs.
Existing 200-sample exec check: direct median 3.40 ms, p95 3.85 ms;
Saqta median 6.67 ms, p95 8.79 ms. These are development-build measurements.
Final check logs and their measurements are in the review bundle.

## Known gaps and risks
- Real Codex trust-screen interaction was not automated. The integration uses
  the payload and trust behavior measured by the user.
- An ambiguous later settings edit can require manual review before uninstall.
  Saqta leaves the settings and installation receipt intact in this case.
- JSON hook formatting can change at init. Unchanged settings round-trip exactly.
  Symlinked project Codex settings are refused to avoid changing another location.
- Patch capture only records named entries. It does not remove unnamed parent
  directories made by a patch applier. Nonregular files that cannot be copied
  and files above SAQTA_MAX_COPY are logged as not protected.
- M2's races with unrelated writers and lack of power-loss durability remain.
- Codex stopping or quitting can leave commands alive. Inside PID namespaces,
  background jobs die with the namespace. The hooks cannot change either fact.
- Existing environment skips remain explicit: Unix socket creation in this
  sandbox, the old direct tmpfs mount fixture, and bare i386 execution. The new
  user-namespace PID and bind-mount tests ran successfully rather than skipping.

## Questions for review
Review hook payload compatibility against the measured Codex 0.169 behavior.
Review the installation ownership receipt and conservative refusal behavior.
No commits will be made until the user says review passed.

## Commit plan
1. `exec: finish journals before PID 1 returns and label cross-mount copies`
2. `codex: capture Bash and patch hooks with M2 journals`
3. `codex: manage project hooks, settings and protection status`
4. `doctor: verify real capture inside the Codex sandbox`
5. `codex: add integration tests, documentation and review checks`
Each commit will build and pass its relevant tests. This is a proposed plan only.
