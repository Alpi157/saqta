# fix-alias-growth: report

## What works now

Undo restores the bytes and metadata of a surviving hard link after growth through a new name whose source was unlinked or replaced.
The four saved seeds and eight named regressions pass, including undo interrupted after step 1.
Six additional regressions from review pass, including interruptions after steps 1 through 4 of undo.

## How it works

The first failing command starts with `d1/f0` and `d2/f1` pointing to the same 13-byte file. It calls `linkat` to add `d4/f1`, `unlinkat` to remove `d1/f0`, and `truncate` to grow `d4/f1` to 64 bytes. All three calls succeeded in the trace.

1. In `src/capture.rs`, `Capture::capture` records the new name as a create and remembers `d1/f0` as its source. For the unlink, `keep_link` takes a quarantine hard link before the syscall continues. When the unlink is observed, `Capture::complete` drops the source from `original_links`. The same tracking drops a source that was replaced by a rename.
2. Before the fix, the truncate branch froze the quarantine hard link into an independent 13-byte copy. It then skipped the growth record because `d4/f1` was created by the command and the recorded source was gone. This path returned before appending even the usual size-only record. The `kept` field describes how the delete was first captured, so it still says `hard link` after the freeze.
3. In `src/restore.rs`, `restore_effects` reverses the effects in descending sequence order. Before the fix, `Undo::plan` first moved the frozen delete copy back to `d1/f0`, then moved the created name `d4/f1` out of the project into recovery. The old inode stayed at 64 bytes through `d2/f1`. There was no growth effect for `prepare_sizes` or alias repair to use.

Before, `saqta show 1 --json` showed these two effects. Paths are relative to the fixture and the quarantine entry name is shortened.

```json
{"effects":[
  {"seq":1,"op":"create","path":"d4/f1","quarantine":"","note":{"before":null}},
  {"seq":2,"op":"delete","path":"d1/f0","quarantine":"2-…","note":{"kept":"hard link","before":{"size":13}}}
]}
```

With the fix, the truncate branch sees that the command created the name, its recorded source is gone, and the file still has multiple links. It calls `keep_copy` to freeze the delete entry and store a write pre-image. `Capture::handle` sends CONTINUE after capture and journal append. The new effect from `saqta show 1 --json` is below.

```json
{"seq":3,"op":"modify","path":"d4/f1","quarantine":"3-…","note":{"kept":"copy","before":{"size":13}}}
```

Undo now reverses effect 3 first. `Undo::alias_paths` includes the named `d4/f1`. `Undo::plan_repair` saves the 64-byte version and uses `execute_write` in `src/restore/steps.rs` to put the 13-byte pre-image, mode, and mtime back on the live inode. `d2/f1` shares that inode, so it gets the repair without being named in the command or journal. Undo then restores the deleted name and removes the created name. The trace finishes with both original paths at 13 bytes and with the 64-byte version in recovery. The fix reuses the existing capture and undo actions, with zero additional exec-path fsyncs or per-file subprocesses.

### Reproduction on `664690a`

`SAQTA_HARNESS_CASES=1 cargo test --locked --test harness` with the four additional `cc` lines, before adding the named tests or changing capture.

```text
test concurrent_subtrees_restore_exactly ... FAILED
test interrupted_undo_preserves_versions_and_resumes ... FAILED
test consecutive_commands_restore_every_snapshot ... FAILED
test result: FAILED. 31 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.88s
```

`cargo test --locked --test harness regression_` after adding the named tests and before changing capture.

```text
test regression_growth_through_new_hard_link_after_unlink_resumes ... FAILED
test regression_growth_through_new_hard_link_after_rename_over ... FAILED
test regression_growth_through_new_hard_link_after_unlinking_second_original ... FAILED
test regression_growth_through_new_hard_link_after_unlinking_first_original ... FAILED
test regression_append_through_new_hard_link_after_original_removed ... ok
test regression_shrink_through_new_hard_link_after_original_removed ... ok
test regression_in_place_write_through_new_hard_link_after_original_removed ... ok
test regression_truncating_write_through_new_hard_link_after_original_removed ... ok
test result: FAILED. 23 passed; 4 failed; 0 ignored; 0 measured; 15 filtered out; finished in 0.32s
```

The two cases that unlink `d1/f0` leave `d2/f1` different. The unlink and rename-over of `d2/f1` leave `d1/f0` different. Each shrink, append, and write regression tries both unlink and rename-over. Shrink uses the `Kind::Truncate` copy path. Append and both write modes reach `keep_copy` through `Kind::Open`. The write cases supply nonempty data.

### Why the failures appeared now

The harness draws 1–40 operations uniformly per vector. Each operation independently draws one of 15 kinds and two slots from 18 choices. The initial fixture has one inode with two names, at slots 8 and 11. There are four empty file slots where the link can land. In one ordered triple of positions, the chance of linking one of the initial names to one of these empty slots is `8/(15×18²)`. The middle operation must unlink that source or rename one of the six other initially occupied files over it, with probability `1/(15×18) + 6/(15×18²)`. Either growth kind, 5 or 6, can then target the new link, with probability `2/(15×18)`. Their product is about `6.02 × 10⁻⁸` per ordered triple.

Count all triples in the same command, including nonadjacent operations. Averaging `choose(length, 3)` gives 2,532 triples in one full vector. Averaging over the consecutive property's splits gives 1,110 per case. The concurrent property averages three independent worker vectors and 7,595 triples per case.

| Property | Expected matching triples per random case | Expected triples in 5,000 cases |
| --- | ---: | ---: |
| Consecutive commands | 0.000067 | 0.33 |
| Interrupted undo | 0.000152 | 0.76 |
| Concurrent subtrees | 0.000457 | 2.29 |

These are opportunities for the initial-fixture shape. Other operations can invalidate a matching triple or create different hard-link setups. The rates are therefore approximate guides to detection. The archived failures came after 806 consecutive and 1,853 concurrent successes in the scheduled run, and 363 interrupted and 4,274 concurrent successes in the branch run.

Proptest 1.12.0 replays saved seeds, then samples new cases with a random RNG seed by default. All three jobs used 5,000 cases and a shrink limit of 20,000. The earlier successful run checked out `fc971b9`, whose tree object matches `bd75176`. The growth shortcut is also present in `5972930`, the M5 merge. The two failures used different Ubuntu 24.04 image revisions and the artifacts record different seeds. The saved seeds failed before any new local case passed, including failures in the single-command, single-process regressions on WSL2. The evidence is consistent with random sampling of a rare shape. [Earlier successful job](https://github.com/Alpi157/saqta/actions/runs/44281619076/job/134723487956) · [Scheduled failure](https://github.com/Alpi157/saqta/actions/runs/44505830955/job/135497073031) · [Branch failure](https://github.com/Alpi157/saqta/actions/runs/44510544721/job/135513351876)

## Acceptance checklist

| # | Item | Result | Proven by |
| --- | --- | --- | --- |
| 1 | Reproduce the saved seeds and four minimal cases | All three properties and all four named growth cases fail before the fix | Before-fix output above |
| 2 | Trace capture and undo and check the other operations | Trace captured. Shrink, append, and both write modes pass with either removal | `saqta show 1 --json`, verbose undo, and named tests above |
| 3 | Reuse ADR 0022's copy and alias repair path | Pass | `src/capture.rs`, the after-fix trace, and the resume regression |
| 4 | Explain the timing of the failures | Analyzed | Operation probabilities and CI case counts above |
| 5 | Pass added tests and saved seeds | 42 passed in 6.62 s | `SAQTA_HARNESS_CASES=1 cargo test --locked --test harness` |
| 5 | Pass the 5,000-case harness locally | 42 passed in 955.01 s wall-clock time | `SAQTA_HARNESS_CASES=5000 cargo test --locked --test harness` |
| 5 | Pass the four-CPU flake hunt | 3 of 3 passed | `scripts/flake-hunt.sh 3 --mode ci4` |
| 5 | Measure delete, create, and rename-over capture cost | Median added cost varied by at most 0.4% | `scripts/overhead.sh` |
| 5 | Pass formatting, lints, and the full suite | Pass | `cargo fmt --all`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` |
| Review | Cover chained links, later writes and shrinks, deletion and rename of the new link, and interrupted undo | 48 harness tests passed in 20.06 s after adding the six review cases | `cargo fmt --all`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked --test harness` |

## Decisions

- [0025 Capture growth through new aliases whose original name is gone](../docs/decisions/0025-capture-growth-through-new-aliases.md)

## Measurements

AMD Ryzen 9 9900X, x86-64 WSL2, Linux `6.18.58.1-microsoft-standard-WSL2`, ext4, Rust 1.100.0.

The 5,000-case harness took 955.01 s wall-clock time, with 5,000 cases per property and 42 tests passed. The full suite took 33.89 s. The four-CPU flake hunt took 111.74 s across three full runs, with zero failures. Its summary is in `target/flake-hunt/20261119T112645Z.I2A8He/summary.md`.

### Capture overhead per file

`scripts/overhead.sh` ran release builds of `664690a` and the fixed working tree on CPUs 0–3, with five samples per workload and 20,000 files per sample. Each build had its own bare timings. The first attempted baseline hit the automatic discard breaker. Both measured runs used `SAQTA_RULES=off` to isolate capture and used the default synchronous wakeups. Added cost per file is the difference between the wrapped and bare medians divided by 20,000.

| Workload | Before bare / wrapped (ms) | After bare / wrapped (ms) | Before added (µs/file) | After added (µs/file) | Change |
| --- | ---: | ---: | ---: | ---: | ---: |
| Deletes | 205.84 / 868.39 | 193.93 / 853.80 | 33.13 | 32.99 | −0.40% |
| Creates | 1004.27 / 1638.57 | 1002.73 / 1635.85 | 31.71 | 31.66 | −0.19% |
| Rename-overs | 525.16 / 1495.13 | 531.88 / 1499.37 | 48.50 | 48.37 | −0.26% |

These differences are smaller than the spread in repeat timings. The measurements show comparable cost across the three workloads. The raw timings and full reproduction logs are in `target/alias-growth-evidence/`.

## Known gaps and risks

- The added pre-image uses the existing copy limit. Capture reports `not protected` when that copy fails.
- The restored deleted name comes from the frozen copy, so it has a different inode after undo. The surviving alias keeps its inode and gets its original bytes and metadata back.
- The full suite printed its environment skips for a Unix socket fixture, a tmpfs mount, a bare i386 syscall, `pytest`, `pnpm`, and the backing-device collision fixture.

## Questions for review

None.
