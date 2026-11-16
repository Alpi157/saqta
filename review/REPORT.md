# M3 Codex R1: report

## What works now (5 lines max)
Project-local Codex hooks wrap Bash and capture apply_patch pre-images.
Init, uninstall and status manage project settings and installation ownership.
PID 1 writes complete journals before returning foreground or detached status.
Separate bind mounts use bounded copies; doctor exercises real capture.
The original five commits remain intact. R1 fixes are uncommitted on m3-codex.

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

## CI failure analysis and reproductions
Evidence comes from `target/ci-m3-failed.log`, supplied from PR #4.
The log has no runner kernel or util-linux version dump. Do not infer a specific
kernel release from the Ubuntu job label.

### 1. Doctor on both Ubuntu jobs: product detection bug
At log lines 139-141 (22.04) and 302-304 (24.04), the failing assertion is the
predicate requiring a check named `inside Codex sandbox` with advice
`capture works`. Actual predicate: false. Expected: true. This is an `assert!`,
so the log prints no numeric left/right pair or full JSON. The preceding
successful-exit assertion passed. Capture failure would instead set a nonzero
exit code, so this points to an omitted check, not failed capture.

`inside_sandbox` only considered the SAQTA_HOME mount and `/proc/1/comm`.
On hosted VMs, PID 1 is systemd and the test does not separately mount the store.
The existing supervisor was ignored. The local PID 1 is named `codex`, hiding
that bug. Doctor now also recognizes the same marker-plus-active-filter
condition used by nested exec, through `exec::is_supervised`.

Reproduction: run the original product under a private namespace whose PID 1
is named systemd. `doctor_inside_systemd_named_namespace_checks_actual_capture`
failed before the fix with exit 101 from the test harness, while doctor itself
returned zero and its JSON omitted the capture check. It passes after the fix.
`supervised_doctor_probes_on_a_systemd_host` exercises the detection with init,
systemd and unreadable PID-1 metadata on every machine, without namespaces.
The original wrapped-doctor assertion remains and now includes JSON on failure.

### 2. PID 1 on Ubuntu 22.04: inherited signals in the test launcher
At lines 146-149 the status assertion has **left `Some(0)`**, **right
`Some(143)`**. It reached the third case, the shell's `kill -TERM $$`, after
namespace setup and the first two cases succeeded. It failed on exit status,
before the journal assertion. The log alone does not identify a kernel race.

The older util-linux launcher sets SIGINT and SIGTERM to SIG_IGN before fork
and leaves them ignored in its execed child. See [util-linux v2.37.2
unshare.c, lines 532-546](https://github.com/util-linux/util-linux/blob/v2.37.2/sys-utils/unshare.c#L532-L546).
The newer launcher blocks signals across fork and restores the old mask in
the child instead. See [v2.39.3, lines 896-922](https://github.com/util-linux/util-linux/blob/v2.39.3/sys-utils/unshare.c#L896-L922).
An ignored disposition survives exec. Saqta preserved it, so Bash ignored its
self-directed TERM and exited zero. Changing Saqta to reset TERM would change
commands that intentionally inherit an ignored signal.

Local reproduction on the same 6.18 kernel: put SIG_IGN for INT and TERM in a
Python launcher before modern unshare. The original failing command returned
**0** and its raw journal end status was **0**, with both
SAQTA_NO_SYNC_WAKE=0 and SAQTA_NO_SYNC_WAKE=1. Thus the observed mismatch can be
reproduced without changing kernel, POLLHUP timing or finish ordering. No
supervisor or wrapper status fix is justified by this failure.

The test now uses a small Python launcher inside the namespace to establish
SIG_DFL for INT and TERM, then execs W, which remains PID 1. The expected exit
143 is unchanged. It checks foreground, background and TERM cases with ordinary
and simulated older unshare signal inheritance, with synchronous wake-up both
enabled and explicitly disabled in the child's environment. All 12 runs
reported the expected status and a finished journal. Assertions also check each
journal exit status. `inherited_ignored_term_is_preserved_without_namespaces`
checks both intentional ignore and explicit restoration on every machine,
including machines that cannot create namespaces.

### 3. Read-only home on Ubuntu 24.04: capability detection bug in the test
At lines 309-312, the status assertion has **left `Some(1)`**, **right
`Some(7)`**. Neighbouring namespace probes at lines 283 and 291 print
`unshare: write failed /proc/self/uid_map: Operation not permitted` and SKIP.
The read-only test only recognized stderr containing `unshare failed`, or a
mount command's reserved exit 77. UID-map denial occurs earlier and matches
neither condition. Saqta and its exit-7 command were never reached. The log
omits this test's own stderr, so that diagnosis also uses the neighbouring
capability evidence and the supplied AppArmor runner configuration.

The test now runs a separate `unshare --user --map-root-user --mount /bin/true`
probe first. Probe failure prints its status and exact reason as SKIP; once
setup succeeds, command failures remain failures. The exact read-only warning
and exit-7 assertions are unchanged. All namespace fixtures share that explicit
probe, including missing-unshare handling. The existing mount-77 handling still
only covers mount setup, before Saqta runs.

`namespace_probe_reports_uid_map_denial` simulates the runner's exact UID-map
stderr and exit 1 on every machine. It proves that the setup failure is reported
without relying on that particular wording. The real read-only bind test ran
successfully locally. AppArmor itself was not reconfigured or disabled.

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
| 15 | Formatting, clippy, dependency policy, docs and R1 bundle | See bundle checks | scripts/review-bundle.sh m3-codex-r1 |
| R1.1 | Wrapped doctor probes on systemd hosts | PASS | supervised_doctor_probes_on_a_systemd_host; doctor_inside_systemd_named_namespace_checks_actual_capture |
| R1.2 | Ignored and restored TERM semantics without namespaces | PASS | inherited_ignored_term_is_preserved_without_namespaces |
| R1.3 | UID-map denial is a setup capability result | PASS | namespace_probe_reports_uid_map_denial |
| R1.4 | PID 1 with wake-up disabled and older launcher semantics | PASS, 12 namespace runs | pid_one_namespace_finishes_journal |

## Decisions
[0008 Codex integration](../docs/decisions/0008-codex-integration.md): local hooks,
namespace finish ordering and settings ownership. Prior decisions remain in place.

## Measurements
WSL2 x86_64, kernel 6.18.58.1-microsoft-standard-WSL2, Rust and Cargo 1.100.0.
Bash hook wall time: 1.76 to 3.70 ms across local test runs.
5,000 cross-mount deletes plus undo: 1.60 to 2.27 seconds across local runs.
Existing 200-sample exec check: direct median 3.40 ms, p95 3.85 ms;
Saqta median 6.67 ms, p95 8.79 ms. These are development-build measurements.
R1: `cargo test --locked` passed 117 tests (10 unit, 107 integration).
`SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture` passed all 56 tests.
R1's 5,000-file bind delete and restore took 2.26 seconds. The existing exec
measurement was direct median 3.36 ms, p95 3.79 ms; Saqta median 6.66 ms,
p95 6.85 ms. Namespace cases also ran with synchronous wake-up disabled.
Final check logs and their measurements are in the R1 review bundle.

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
Hosted Ubuntu CI has not been rerun from these uncommitted changes. Review the
local reproductions and the runner's next CI result. No R1 commits will be made
until the user says this review passed.

## Commit plan
Original approved plan:
1. `exec: finish journals before PID 1 returns and label cross-mount copies`
2. `codex: capture Bash and patch hooks with M2 journals`
3. `codex: manage project hooks, settings and protection status`
4. `doctor: verify real capture inside the Codex sandbox`
5. `codex: add integration tests, documentation and review checks`
The five reviewed commits above already exist as b5cfa32, caff1f6, 0523f24,
875f156 and 3bda5d9. They will not be rewritten.

New commits proposed on top, after R1 review:
6. `doctor: check actual capture when supervised on systemd hosts`
   Share supervised detection, add the host-context unit regression and the
   systemd-named namespace reproduction.
7. `tests: probe namespace setup before running capture fixtures`
   Centralize capability probes and cover the UID-map failure independently.
8. `tests: establish PID 1 signal semantics across unshare versions`
   Restore fixture signal dispositions, cover ignored signals without namespaces,
   exercise both wake-up modes, and include this report.
Each new commit will build and pass its relevant tests. None is committed yet.
