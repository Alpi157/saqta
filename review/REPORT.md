# fix-alias-inode-reuse: report

## What works now

Alias repair verifies birth time and generation and requires retained-data provenance.
Control files and size prefixes are excluded from repair candidates and alias maps.
Unverifiable legacy lineage reports a conflict and keeps both versions.
CLI JSON errors identify their source file.
The loaded replay has zero failures on the final build; validation is below.

## How it works

Undo now compares a file's device and inode number together with its birth time
and inode generation. A new file can reuse the number, but its recorded lifetime
markers differ. Size and mtime can change through another hard link, so they do
not define identity. Repair inside SAQTA_HOME also needs explicit retained-data
provenance. The detailed diagnosis below describes the behavior before the fix.

Capture now gets birth time with its existing stat. A quarantine hard link keeps
the original inode alive, so generation can wait until undo reads that link.
Capture saves the full identity before replacing a link with a copy; undo and GC
save it before consuming the link. Names without a pin record generation through
a file handle without opening the file. The r1 section below gives the cost and
lifecycle evidence.

### Reproducer and counts

Branch: `fix-alias-inode-reuse`. HEAD: `e11bdfeb04ac2be9f06b8ddf847d2fec66889329`.

`tests/harness.rs::interrupted_undo_inode_reuse_stress` is ignored by default.
It calls `check_interrupted(ops.clone(), 14)` in a loop controlled by
`SAQTA_INODE_REUSE_ITERATIONS`, which defaults to 1. It catches each panic to
count all iterations and retain each failed sandbox, then fails the test if
any iteration failed. This test was already present as an uncommitted change
when work began. It was preserved and its comments clarified. A field-by-field
comparison verified all 34 operations, including every data byte, against the
minimal input in the supplied `run-3.log`.

The harness was built once with `cargo test --locked --test harness --no-run`.
Each batch started four copies of that executable with these arguments:

```text
taskset -c 0,1 env SAQTA_INODE_REUSE_ITERATIONS=150 <harness-executable> interrupted_undo_inode_reuse_stress --ignored --exact --nocapture --test-threads=1
```

Four separate `taskset -c 0,1 yes > /dev/null` processes ran throughout each
batch. Each worker has its own sandbox and SAQTA_HOME. No extra SAQTA settings
were inherited. The load processes were stopped after the workers finished.
All workers completed all 150 iterations and exited 101 because their final
failure count was nonzero.

| Batch | Worker | Failed / 150 | Exit 125, expected value | Final tree mismatch |
| --- | --- | ---: | ---: | ---: |
| Requested | 1 | 1 | 0 | 1 |
| Requested | 2 | 4 | 2 | 2 |
| Requested | 3 | 2 | 1 | 1 |
| Requested | 4 | 3 | 1 | 2 |
| Requested total | | 10 / 600 | 4 | 6 |
| Confirmation | 1 | 4 | 4 | 0 |
| Confirmation | 2 | 3 | 1 | 2 |
| Confirmation | 3 | 5 | 4 | 1 |
| Confirmation | 4 | 2 | 1 | 1 |
| Confirmation total | | 14 / 600 | 10 | 4 |

The second batch captured the specific state.log variant after the first batch
corrupted other recovery files. Across both batches, 24 of 1,200 iterations
failed: 14 parse errors and 10 final tree mismatches. The parse-error fixtures
contain 4 corrupt state.log files, 4 corrupt state.json files, and 6 corrupt
directory-observations.json files. The tree-mismatch fixtures have recovery
entries showing repair of the current undo's size-2 or size-5 prefix source.

Logs and fixture lists: `target/alias-diagnosis/{run.json,worker-*.log}` and
`target/alias-diagnosis/confirmation/`. `counts.json` records counts and the
corrupted file in every failed fixture. `stress.py` reproduces the batch.

### A newly failed fixture

Fixture: `target/saqta-tests/harness-interrupted/s-41b49894ba/`.
Below, F means that fixture's absolute path, and U means
`F/SAQTA_HOME/q/main/undo-2-1795436799950505702`.
The original fixture is retained. Raw copies are in
`target/alias-diagnosis/fixture-evidence/`.

**state.log:** 28,437 bytes. The first 68 bytes are the 13-byte seed
`b"original 1 0\0"` followed by 55 more zero bytes. JSON starts at offset 68,
in the same newline-delimited frame:

```text
original 1 0<NUL x 56>{"completed_through":17,"origins":...
```

Later frames contain effect 16's repair plan. It copies captured data to
`HOME/project/d4/f1`, moves `U/state.log` to `U/1`, copies `U/1` to `U/1-post`,
writes the 68-byte pre-image into `U/1`, moves it back to `U/state.log`, then
moves `U/1-post` to `U/1`. The log retains device 2096, inode 8569612. `U/1`
retains 7,797 bytes of earlier JSON log contents on inode 8569638.
The captured effect-16 pre-image equals the first 68 bytes of state.log
byte for byte. A copy is included as `fixture-evidence/preimage-16.bin`.

**recovery.log:** exactly these three displacement records. Paths use F and U;
the original records are tab-separated.

```text
1  U/state.log             {"mode":33152,"dev":2096,"ino":8569612,"size":143,"mtime":1795436800,"mtime_ns":51252345}
2  F/HOME/project/d4/f1    {"mode":33188,"dev":2096,"ino":8569636,"size":68,"mtime":1795436799,"mtime_ns":970921591}
3  F/HOME/project/d0/f1    {"mode":33188,"dev":2096,"ino":8569153,"size":20,"mtime":1795436799,"mtime_ns":970921591}
```

`Undo::save_current` in `src/restore.rs` writes and syncs each line before
scheduling displacement. Record 1 proves that undo planned to displace its own
log. Mode 33152 describes a regular file with permissions 0600. Its size was
143 when selected for repair.

**undo-aliases.json:** valid JSON, 42 entries. Each value is an `[origin, actual]`
pair of State records. Relevant entries are:

| Path | Origin dev:ino, size | Actual dev:ino, size |
| --- | --- | --- |
| `HOME/project/d1/f0` | 2096:8569234, 13 | 2096:8569612, 13 |
| `HOME/project/d2/f1` | 2096:8569234, 13 | 2096:8569234, 13 |

The map also includes the previous undo's numeric backups, inode handles,
copy markers, size-2, recovery.log, directory-observations.json, state.json,
and state.log. These entries are from undo-1. It has not been republished for
the interrupted second undo.

`Undo::save_alias_observations` in `src/restore.rs` wrote this map after the
baseline undo. It records restored paths using `restored_origins`, then
recursively adds every regular file in that undo folder, including control
files, as `(actual, actual)`. It publishes through `journal::synced_json_write`
in `src/journal.rs`.

### Cause and writers

1. `tests/harness.rs::check_interrupted` executes and undoes the input as a
   baseline, then executes it again before the interrupted undo. The baseline
   leaves d1/f0 restored on inode 8569612. Its recorded origin is 8569234;
   d2/f1 still uses 8569234.
2. The second command grows d1/f0, links it as d4/f1, removes d1/f0, writes
   through d4/f1, then removes that last name. Capture of the write calls
   `Capture::keep_copy` and `Capture::freeze_links` in `src/capture.rs`. This
   replaces the delete's quarantine hard link with an independent copy.
   Removing the last live name frees inode 8569612. Effects 5, 12, 16, and 17
   in cmd/2/effects.log record that inode.
3. `Checkpoints::open` in `src/restore/checkpoints.rs` creates the second undo's
   state.log. ext4 reuses inode 8569612. The recovery log and current file stat
   prove the exact matching device and inode.
4. `restore_effects` reverses effects. For modify effect 16, `Undo::plan` calls
   `Undo::alias_paths`. No recorded live candidate retains the old data inode,
   so it recursively scans the current backup folder. It accepts any regular
   file satisfying `State::same_inode` in `src/journal.rs`, which compares only
   device and inode number. It accepts the new state.log despite its different
   size, mode, and mtime. This new log was found by the scan, rather than an
   entry for it in the previous undo's alias map.
5. `Undo::plan_repair` and `save_current` build the actions described above.
   `Undo::execute_write` in `src/restore/steps.rs` truncates the saved log inode
   and copies the data pre-image into it. The checkpoint descriptor remains
   open on that inode through the moves. `Checkpoints::record` then appends JSON
   and a newline to the 68 bytes of data, creating a malformed complete first
   frame. Scheduling and competing allocations affect which new file receives
   the reused inode number, so the victim and failure vary between runs.

### Why resume refuses

The debug abort in `Undo::execute` in `src/restore/steps.rs` exits 86 after
mutation 14 and before its completion checkpoint. The last intact JSON frame
records effect 6, cursor 3, steps 13. state.json is valid and still contains
the initial state with steps 0.

`restore::apply` calls `checkpoints::load` on resume. The loader reads
state.json, then deserializes every complete newline-delimited state.log frame
with `serde_json::from_slice`. The first frame starts with `o` from `original`,
so it returns `expected value at line 1 column 1`.
The malformed frame has a terminating newline, so ADR 0017's complete-frame
validation refuses it. Incomplete-tail preservation in `Checkpoints::open` is
never reached. `src/main.rs::main` prints the propagated error and exits 125.
The corruption was written by undo's planned data repair, with subsequent JSON
frames written by its checkpoint code.

## Acceptance checklist

| # | Item | Result | Proven by |
| --- | --- | --- | --- |
| 1 | Exact input, ignored test, environment-controlled loop | Pass | `interrupted_undo_inode_reuse_stress`; all 34 operations and bytes compared to CI log |
| 2 | Four workers, CPUs 0 and 1, four yes processes, 150 iterations each | Pass | run.json and worker logs; 10 / 600 failures |
| 3 | Inspect all three artifacts and explain their writers | Pass | Fresh fixture s-41b49894ba, raw evidence, code trace above |
| 4 | Explain resume parse failure | Pass | 68-byte non-JSON prefix, inode collision, `checkpoints::load` |
| 5 | Keep diagnosis outside src | Pass | `git --no-optional-locks diff --exit-code -- src/` |
| 6 | Write report and run review bundle | Pass; all seven checks passed | `scripts/review-bundle.sh alias-diagnosis` |

The review bundle passed formatting, Clippy, the full test suite (29 seconds),
forced proc-mem capture tests, the 256-case harness (49 seconds), doctor, and
dependency checks. The ignored stress test is intentionally run separately;
its observed failures above remain reproducible evidence of the unfixed bug.

## Decisions

No implementation decision or ADR change. Relevant records:
[0009](../docs/decisions/0009-resumable-undo.md),
[0017](../docs/decisions/0017-append-plans-and-batched-checkpoints.md), and
[0022](../docs/decisions/0022-retain-alias-repair-paths.md).

## Measurements

AMD Ryzen 9 9900X, 24 logical CPUs, x86-64 WSL2, ext4.
Linux `6.18.58.1-microsoft-standard-WSL2`, Rust and Cargo 1.100.0.
Both batches pinned all eight worker/load processes to CPUs 0 and 1.
The requested batch took 65.06 seconds; confirmation took 68.65 seconds.
Observed failure rates were 1.67% and 2.33%. These are local measurements,
not an estimate of a stable CI failure probability.

## Known gaps and risks

At the diagnosis stage, the bug remained present. Undo could corrupt resumability metadata and prefix
sources by treating recycled inode numbers as data aliases. Numeric displacement
copies survive in the inspected fixture, but automatic resume refuses. No
manual recovery was attempted and no failed fixture was repaired. Additional
tree mismatches were retained and traced to prefix-source repair; the diagnosis phase did
not implement a fix or establish full recovery for every fixture.

## Questions for review

How should a later fix prove that a candidate still belongs to a captured inode
incarnation? Which retained data handles may be searched and published as
aliases? Current recovery control files must stay outside that candidate set.


## Fix

### Mechanism

`src/sys/files.rs::incarnation_at` opens one non-followed descriptor and reads
statx birth time and FS_IOC_GETVERSION generation on that pinned inode. Every
descriptor is CLOEXEC. Unsupported or inaccessible fields stay absent. State
records the markers in its backward-compatible `incarnation` field.
`State::same_inode` requires the same device, inode, and all available markers,
with at least one marker present. It ignores size and mtime. Capture caches the
marker within each resolved notification path and checks it before reusing a
deduplicated pre-image.

The local ext4 probe returned both markers: birth `[1795439466,108584429]`
and generation `2661711291`. Generation zero is also valid and tested. The ioctl
returns the inode generation described by the [kernel ext4 documentation](https://kernel.org/doc/html/v5.3/admin-guide/ext4.html#ioctls).

`Undo::save_current` records retained files and files inside displaced directories
in `Recovery::data_handles` before their moves. The recovery checkpoint retains
that provenance across interruption. `Undo::alias_paths` searches this explicit
set, rather than scanning a folder. A candidate inside SAQTA_HOME must be under
a numbered data entry or inode-N entry in an explicit undo or automatic-hold
recovery folder, be registered as
retained data, and match its recorded incarnation. Control files, arbitrary
bookkeeping, copy markers, and size-N prefixes have no such role.
`repair_target` checks the same boundary before displacement and before a data
write. Prefixes remain immutable sources read by size repair; they are never
alias candidates or repair targets.

`Undo::save_alias_observations` publishes only verified data handles inside
SAQTA_HOME. `undo-data-handles.json` records their provenance separately from
`undo-aliases.json`. Old control entries are dropped. A cross-filesystem move
uses its existing copy publication marker to prove the newly retained copy,
then records its new incarnation and lineage. A file with no available marker
can still be displaced and independently restored at its named path; it cannot
authorize discovered-alias repair. Unverifiable discovered lineage reports a
conflict and retains both versions, even for old journals or alias maps.

Alias names move with reverse renames and directory exchanges. The original
alias map is checkpointed as alias-observations.json, so finalization after an
interruption cannot apply those name transformations twice. Changes made by
the command to a recorded ancestor are handled by its reverse actions. A
replaced discovered alias outside those actions causes a conflict.

`journal::{parse_json,read_json,read_json_optional}` attach the source file to
JSON errors on undo, resume, release, log, show, status, and gc paths. Framed
effect logs, post logs, checkpoint logs, copy markers, hold plans, and cached
SQLite summaries also identify their source.

### Regression coverage

- `recycled_inode_in_lineage_is_a_conflict_and_keeps_the_new_file` frees the
  inode named by lineage and creates up to 4,096 candidates. The measured run
  reused it on attempt 1. Undo reported a conflict and left the new bytes and
  the whole project snapshot unchanged. Unavailable reuse prints an explicit
  SKIP reason.
- `missing_incarnation_in_old_lineage_refuses_discovered_alias_repair` removes
  markers from the origin, the actual alias, and the journal in separate cases.
- `legacy_control_alias_entries_are_removed_on_undo` supplies a real control
  file through stale lineage and a bogus handle registry. Undo keeps its bytes
  unchanged and removes it from the alias map.
- `incarnation_identity_requires_all_markers_and_ignores_written_metadata`
  checks both markers, missing fields, generation zero, and legitimate writes.
- `assert_recovery_data_only` runs after every undo in each property, including
  the interrupted prefix and resume. It rejects recovery-log lines or alias-map
  entries that name bookkeeping inside SAQTA_HOME. The ignored CI reproducer
  remains intact.
- JSON regressions corrupt metadata on each requested CLI path and check that
  errors name the file. Resume covers state.json, state.log, directory and
  alias observations, and a copy publication marker.
- Two new property seeds and named regressions cover lineage across a directory
  exchange and a later undo of that exchange. The existing overlay regression
  covers a retained-data copy between filesystems. No test was weakened.

### Validation

| Check | Result | Evidence |
| --- | --- | --- |
| Full suite | 248 passed, 1 ignored; 48.28 seconds | `full-suite.log` |
| Clippy with -D warnings | Pass | `clippy-final.log` |
| Harness with 256 cases per property | 50 passed, 1 ignored | `harness-256.log` |
| Loaded exact replay | 0 / 600 failures on the final build, 118.12 seconds | `stress-with-holds/worker-*.log` |
| Earlier post-fix loaded replays | 0 / 1,200 failures | `stress-after/` and `stress-final/` |
| Three-run stress flake hunt | All 3 passed; 185.35 seconds | `flake-hunt-final.log`; `target/flake-hunt/20261123T133239Z.Qk41us/` |
| Harness with 5,000 cases per property | 50 passed, 1 ignored; 1,056.44 seconds | `harness-5000.log` |
| Final review bundle | All seven checks passed | `scripts/review-bundle.sh fix-alias-inode-reuse` |

Evidence paths in the table are under `target/alias-fix-evidence/` unless an
explicit path is given. The before-fix diagnosis measured 24 failures in 1,200
iterations: 14 parse errors and 10 tree mismatches. All 1,800 post-fix replay
iterations had zero parse errors and zero tree mismatches. The last 600 used the
final build, including automatic-hold and release support. A fixture counted
by the last driver's directory snapshot belonged to the concurrent 5,000-case
property run; all four replay workers completed successfully with 0 / 150 each.

An earlier flake hunt overlapped the four replay workers and four yes processes.
Its first run failed only `overhead_p95_is_below_fifty_milliseconds`: p95 was
50.999822 ms against 50 ms. The next two runs passed after that load ended. The
separate final hunt then passed all three runs. The timing test was unchanged.
The earlier output is retained as `flake-hunt-overlapping-load.log`. Four hold
and release regressions also exposed the missing automatic-hold data locations
in the first implementation. These were corrected; all 38 rules tests pass.
An initial 5,000-case run was interrupted so validation could restart after that
correction. It is retained as `harness-5000-interrupted.log`, not counted as a
completed validation run.

### Capture cost

Rules were off. Both measurements ran `scripts/overhead.sh` with N=20,000,
REPS=3, deletes/creates/rename-overs, bare/no-sync/sync variants, and both all-CPU
and CPU-0,1 affinity. The table shows median total capture time for the default
synchronous variant; bare timings and all samples are retained in the JSON.

| Affinity | Workload | Before ms | After ms | Total change | Added-cost change |
| --- | --- | ---: | ---: | ---: | ---: |
| All CPUs | Deletes | 897.14 | 1,217.88 | +35.75% | +47.68% |
| All CPUs | Creates | 1,754.93 | 1,887.62 | +7.56% | +6.69% |
| All CPUs | Rename-overs | 1,443.89 | 2,292.41 | +58.77% | +89.97% |
| CPUs 0,1 | Deletes | 863.43 | 1,096.90 | +27.04% | +24.31% |
| CPUs 0,1 | Creates | 1,746.35 | 1,983.20 | +13.56% | +19.76% |
| CPUs 0,1 | Rename-overs | 2,583.47 | 2,887.92 | +11.78% | +16.77% |

Added-cost change subtracts the corresponding bare median before comparison.
The additional cost per operation was 2.19 to 42.58 microseconds.

These local measurements include scheduling noise and the cost of the added
metadata reads, descriptor opens, and generation ioctls. They establish a
material performance cost; this change prioritizes verified recovery identity.
Evidence: target/alias-fix-evidence/overhead-{before,after}.json and
overhead-comparison.json. Hardware and kernel are unchanged from diagnosis.

### Decision and remaining limits

[0026: Verify the incarnation of alias repair targets](../docs/decisions/0026-inode-incarnation.md).
No dependency was added. Filesystems or permissions that expose no usable
marker cannot support discovered-alias repair; undo keeps the versions and
reports a conflict. Existing corrupted fixtures have not been repaired. The
existing process-crash guarantee and power-loss limits remain. No commit was
made; the working tree is ready for review.

Final bundle: `review/20261123-1845-fix-alias-inode-reuse.zip`. It includes the
working source, uncommitted patch, all review check output, stress and capture
measurements, and the original diagnostic fixture artifacts. The bundle passed
formatting, Clippy, full tests, forced proc-mem capture, harness-256, doctor, and
dependency checks.

## Follow-up r1: capture cost

This section updates the capture-cost result above. The diagnosis, original fix,
and its measurements remain in the report as reviewed history. Branch and HEAD
are unchanged. All earlier regressions and the ignored stress replay remain.

### How it works

A quarantine hard link keeps the original inode alive. Its number cannot be
recycled while that link exists. `Capture::keep_link` in `src/capture.rs` now
records that fact, the unique quarantine name, and birth time without opening
the target for a marker read. `sys::stat_capture_at` requests birth time in the
statx call capture already needed. Post-operation State observations use statx
in place of fstatat, rather than adding a second stat call. The creation outcome
check uses `Entry::exists` and needs only the original stat. Its discarded result
never feeds alias repair. The final snapshot still records full incarnation
identity, so creation now needs one generation lookup instead of two.

`journal::complete_pins` reads generation through the exact retained hard link
when the command is loaded. It verifies device, inode and recorded birth time.
This is completion through a still-live pin, never a search by inode number.
Inspection stays read-only. `journal::persist_pins` saves those completed States
in `cmd/<id>/inode-pins.json` before undo moves a pin or GC unlinks it. GC saves
this before its expiry record. `Capture::freeze_links` gets generation through
its already-open copy source and calls `Journal::record_pin` before replacing
the quarantine link with a copy. The sidecar uses quarantine names as keys;
it never combines two incarnations merely because their stat numbers match.
Cross-mount captures also read generation from the source already open for copying.

Unpinned rename sources, created files and final path observations cannot safely
wait until undo. They might disappear first. For those names,
`sys::generation_at` uses `name_to_handle_at` through the existing parent
folder descriptor. It accepts only the kernel's exact eight-byte
FILEID_INO32_GEN format and a matching inode. This gives the same inode generation
as FS_IOC_GETVERSION, without opening the file. The local comparison test found
both birth time and an identical generation through these two interfaces.
The kernel defines the [handle layout](https://raw.githubusercontent.com/torvalds/linux/v6.18/include/linux/exportfs.h)
and [fills it from i_generation](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/libfs.c).

This is a deliberate addition to the suggested direction. Reading generation
only through existing file descriptors would leave unpinned names with birth
time alone. ext4 can assign the same timestamp within a clock tick; its
[inode allocator initializes generation independently](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/ext4/ialloc.c).
Pinned inodes get generation before that pin can end; unpinned regular files
get it immediately through the handle. There is no marker-only target open in
capture. Unsupported handle formats or failed reads mark an unpinned observation
`unverified`, while preserving its measured fields. It cannot authorize alias
repair, even when birth time matches. A copy's existing source descriptor can
complete it with the ioctl. Undo still obtains fresh markers from its pinned
candidate descriptor and applies the same strict marker comparison. Size,
mtime, retained-data provenance and the control-file exclusions are unchanged.

The 4,096-attempt local probe did not observe reuse with an equal birth timestamp.
That does not establish uniqueness. The unit regression explicitly rejects equal
birth times with different generations. The original bounded reuse regression
and new unavailable-generation regression exercise the conflict paths.

A successful quarantine link also proves identity while the interrupted syscall
completes. `Capture::applied` uses that live-pin proof to recognize a failed
unlink, without confusing its incomplete pre-state with a full current marker.
This dev/inode comparison is limited to a known live pin, as is the selection of
links to freeze. Neither is a discovered-alias identity check.

The byte-path encoder now bulk-copies plain ASCII names. Its percent-encoding
format and arbitrary-byte round-trip test are unchanged. No dependency was added.

When missing generation prevents proving a rename or exchange outcome, capture
leaves it unknown. An unknown outcome is considered by undo. It is never written
as a negative outcome that would silently skip the operation. Pending observation
also waits for positive proof, the next notification from that thread, or exit.
The new unavailable-generation rename regression checks both versions survive.


### Syscalls: 2,000 operations per workload

The ptrace counter follows fork and exec and counts actual syscall entries in
one `__supervisor` process. It excludes wrapper and command work, and includes
supervisor startup and teardown. It detaches the filtered command so tracing
does not change process-memory permissions. Each run checked command exit,
one supervisor, exactly 2,000 effects (4,000 for rename-overs), and no unprotected
capture. The snapshots each had their own Cargo target directory.

| Workload | e11bdfe calls/op | Reviewed calls/op | r1 calls/op |
| --- | ---: | ---: | ---: |
| Deletes | 15.1220 | 20.1230 | 15.1240 |
| Creates | 14.1245 | 24.1230 | 15.1235 |
| Rename-overs | 19.1240 | 39.1235 | 22.1235 |

Deletes add no per-file identity calls. Creates add one generation lookup in the
final snapshot. Rename-overs add three: source, successful outcome, and final
snapshot. These names have no quarantine pin. The reviewed implementation added
5, 10, and 20 calls respectively. Small differences in memory allocation counts
are retained in the totals above.

| Workload | e11bdfe openat calls | Reviewed openat calls | r1 openat calls |
| --- | ---: | ---: | ---: |
| Deletes | 22 | 2022 | 22 |
| Creates | 21 | 4021 | 21 |
| Rename-overs | 22 | 8022 | 22 |

No target is opened just for markers. The remaining openat calls are fixed
startup/store work, also present in e11bdfe. Parent-directory openat2 calls are
unchanged. The new inotify test measured zero file IN_OPEN events for deletes.
Raw counts, stderr, the C counter, and `count.py` are in the r1 evidence.

### Capture timings

AMD Ryzen 9 9900X, 24 logical CPUs, ext4 on WSL2, Linux
`6.18.58.1-microsoft-standard-WSL2`, Rust/Cargo 1.100.0. Rules off, N=20,000,
REPS=3, both affinities, and all bare/no-sync/sync variants. The unchanged
`scripts/overhead.sh` ran each baseline/fixed workload block consecutively,
with setup and cleanup untimed. No other validation ran during measurement.
Every sample is retained, including earlier prototypes.

Before is e11bdfe; after is r1. Default synchronous capture, medians:

| Affinity | Workload | Before ms | After ms | Total change | Added-cost change |
| --- | --- | ---: | ---: | ---: | ---: |
| All CPUs | Deletes | 873.77 | 856.53 | -1.97% | -1.05% |
| All CPUs | Creates | 1,738.11 | 1,748.21 | +0.58% | -1.86% |
| All CPUs | Rename-overs | 1,502.47 | 1,606.14 | +6.90% | +10.39% |
| CPUs 0,1 | Deletes | 878.30 | 873.09 | -0.59% | -1.10% |
| CPUs 0,1 | Creates | 1,696.98 | 1,720.34 | +1.38% | +5.28% |
| CPUs 0,1 | Rename-overs | 2,516.59 | 2,632.14 | +4.59% | -14.14% |

Before is the reviewed identity fix at the start of r1; after is r1:

| Affinity | Workload | Before ms | After ms | Total change | Added-cost change |
| --- | --- | ---: | ---: | ---: | ---: |
| All CPUs | Deletes | 1,073.82 | 856.53 | -20.24% | -22.87% |
| All CPUs | Creates | 2,035.29 | 1,748.21 | -14.11% | -21.37% |
| All CPUs | Rename-overs | 2,331.47 | 1,606.14 | -31.11% | -33.24% |
| CPUs 0,1 | Deletes | 1,026.45 | 873.09 | -14.94% | -8.88% |
| CPUs 0,1 | Creates | 2,179.71 | 1,720.34 | -21.08% | -37.11% |
| CPUs 0,1 | Rename-overs | 2,822.12 | 2,632.14 | -6.73% | -8.67% |

Added-cost change uses (sync median minus the corresponding bare median),
as in the previous table. The maximum increase is 10.39%, below the 15% gate.
These three-sample measurements do not establish zero residual cost. In the
all-CPU rename run, r1 samples span 1,453.38 to 1,613.95 ms,
which includes the baseline median. The extra generation calls remain visible
in the syscall evidence.

Bare rename medians on CPUs 0,1 were 525.54 and 922.72 ms,
even though the bare program is identical. Its negative added-cost delta is
not evidence of a speedup. Using one shared bare median from the six bare
samples in each block gives a maximum increase of 9.93%,
and every combination still passes 15%. This check is recorded separately; it
does not replace the requested calculation above.

The first full-run comparison of the pin prototype reported +25.50% and +22.82%
added rename cost with substantially different bare medians. That result is
retained in `overhead-after.json` and `overhead-baseline.json`. Adjacent blocks
reduced this measurement drift. Later code changes added conservative fallback
handling and removed the discarded creation lookup. No preliminary run is
counted as an extra sample of the final source.

Final evidence: `overhead-paired-{baseline,after}.json`, `overhead-reviewed.json`,
`paired-*.{json,log}`, `counts-{baseline,reviewed,after}.json`,
`performance-gate.json`, `shared-bare-comparison.json`, and
`final-source-sha256.json` under `target/alias-r1-evidence/`.

### Regressions and validation

All earlier tests remain, including the ignored exact CI replay and the
control-file invariants after every property undo. New tests cover:

- File-handle generation equals the ioctl, including a real local birth-time probe.
- Delete capture emits zero file IN_OPEN events.
- Pin completion stays read-only until undo, and survives consuming the pin.
- Freeze records the original generation before replacing a link with a copy.
- GC records generation before expiry and collection; dry-run metadata stays unchanged.
- Missing handle generation refuses discovered-alias repair and keeps both versions.
- Birth time alone cannot complete an unverified historical incarnation.
- Failed unlink through a live pin is recorded as unapplied and leaves its inode unchanged.
- Unverifiable rename outcomes stay unknown, with both versions retained.
- Corrupt inode-pins.json errors identify that path on the requested CLI commands.

| Check | Result | Evidence |
| --- | --- | --- |
| Alias, pin, watcher and fallback tests | 15 passed, including two ABI checks | `lifecycle-final.log` |
| JSON-path and retention tests | 4 and 7 passed | `lifecycle-final.log` |
| Bounded real inode reuse on final source | Reused inode 8475728 on attempt 1; conflict, new bytes and project unchanged | `inode-reuse-final.log` |
| Full suite | 257 passed, 1 ignored; 33.65 seconds | `full-suite.log` |
| Loaded replay: four workers, 150 iterations each, four yes processes, CPUs 0,1 | 0 / 600 failures; 77.40 seconds | `stress-final/run.json`, `worker-*.log` |
| SAQTA_HARNESS_CASES=5000 cargo test --locked --test harness | 50 passed, 1 ignored; 971.04 seconds | `harness-5000.log` |
| scripts/flake-hunt.sh 3 --mode stress | All three passed; 154.55 seconds | `flake-hunt.log` and bundled per-run logs |
| cargo fmt --all, then --check | Pass | `fmt.log` |
| cargo clippy --all-targets --locked -- -D warnings | Pass | `clippy.log` |
| Supervisor call counts on three versions | Pass, all nine fixtures captured every operation | `counts-*.json` |
| Capture added-cost gate | All six comparisons below 15% | `performance-gate.json` |
| Final review bundle | All seven checks passed | `scripts/review-bundle.sh fix-alias-inode-reuse-r1` |

Both parse errors and tree mismatches were zero in every replay worker. No
failed fixture remained. The load was stopped before the large harness and
flake hunt. Before the original correctness fix the replay had 24 failures in
1,200 iterations. The r1 source was not changed during measurements or validation.

The final targeted inode-reuse regression freed inode 8475728 and received that
number on its first allocation. Undo reported a conflict, left the new file
unchanged, and preserved the complete project snapshot. The earlier lifecycle
run printed the permitted SKIP when 4,096 allocations did not reuse its inode.
No bound or assertion was weakened. The malformed truncate fixture in
`lifecycle-wrong-truncate-fixture.log` opened the file before truncation; the final
test uses os.truncate to exercise a generation read without a copy descriptor.

### Decision and limits

[ADR 0026](../docs/decisions/0026-inode-incarnation.md) now records when capture
can defer markers, when it must save them, and why coarse timestamps require
generation. It remains under 300 words. No dependency was added.

Only the exact generic inode/generation handle format is decoded. Other formats
or denied calls cause conservative conflicts for unpinned lineage. Pin completion
adds identity work when a command is inspected or undone, and saves a sidecar
before consuming its pins. Capture no longer pays for marker-only target opens.
The existing process-crash guarantee and power-loss limits are unchanged.

All r1 evidence is under `target/alias-r1-evidence/` and in the review bundle.
The earlier diagnosis and fix evidence are also retained in that bundle. The
branch is `fix-alias-inode-reuse`; no commit was made.

Final r1 bundle: `review/20261124-1056-fix-alias-inode-reuse-r1.zip`. Its seven
checks passed: format, Clippy, full tests, forced proc-mem capture, harness-256,
doctor and dependency checks. The archive includes the working source and patch,
all check output, raw r1 measurements and replay logs, source snapshots for
both comparisons, and the preserved earlier diagnosis and fix evidence.
