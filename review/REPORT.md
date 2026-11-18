# M5 speed: report

## What works now (5 lines max)
- Capture uses bounded task caches and keeps pre-images and complete intents before CONTINUE.
- Undo uses append-only plans, periodic checkpoints and pinned private store parents.
- Tests cover exec with a readable old mm, actual PID reuse, root and mount changes, and interrupted cross-mount publication.
- The extended harness found an M4 shared-alias defect. The fix, minimized regression and seed are retained.
- Delete and create meet 25 us. Same-mount undo meets five seconds. Review accepted the rename-over limit.

## How it works

filter::CONTEXT_CALLS sends exec and filesystem-context changes to the supervisor.
TaskCache in src/capture/context.rs discards its one cached task before continuing
such a call. Only the issuer's next notification can repopulate it. Other tids
use temporary context. This prevents another thread from retaining an old mm or
root while a context change is in flight. A pinned pidfd checks lifetime. If
unavailable, a pinned proc-stat descriptor checks the original task's start time.
That descriptor cannot follow a reused PID number. Read failure reopens memory
once. Reap, idle checks and command end close cached descriptors.

PathReader in src/sys/memory.rs remembers VM-read denial and reads bounded pages
through proc memory. Page bytes live only for one stopped notification. Both
notification validity checks remain. Cwd display names are refreshed; numbered
dirfds open afresh. Simple names, including ./file, use a pinned base. Other
paths retain the restricted root walk. Filesystem decisions use statx mount identity and pin the mount against ID reuse. Mount-context calls release classification and observation
pins before CONTINUE, so the caches cannot keep mounts busy.

Capture::complete updates creation and inode bookkeeping before later calls can
obscure it. Ordered subtree ranges replace whole-set deletion scans.
Journal::append_batch writes one notification's effects together, after its
pre-images and before its reply. Positive outcomes wait for atomic snapshot
publication. Unknown already replays exactly like true, so losing unpublished
positives changes no recovery decision. False outcomes still write immediately.
Post-state serialization flushes before status or end publication. Intent never
waits past its dependent CONTINUE.

restore::Checkpoints appends each full plan before mutation. It publishes
state.json every 128 frames or 250 ms, and at completion, before truncating
state.log. Resume validates all complete frames and uses the latest. An
incomplete final frame authorizes no mutation; its exact bytes are retained.
Existing move, exchange and copy-publication identities recognize completed
actions. The synced displacement map is unchanged. Only command quarantine and
private undo parent fds are reused across effects, under the command lock.
Working-tree parents keep fresh secure walks.

Undo remembers absences proved by preflight or completed moves. Copies, exchanges
and directory moves invalidate that knowledge. NOREPLACE still protects entries
that appear later. Resume starts without remembered absences. Directory
observation collection uses a hash set. Reverse path mapping visits affected
subtrees. Observation values and conflict checks remain unchanged.

The alias regression links an original inode under a new name, deletes its
recorded origin, then writes through that new name. Untouched M4 leaves another
surviving alias changed. M5 conservatively captures shared created aliases and
repairs their inode before removing the new name.

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
|---|---|---|---|
| 0 | Build main first into a separate target | PASS | Main e138ea2; cargo build --release --locked --target-dir target/baseline-m4; provenance.json |
| 1 | Every workload, both wake modes, normal and CPUs 0,1, medians of three | PASS | scripts/overhead.sh; baseline.json and m5.json |
| 2 | Added delete cost below 25 us | PASS, 19.01/18.77 us | Final timing table |
| 3 | Added create cost below 25 us | PASS, 18.48/16.82 us | Final timing table |
| 4 | Added rename-over cost below 25 us | ACCEPTED LIMIT, 25.31/88.75 us | Final timing table; user review passed 2026-11-18 |
| 5 | Read-only workloads unchanged | PASS | Stat and git-status rows; argument-aware filter tests |
| 6 | Same-mount rescue undo below five seconds | PASS, max median 4.561 s | Separate timers and exact file snapshots |
| 7 | Cross-mount restore remains proportional | PASS, 2.20x undo for 2x files | 30,000 and 60,000 file measurements |
| 8 | Existing suite passes | PASS, 176 tests | cargo test --locked |
| 9 | Unchanged interrupted-undo property at 256 cases | PASS, 47.42 s | SAQTA_HARNESS_CASES=256 cargo test --locked --test harness; 29 tests |
| 10 | Forced proc-memory run | PASS | Capture and speed suites with SAQTA_FORCE_PROC_MEM=1 |
| 11 | Exec invalidation, including a readable retained old mm | PASS | exec_between_deletes_reads_the_new_address_space; exec_invalidates_readable_old_mm_retained_by_clone_vm |
| 12 | Actual PID reuse, pidfd and proc-stat fallback | PASS | reused_pid_reads_the_new_process_memory, prints reused PID in both modes |
| 13 | Root, cwd and numbered dirfd changes | PASS | chroot_between_deletes_uses_the_new_root; fchdir_between_deletes_invalidates_cached_cwd; directory_fd_reuse_and_cwd_rename_use_fresh_parents |
| 14 | Mount change and pins do not block unmount | PASS | bind_mount_change_on_cached_device_still_captures_the_new_file; skipped_filesystem_cache_does_not_keep_a_mount_busy |
| 15 | Partial plans retained; damaged complete plans refuse | PASS | resume_keeps_a_partial_plan_tail_and_replays_the_last_complete_plan; malformed_complete_plan_refuses_resume_without_mutations |
| 16 | Cross-mount source removal can resume | PASS | cross_mount_publication_resumes_after_source_removal |
| 17 | Positive batching preserves crash replay and rejects damage | PASS | unpublished_positive_batch_keeps_failed_outcomes_and_recovers_successes; damaged_published_positive_batch_refuses_undo |
| 18 | Interrupted grouped intents keep both original names | PASS | unacknowledged_rename_batch_prefix_preserves_both_original_names |
| 19 | Shared created alias repairs surviving inode | PASS | regression_write_new_alias_after_removing_origin_repairs_surviving_inode; saved seed f7c0d98c... |
| 20 | Formatting, clippy, release configuration and bundle | PASS | scripts/review-bundle.sh m5-speed |
| 21 | Mount identity separates stacked filesystem decisions; uncached fallback | PASS; backing-device collision subcase SKIP on this kernel | overlay_backing_device_does_not_reuse_tmpfs_skip_decision |

## Decisions

- [0016](../docs/decisions/0016-bounded-task-context-cache.md): task identity, context barriers, bounded memory and root caches, and filesystem pins.
- [0017](../docs/decisions/0017-append-plans-and-batched-checkpoints.md): write plans before mutation and publish periodic checkpoints.
- [0018](../docs/decisions/0018-linear-path-bookkeeping.md): subtree updates, post-state buffering and shared aliases.
- [0019](../docs/decisions/0019-batch-only-positive-outcomes.md): positive batches preserve the unknown-equals-true replay rule.
- [0020](../docs/decisions/0020-write-one-notifications-effects-together.md): one stopped syscall's effect lines share a write.
- [0021](../docs/decisions/0021-key-filesystem-decisions-by-mount.md): distinguish overlay and backing mounts, with uncached fallback.

No dependencies were added or changed. S remains single threaded. A thread pool
would not shorten the serialized one-notification-at-a-time workloads, and
would add ordering and shared-inode obligations.

## Measurements

Machine: AMD Ryzen 9 9900X, 24 logical CPUs, WSL2 x86_64, ext4.
CPUs 0 and 1 are the two SMT threads of core 0.
Kernel: 6.18.58.1-microsoft-standard-WSL2. Rust: stable 1.100.0.
Measurements ran inside the Codex sandbox. M4 was built from main e138ea2 before
production changes, using target/baseline-m4. Experimental snapshots used their
own target directories. M5 uses target/m5-speed. provenance.json records binary
and source hashes. The final M5 timing pass ran without concurrent test jobs.
The dependency audit uses a disposable snapshot of the cached advisory database
under target/saqta-tests. Its policy is unchanged, and it does not take a lock
in the real home directory.

Every table cell is the median of three fresh fixtures. Setup, hashing and
cleanup are untimed. Capture and undo have separate timers. Every rescue undo
is checked against its complete pre-command file snapshot: mode, mtime and
SHA-256 bytes.

Rescue has 60,000 project files in 1,800 leaf directories, plus Git metadata
and the tracked ignore file. It contains 600 dirty tracked files, 39,600
untracked files and 19,800 ignored files, from empty to 64 KiB. Git uses
--no-optional-locks. The cross-mount worker binds SAQTA_HOME separately and
retains that namespace for both capture and undo, exercising EXDEV in both
directions. Affinity applies to each timed command and undo. Namespace setup
is untimed.

All times in the next two tables are milliseconds. Capture includes startup and final publication.

CPU affinity: all.

| Workload | M4 bare | M5 bare | M4 no sync | M5 no sync | M4 sync | M5 sync |
|---|---:|---:|---:|---:|---:|---:|
| deletes | 213.8 | 201.4 | 3297.3 | 2463.9 | 1580.1 | 869.1 |
| creates | 1061.6 | 980.5 | 5232.5 | 3607.2 | 2819.6 | 1623.7 |
| rename-overs | 492.2 | 475.4 | 5562.8 | 3294.1 | 3788.0 | 1522.3 |
| read-only stats | 47.5 | 40.6 | 46.5 | 45.1 | 50.1 | 45.5 |
| git status | 45.1 | 45.0 | 47.5 | 44.1 | 49.6 | 56.3 |
| clean cargo build | 7458.7 | 6582.1 | 7814.8 | 6970.6 | 7087.7 | 6866.2 |
| rescue | 799.3 | 750.0 | 16253.9 | 8271.0 | 11432.7 | 3303.2 |
| rescue-cross-mount | 745.8 | 750.6 | 19297.0 | 12119.9 | 13712.9 | 6356.6 |



CPU affinity: 0,1.

| Workload | M4 bare | M5 bare | M4 no sync | M5 no sync | M4 sync | M5 sync |
|---|---:|---:|---:|---:|---:|---:|
| deletes | 219.9 | 199.7 | 3419.4 | 2461.3 | 1507.2 | 879.2 |
| creates | 961.7 | 996.5 | 5346.6 | 3608.0 | 2745.9 | 1644.3 |
| rename-overs | 515.1 | 525.8 | 5618.8 | 3313.0 | 4789.9 | 2552.5 |
| read-only stats | 40.7 | 41.8 | 46.4 | 48.0 | 46.0 | 47.8 |
| git status | 47.6 | 37.5 | 86.8 | 80.2 | 43.1 | 52.0 |
| clean cargo build | 9921.8 | 10102.7 | 10355.8 | 10532.6 | 11314.9 | 10329.9 |
| rescue | 744.9 | 760.2 | 15039.6 | 8283.3 | 10557.2 | 3254.6 |
| rescue-cross-mount | 765.9 | 762.7 | 19784.4 | 12097.1 | 14075.6 | 6419.4 |



Rescue undo, seconds. Every cell is a median of three. Undo runs without the supervisor; wake-up columns identify the preceding capture.

| CPUs | Workload | M4 no sync | M5 no sync | M4 sync | M5 sync |
|---|---|---:|---:|---:|---:|
| all | rescue | 19.671 | 4.129 | 19.769 | 4.375 |
| all | rescue-cross-mount | 31.105 | 16.625 | 31.863 | 17.394 |
| 0,1 | rescue | 21.793 | 4.561 | 21.040 | 4.020 |
| 0,1 | rescue-cross-mount | 37.834 | 19.942 | 37.984 | 19.976 |



The following per-operation values exclude startup and final snapshots. Each sample is the median of 20,000 timed operations; each cell is the median of three samples. Names are prepared before the clock. Create includes open, write and close. Added cost subtracts the corresponding bare median. Raw nanosecond arrays are retained in latency.json.

| CPUs | Operation | Bare us | M4 no sync | M5 no sync | M4 sync | M5 sync | M5 added sync us |
|---|---|---:|---:|---:|---:|---:|---:|
| all | deletes | 8.630 | 144.843 | 100.652 | 51.841 | 27.640 | 19.010 |
| all | creates | 37.230 | 183.393 | 143.523 | 79.442 | 55.711 | 18.481 |
| all | rename-overs | 20.261 | 203.504 | 151.553 | 107.097 | 45.571 | 25.310 |
| 0,1 | deletes | 8.670 | 133.673 | 110.462 | 53.731 | 27.441 | 18.771 |
| 0,1 | creates | 38.731 | 173.163 | 148.263 | 83.371 | 55.547 | 16.816 |
| 0,1 | rename-overs | 20.711 | 205.974 | 148.112 | 177.404 | 109.462 | 88.751 |



Cross-mount scaling with synchronous wake-up, unrestricted CPUs. Seconds, medians of three. Scaling and per-operation comparisons use the retained immutable M4 binary after implementation.

| Files | M4 capture | M5 capture | M4 undo | M5 undo |
|---:|---:|---:|---:|---:|
| 30000 | 5.477 | 3.162 | 13.642 | 7.902 |
| 60000 | 13.713 | 6.357 | 31.863 | 17.394 |


Doubling files multiplies M5 cross-mount undo by 2.20.

The first exploratory fixture used 20,400 dirty tracked files instead of 600.
Its first M4 no-sync sample measured capture 162.398 s and undo 211.485 s,
before production changes. This exposed whole-set scans and is retained in
heavy-initial-baseline.*. It is separate from the fixed three-run comparison.
Baseline collection continued against unchanged M4 code while development used
an isolated snapshot. The fixed baseline therefore includes development load.

Supervisor-only affinity did not consistently help. Its two-CPU rename added
cost was 55.81 us versus 51.79 us without that change. The experiment was
rejected and is absent from the final source.

M2's code inventory counted 29 calls per steady-state deletion and 26 per
creation, including deferred outcome and the event loop. M5's warm simple-name
path with cached cwd uses about 12 and 11: outer poll, receive, prior outcome
stat, pidfd poll, memory pread, two validity ioctls, cwd readlink, final stat,
optional link, effect write and reply. Numbered dirfds, cold caches, multiple
pages, errors and context changes add calls. This is a code inventory, not
strace. Positive publication and final snapshots are command-end work. The
timing table includes them.

## Known gaps and risks

- Rename-over remains above 25 us, including on two SMT siblings. The user accepted this known performance limit when review passed on 2026-11-18. README records the measurements.
- One task cache deliberately gives up hits when other tids intervene. Numbered dirfds reopen for each notification.
- CONTINUE retains the existing races with unrelated filesystem changes and hostile argument mutation. No broader security or power-loss guarantee is claimed.
- New interrupted undo logs need the M5 resume reader. M5 still reads older state.json-only recovery plans.
- Unpublished positive outcomes remain unknown to inspection. Replay already treats them as applied. Failed outcomes stay explicit immediately.
- The OverlayFS collision configuration is unavailable on this kernel. Overlay capture and the no-mount-ID fallback both ran and restored bytes.
- The fixed rescue has 600 dirty tracked files. The first, much larger dirty tracked population was not measured three times.
- Cross-mount regular-file copying remains more expensive. Existing refusals for nonempty directory and special-file cross-filesystem copies remain.
- Hard-link freezing and origin tracking retain their existing full-set scans. These timings do not characterize those workloads.
- Exact conditional skips are retained in validation logs.

## Questions for review

- Check the single-issuer cache barrier against concurrent exec, CLONE_FS and nonleader exec.
- Check positive-only batching across supervisor death and index rebuilds.
- Check checkpoint publication before truncation, incomplete-frame retention and cross-mount resume after source removal.
- The rename-over target remains unmet. Review accepted the measured limit without weakening capture or undo guarantees.

## Commit plan

The user approved review and this five-commit plan on 2026-11-18. Each staged
snapshot is validated with its own Cargo target directory before committing.

1. perf: add repeatable rescue and capture measurements, binary selection and raw samples.
2. capture: cache task context behind lifecycle barriers, including syscall wrappers and cache tests.
3. capture: bound path bookkeeping and batch safe journal writes, including the alias fix and crash tests.
4. undo: append plans and reuse locked store parents, including diagnostics and interrupted cross-mount tests.
5. docs: report M5 results and package validation, including decisions and review checks.

Each code commit includes its relevant test hunks. The mixed speed tests are
split by feature when staging. This plan does not include a push.
