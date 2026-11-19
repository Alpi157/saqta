# ci-flakes: report

## What works now

The disabled Codex hook can finish before its caller writes the payload.
The detached-deletion test controls when its background job resumes.
CI shows failed test names and retains logs, harness seeds, and failure fixtures.
The manual flake hunt reports failures across repeated runs and keeps the evidence.

## How it works

In `tests/codex.rs`, the parent test process writes JSON to `saqta hook codex` through a pipe. With `SAQTA_OFF=1`, the child exits before reading stdin. When it exits first, the write has no reader. Rust ignores `SIGPIPE` in the test process, so the write returns `EPIPE` as `ErrorKind::BrokenPipe`. `write_hook_input` accepts that error and fails on any other write error. The regression polls `try_wait` before calling the same helper, then checks exit code zero and empty stdout and stderr. [Codex 0.169 hook runner](https://github.com/openai/codex/blob/rust-v0.169.0/codex-rs/hooks/src/engine/command_runner.rs#L315-L323)

The GitHub connector recovered PR #7’s first-attempt log. Ubuntu 24.04 exited 101 in `cargo test` because `setsid_background_deletions_remain_in_same_command` failed `assert!(!o.status.success())`. The rerun passed and the run has zero artifacts. The old test let the detached job delete its tree after 500 ms. I reproduced the assertion failure by holding the early undo for 750 ms. The test now releases the job after that undo returns, with a bounded wait in the job. The same 750 ms delay passed after the change. All existing assertions stayed in place. [PR #7 failing job](https://github.com/Alpi157/saqta/actions/runs/44281430824/job/134722822540)

In `.github/workflows/ci.yml`, Bash’s `pipefail` carries Cargo’s exit status through `tee`. The full run uses `--no-fail-fast` so Cargo runs the remaining test binaries after a failure. `scripts/ci-failure-summary.sh` puts failed test names and proptest minimal inputs ahead of panic details in the job summary. The upload retains the full logs, seed file, diagnostics and harness fixtures for 14 days. The artifact name includes the OS, run ID and attempt. [GitHub Actions shell behavior](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idstepsshell)

`scripts/flake-hunt.sh` builds once, runs Cargo for each iteration, and saves full output and a Markdown table. It resets `SIGINT` and `SIGQUIT` before starting Cargo in the background, so tests receive the normal signal dispositions while the script can reap its burners on interruption. Each run gets an isolated fixture root. The fixtures remove their data on success and the script prunes empty directories. It keeps nonempty failure fixtures and copies changed seed files next to the finding run’s log. The stress mode applies `SAQTA_NO_SYNC_WAKE=1` to fixture commands. The Unix socket test has a short `/proc/self/fd` address fallback for long fixture paths.

### Stdin audit

| Site | Can exit before a required read? | Result |
| --- | --- | --- |
| `tests/codex.rs`, `hook` | Yes. The disabled hook completes without reading. | Accept `BrokenPipe` in the shared hook input helper. |
| `tests/codex.rs`, `raw_garbage_hook_is_silent_and_successful` | The enabled hook reads stdin to EOF before trying to parse the invalid bytes. | Keep the write strict so the invalid input is delivered. |
| `tests/common/mod.rs`, `Running::spawn` | Its two input callers require the bytes. `tr` transforms them in the exec test and the harness driver reads its operation batch. | Keep the write strict. |
| `tests/timeouts.rs`, the stopped-supervisor test | The tracee waits at `read -r release` before making the syscall whose blocked state the test observes. | Keep the write strict. |
| `tests/capture.rs`, `killed_command_recovers_only_complete_intents_and_keeps_the_raw_tail` | The write appends to `effects.log`. | Keep the write strict. |
| `tests/speed.rs`, `malformed_complete_plan_refuses_resume_without_mutations` | The write appends to `state.log`. | Keep the write strict. |
| `tests/speed.rs`, PID reuse fixture | The Python parent reads the first PID before releasing the child from `os.read(0, 1)`. The second fork depends on that release. | Keep the write and flush strict. |

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
| --- | --- | --- | --- |
| 1 | Reproduce on the unmodified code | 3 of 60 failed with `BrokenPipe`. | Two CPUs, four burners per test, `hook_rewrite_and_fail_open` with `--exact`. |
| 2 | Match Codex and force the early exit | Pass. 0 of 60 failed after the fix. | Same loop and `hook_input_write_tolerates_an_early_exit_like_codex`. |
| 3 | Preserve CI evidence | Pass locally. The summary preserved the failing test and minimal input within 300 lines. | Forced harness failure, byte-for-byte `tee` probe with exit 101, and all three additional CI test commands. |
| 4 | Run the flake hunt | Pass. Both required tables are below. | Whole suite on four CPUs, focused hunt with four burners, stress run, and failure and SIGINT smoke tests. |
| 5 | Record the CI rules and decision | Done. | `AGENTS.md` and decision 0024. |
| 6 | Validate scripts, YAML and Rust | Pass locally. ShellCheck unavailable. | `bash -n` on both scripts, PyYAML 6.0.1, `cargo fmt --all`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --locked`. |

## Decisions

- [0024 CI keeps test evidence](../docs/decisions/0024-ci-keeps-test-evidence.md)

## Measurements

AMD Ryzen 9 9900X, x86-64 WSL2, Linux `6.18.58.1-microsoft-standard-WSL2`, ext4, Rust 1.100.0. The 60-run comparisons pinned both tests and burners to CPUs 0 and 1. The unmodified test failed on iterations 5, 35 and 44. The fixed test passed all 60.

### `scripts/flake-hunt.sh 3 --mode ci4`

| Test name | Failures out of 3 | First panic line |
| --- | ---: | --- |
| None | 0 | |

The initial script inherited ignored `SIGINT` from Bash and timed out in `group_sigint_reaches_command_and_wrapper` on all three runs. The final table uses the corrected runner.

### `scripts/flake-hunt.sh 40 --test codex --filter hook_rewrite_and_fail_open --busy 4`

| Test name | Failures out of 40 | First panic line |
| --- | ---: | --- |
| None | 0 | |

The stress smoke run also passed the bulk restore test. Its log reported `no_sync=true` and CPUs `0-1`.

### Evidence size

A complete passing run in a fresh `target/saqta-tests` scope left zero files and 806,912 allocated bytes in empty directories. A deliberate inverted model check with proptest RNG seed 42 left 72 case folders and 2,555 files. It wrote one regression seed. The fixture tree held 2,431,474 bytes of file data and occupied 16,908,288 bytes on disk. The model check and the tracked seed file were restored after the probe.

The modeled upload was 924,440 bytes compressed. It used the full suite and failure logs, the generated seed file, and the selected harness fixtures and diagnostic logs. CI uploads the four test log paths, `tests/harness.proptest-regressions`, `target/saqta-tests/**/logs/`, and `target/saqta-tests/harness-*/`. Cargo build output and unrelated fixture trees are excluded to avoid collecting binaries, journals and quarantine contents unrelated to the failure.

## Known gaps and risks

- ShellCheck was unavailable. Workflow YAML and behavior were checked locally. The new workflows have yet to run on hosted runners.
- The artifact size is measured from one forced harness failure. Failure fixtures and full logs can grow when several properties fail or shrink for longer.

## Questions for review

None.

### Planned commits after review

1. `tests: synchronize the Codex hook and detached undo fixtures`
2. `ci: retain test failure logs and harness seeds`
3. `ci: add a repeatable flake hunt`
