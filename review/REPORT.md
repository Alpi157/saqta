# m2-capture-r1: report

## What works now (5 lines max)
- Exec no longer syncs files; capture ordering, byte paths, the deny floor and undo syncs are unchanged.
- Per-command logs preserve pre-images and post-states. The CLI builds a SQLite index.
- `log`, `show`, and guarded `undo` work, including background trees and supervisor crash recovery.
- All M1 tests and 45 capture scenarios pass. The capture suite also passes with forced /proc memory reads.
- CI triggers, decision records, a release measurement script and an eight-commit review plan are included. Review passed; the approved work is committed in eight logical commits. Nothing was pushed.

## How it works

Follow `saqta exec -c 'rm -rf tree'`. W in `src/exec.rs` prepares command strings, environment and internal supervisor argv before forking. The fork child re-execs Saqta as S. S now has a fresh Rust runtime, so it can allocate its journal and capture state without allocating in a fork child. S becomes a subreaper, forks C and detaches its streams. C uses only syscall wrappers until it installs the filter, passes the listener, closes the sockets and execs `rm`.

`filter::program` in `src/filter.rs` suspends each unlinkat and directory removal. `Capture::handle` in `src/capture.rs` reads the pathname page by page. `sys::read_path` first tries process_vm_readv and falls back to /proc memory on EPERM or ENOSYS. It validates the notification before using the bytes. `resolve` pins the target root or directory fd, opens the parent with O_PATH, follows intermediate symlinks within the target root, and reads its canonical name through /proc/self/fd. Final-component inspection and capture use directory fds without following symlinks.

The deny floor runs first. For a regular removed file, `keep_link` adds a quarantine hard link. A failed link falls back to the regular-file copy ladder. Removed symlinks and FIFOs can be hard-linked without opening their content. Directory removals record their modes. In-place writes use `keep_copy` once per inode. Previously kept hard links are frozen before another alias can write them. Creations and their later moves or removals are tracked separately.

`Journal::append` in `src/journal.rs` writes the complete nine-field, percent-encoded effect with one write before S answers CONTINUE. S never opens SQLite. Meta is written once; a separate atomic child file records C's pid, and the CLI merges it into metadata when loading. Meta and end still use write-then-rename. Exec never calls fsync or fdatasync. It observes syscall outcomes before later calls can obscure successful renames. Before W receives C's status, S writes touched-path post-states. A detached S keeps capturing and writes final post-states and end when the entire tree exits.

For `saqta undo <id>`, the CLI imports the folder into SQLite, checks the command's lifetime and undo marker, and compares every affected path with its post-state. `src/restore.rs` opens canonical parents with openat2 RESOLVE_NO_SYMLINKS. It reverses the log: recreate parent directories, put quarantined entries back, remove creations, and reverse renames and exchanges. Before discarding a current entry, it moves it to an undo quarantine and syncs its recovery map. Shared surviving inodes get an independent post-command backup before their bytes are repaired, so untouched hard-link aliases can also recover. Only a completed restore gets an undone marker.

## Acceptance checklist

| # | Item | Result | Proven by (test name or command) |
|---|---|---|---|
| 0 | CI runs on main pushes and PRs | PASS | `.github/workflows/ci.yml` |
| 1 | Every filter rule, argument loads, architecture and x32 rejection | PASS | `filter::tests::every_syscall_rule`, `every_open_flags_combination`; existing ABI tests |
| 2 | Page-bounded path reads and forced proc fallback | PASS | `pathname_read_stops_at_nul_before_protected_page`; normal and forced capture suites |
| 3 | Canonical paths, dirfds, final symlinks, renamed cwd | PASS | `raw_directory_fd_operations_restore_exactly`, `find_delete_uses_directory_fds`, `final_symlink_write_restores_target_without_replacing_link`, `renamed_then_deleted_working_directory` |
| 4 | Hard links, copy ladder, size-only growth, copy limit, no special-file reads | PASS | `rdwr_without_truncation_captures_once_per_inode`, `legacy_open_creat_and_shrinking_truncate`, `growing_truncate_records_only_size_and_restores_mtime`, `max_copy_limit_reports_unprotected_and_keeps_deletes`, FIFO scenario; socket portion explicitly skipped here |
| 5 | Journal, private layout, id allocation, byte paths and SQLite | PASS | `concurrent_commands_allocate_unique_ids_and_log_limit`, `sqlite_index_uses_blob_paths_and_can_be_rebuilt`, existing private-home tests |
| 6.1 | rm tree, nested/empty dirs, symlinks, hard links and unusual names | PASS, byte exact | `rm_tree_restores_all_byte_paths_and_file_types` |
| 6.2 | Dirty Git tree including .git, ignored .env and 5 MiB database | PASS, byte exact | `dirty_git_checkout_and_clean_restore_entire_repository` |
| 6.3 | sed -i on 50 files | PASS, byte exact | `sed_rename_over_fifty_files` |
| 6.4 | O_TRUNC, empty files and O_RDWR without truncation | PASS, byte exact | `shell_truncation_restores_empty_and_nonempty_files`, `rdwr_without_truncation_captures_once_per_inode` |
| 6.5 | mv over a file, directory moves and RENAME_EXCHANGE | PASS, byte exact | `mv_over_file_restores_both_names`, `mv_directory_and_later_deletion`, `rename_exchange_swaps_back` |
| 6.6 | mkdir, touch, hard links, symlinks and mkfifo creations | PASS, byte exact | `creations_only_are_removed_in_reverse_order`; ephemeral-name regressions |
| 6.7 | File outside the project in decoy HOME | PASS, byte exact | `deletion_outside_project_in_decoy_home` |
| 6.8 | Decoy HOME wipe with store inside HOME | PASS, byte exact excluding the store | `decoy_home_wipe_preserves_saqta_and_restores_everything_else` |
| 6.9 | find -delete and xargs -P 8 | PASS, byte exact | `find_delete_uses_directory_fds`, `eight_parallel_removers_share_one_journal` |
| 6.10 | Working directory renamed and removed during command | PASS, byte exact | `renamed_then_deleted_working_directory` |
| 6.11 | Settings and store deny floor | PASS | `agent_settings_and_store_writes_are_blocked`, `settings_symlink_to_plain_file_still_blocks_write` |
| 6.12 | Mounted tmpfs exclusion | SKIP on this machine | `tmpfs_mount_is_skipped`: mount requires privileges unavailable in this sandbox |
| 6.13 | openat2 and io_uring_setup return ENOSYS | PASS | `unsupported_interfaces_return_enosys`; bare probes returned 0 and EPERM respectively |
| 6.14 | Conflicts name changed paths and change nothing | PASS | `undo_conflict_lists_path_and_changes_nothing`, `symlink_parent_substitution_refuses_undo` |
| 6.15 | Second undo refuses | PASS | `second_undo_is_refused` |
| 6.16 | SIGKILL S during 3,000-file removal, incomplete import, recovery | PASS, byte exact | `killed_supervisor_does_not_hang_and_imports_incomplete` |
| 6.17 | Capture suite repeated with SAQTA_FORCE_PROC_MEM=1 | PASS, 45 scenarios | CLI command below; CI and review bundle run it too |
| 6.18 | setsid process deletes after W returns | PASS, byte exact | `setsid_background_deletions_remain_in_same_command` |
| 7 | Release overhead script, 20,000 operations and clean build | PASS | `REPS=1 scripts/overhead.sh`; table below |
| 8 | Three decision records under 300 words; report and commit plan | PASS | `wc -w docs/decisions/000{4,5,6}*`; this report |
| M1 | All M1 scenarios still pass in parallel | PASS | `cargo test --locked` |
| R1.1 | No exec-path fsync/fdatasync; undo syncs retained | PASS | Call-site audit of `json_write`, `synced_json_write`, Journal startup/snapshot/finish and unchanged `restore.rs` |
| R1.2 | Write meta once; separate child record still supports recovery | PASS | Both measurement batches checked all meta/child/end files; existing killed-supervisor and lifetime tests |
| R1.3 | Release added median below 3 ms, 200 runs per command | NOT MET | 3.263 ms initially, 3.239 ms on repeat inside the sandbox; details below |
| R1.4 | Debug phase breakdown and code-derived syscall inventory | PASS | Seven timestamp pairs in each of 200 debug logs per batch; inventory below |

Validation completed before packaging:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`: 85 tests, including 45 capture tests; conditional skips are printed.
- `SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture`: 45 tests.
- `cargo deny --offline check`: advisories, bans, licenses and sources pass.
- `bash -n scripts/overhead.sh scripts/review-bundle.sh`
- `git --no-optional-locks diff --check`

The `m2-capture-r1` bundle repeats the required checks, including the forced proc-memory suite, and records their complete output. Its doctor invocation uses an isolated home under target/saqta-tests. R1 changes only exec sync policy, the separate child record and its loader, debug phase timestamps, ADR 0005, this report, and the bundle label match needed to retain the forced check.

## Decisions

- [0004-capture](../docs/decisions/0004-capture.md): pre-images, copy ordering, filesystem exclusions and the fresh supervisor runtime.
- [0005-journal](../docs/decisions/0005-journal.md): write-ahead command folders, observed outcomes, final snapshots and a rebuildable SQLite index.
- [0006-undo-safety](../docs/decisions/0006-undo-safety.md): preflight conflicts, safe directory walks and retained undo recovery data.

Direct dependencies added are the allowed serde derive support and rusqlite 0.43 with bundled SQLite. The initial rusqlite selection failed the local SQLite advisory check and was replaced. No advisory exceptions were added. `deny.toml` now checks the two supported x86_64 Linux targets, including musl, so unbuilt wasm bindings do not introduce an irrelevant second syn major version into the shipped graph.

## Measurements

Machine: AMD Ryzen 9 9900X, 24 logical CPUs visible. WSL2, x86_64, ext4, kernel `6.18.58.1-microsoft-standard-WSL2`. Rust stable 1.100.0. All measurements, including R1, were taken inside the Codex sandbox.

The following 20,000-operation table is the original M2 measurement before R1. Capture handling has not been optimized or remeasured. These are elapsed times from one fresh run per variant, not a statistical performance claim. The script defaults to three repetitions.

| Workload | Bare ms | saqta no-sync ms | saqta sync ms |
|---|---:|---:|---:|
| 20,000 deletes | 243.1 | 4589.5 | 3433.4 |
| 20,000 creates | 939.3 | 7054.5 | 6085.8 |
| 20,000 rename-overs | 513.8 | 7768.0 | 6587.1 |
| 20,000 read-only stats | 42.3 | 71.3 | 65.3 |
| Git status, 20,000 indexed files | 32.7 | 95.9 | 54.5 |
| Clean cargo build of this crate | 5987.4 | 6415.0 | 6457.8 |

The original M2 debug-build timing test measured direct median/p95 3.66/4.16 ms and supervised median/p95 25.36/35.79 ms. After R1, that same unchanged test measured 3.26/3.73 ms direct and 6.58/6.75 ms supervised. Its polling test harness and debug build make it separate from the blocking-wait release measurements below. The R1 capture suite took 0.96 s and the M1 exec binary took 2.03 s. Supervisor SIGKILL to wrapper return measured 12.16 ms. All fixtures and measurement state paths remained under target/saqta-tests.

### R1 release command latency

Build: `cargo build --release --locked`. A Python driver used `subprocess.run` with a blocking wait, identical isolated HOME/XDG/SAQTA_HOME values and cwd, and alternating invocations of `true` and `target/release/saqta exec -- true`. Each batch has exactly 200 runs of each. Timings include process creation and wait; DEBUG was unset. There were no warm-up runs or discarded samples. p95 is the nearest-rank 190th sorted sample.

| Batch | Command | Runs | Median ms | p95 ms |
|---|---|---:|---:|---:|
| Initial | `true` | 200 | 0.610 | 0.737 |
| Initial | `saqta exec -- true` | 200 | 3.873 | 4.552 |
| Repeat | `true` | 200 | 0.609 | 0.759 |
| Repeat | `saqta exec -- true` | 200 | 3.848 | 4.409 |

Added median latency was **3.263 ms** initially and **3.239 ms** on the repeat. The under-3-ms added-median target is **not met** in this sandbox, by about 0.24 to 0.26 ms. The repeat confirmed that the miss was consistent. No further startup or capture optimizations were made because this review limits the change to sync removal, one-time meta writing and instrumentation.

Local raw measurements and the driver are retained at `target/m2-r1-latency-first.json`, `target/m2-r1-latency.json` and `target/m2-r1-latency.py`. Both fresh fixtures contain 400 successful command folders: 200 timing runs and 200 subsequent debug runs. Each meta still contains child=0, its separate child file contains a positive pid, and end exists. This directly checks that meta was not rewritten for the child.

### R1 supervisor phases from SAQTA_DEBUG timestamps

Each latency batch was followed by a separate set of 200 `SAQTA_DEBUG=1` runs. Every debug file contains all seven begin/end pairs as `<phase><TAB><CLOCK_MONOTONIC nanoseconds>`. Timestamp formatting uses stack storage and is safe before re-exec. Logging is skipped when DEBUG is unset. The following table uses all 200 traces from the repeat batch. It includes timestamp/log-writing overhead, so it is not the uninstrumented command-latency table.

| Phase | Median microseconds | p95 microseconds | Boundary |
|---|---:|---:|---|
| Re-exec | 570.4 | 675.0 | Fork child immediately before exec to the new supervisor parsing its debug fd; includes loader and argv setup. |
| Id allocation | 49.1 | 58.0 | Open, lock, read, update and truncate next-id, then unlock. No sync. |
| Folder creation | 836.4 | 1172.0 | Create cmd/id and q/main/id. |
| Meta | 92.3 | 123.6 | Build and write meta to a temporary file, then rename it once. |
| Listener handoff | 285.5 | 335.0 | Subreaper/group/signal setup, fork C, detach streams, write run marker and child record, receive listener and set sync wake flag. |
| Snapshot | 46.5 | 87.3 | Open post.log and append the current touched-path states. For true there are no touched paths. |
| End | 82.8 | 129.8 | Build/write the temporary end record and rename it. |
| Other startup | 254.5 | 302.8 | Unattributed gaps: command/filter/resource preparation, home preparation, log opens/closes and Capture initialization. |
| Startup total | 2120.1 | 2561.6 | Re-exec begin through handoff end. |
| Finish total | 134.5 | 201.4 | Snapshot begin through end end. |

The first debug batch measured startup median/p95 2219.1/2912.4 microseconds and finish 132.9/201.5 microseconds. Instrumented end-to-end median/p95 was 4.356/5.294 ms initially and 4.096/4.701 ms on repeat. Phase medians need not sum to total medians. W preparation, its fork, command execution/wait, reporting and teardown are outside S's startup/finish boundaries.

### R1 syscall inventory for the next milestone

This is a code-derived inventory, not a strace result. No capture optimizations were made. It follows `supervisor::serve`, `Capture::handle/read_call/resolve/capture`, `sys::read_path`, `keep_link` and `Journal::append`. Kernel syscall names are used below; libc's open64 and fstat64 entry points correspond to openat and fstat on this x86_64 glibc system. The linked binary imports readlink, open64, openat, pread64, fstat64 and fstatat, consistent with those library calls.

Assumptions: C is alive; no wrapper signal, deadline, SIGCHLD or stale notification; DEBUG is off; no pending earlier effect at entry; a short NUL-terminated path fits in one memory page and proc readlink buffers; the target filesystem is ext4; the existing victim is a regular pre-command file; its link into q/main succeeds; no intermediate/final symlink traversal is needed. For the creation case the final path is absent and flags are exactly O_WRONLY|O_CREAT. The configured root/cwd or directory fd and command quarantine are already initialized.

| Order | Supervisor syscall and purpose | Existing-file unlinkat | New-file openat | Repeated work / cache considerations |
|---|---|---:|---:|---|
| 1 | poll(listener, signalfd, W socket) waits for readiness | 1 | 1 | Event-loop envelope, outside Capture::handle. |
| 2 | read(signalfd) drains notifications, normally EAGAIN here | 1 | 1 | Called even when this poll did not report a signal event. |
| 3 | wait4(-1, WNOHANG) checks/reaps children, normally 0 here | 1 | 1 | Called on each non-HUP loop iteration. |
| 4 | ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV) receives the request | 1 | 1 | Required notification protocol. |
| 5 | process_vm_readv(pid, pathname page) | 1 | 1 | Returns EPERM in this sandbox. Its unavailability is rediscovered per pathname. |
| 6 | openat(AT_FDCWD, /proc/pid/mem, O_RDONLY\|O_CLOEXEC) | 1 | 1 | Reopened per pathname; a per-process fd cache would need exec/lifetime invalidation. |
| 7 | pread64(memfd, pathname address, page-bounded length) | 1 | 1 | Path contents must be reread for each request. |
| 8 | close(memfd) when read_path returns | 1 | 1 | Paired with the repeated proc-memory open. |
| 9 | ioctl(listener, SECCOMP_IOCTL_NOTIF_ID_VALID) after memory reads | 1 | 1 | Required validity check; not a cache candidate. |
| 10 | openat(AT_FDCWD, /proc/pid/cwd or /proc/pid/fd/D, O_PATH\|O_DIRECTORY\|O_CLOEXEC); /proc/pid/root for an absolute path | 1 | 1 | Reopens the process base. Cwd, fd reuse and root/mount changes constrain caching. |
| 11 | openat(AT_FDCWD, /proc/pid/root, O_PATH\|O_DIRECTORY\|O_CLOEXEC) inside resolve_from | 1 | 1 | Reopens the root; duplicates the base open for absolute paths. |
| 12 | readlink(/proc/self/fd/rootfd) | 1 | 1 | Rediscovers the root's display path per resolution. |
| 13 | readlink(/proc/self/fd/basefd) | 1 | 1 | Rediscovers the cwd/dirfd display path; renames can change it. |
| 14 | openat2(rootfd, root-relative parent, O_PATH\|O_DIRECTORY\|O_CLOEXEC, RESOLVE_IN_ROOT) | 1 | 1 | Opens/resolves the parent on every request, even for siblings. |
| 15 | newfstatat(parentfd, last, AT_SYMLINK_NOFOLLOW) | 1 | 1 | Existing-victim metadata or ENOENT. Cannot blindly cache across writes, path reuse or concurrent changes. |
| 16 | readlink(/proc/self/fd/parentfd) | 1 | 1 | Rebuilds the canonical parent path; directory renames require invalidation. |
| 17 | fstat(parentfd) checks st_nlink for a deleted parent | 1 | 1 | Repeated parent metadata also contains the device id useful for future filesystem caching. |
| 18 | close(rootfd), close(basefd) as resolution returns | 2 | 2 | Paired with the repeated process-directory opens. |
| 19 | ioctl(listener, SECCOMP_IOCTL_NOTIF_ID_VALID) after resolution | 1 | 1 | Required validity check before acting. |
| 20 | openat(parentfd, last, O_PATH\|O_NOFOLLOW\|O_CLOEXEC) for filesystem classification | 1 | 0 | Only for an existing final component; the new path uses its parent fd. |
| 21 | fstatfs(targetfd or parentfd) tests the filesystem exclusion list | 1 | 1 | Filesystem type is rediscovered per request. A cache must distinguish filesystems/mounts and handle mount changes. |
| 22 | close(target classification fd) | 1 | 0 | No content is opened or read. |
| 23 | linkat(parentfd, last, qfd, quarantine-name, 0) keeps the victim | 1 | 0 | Necessary pre-image work. The qfd is already cached per command. |
| 24 | write(effects.log, one complete percent-encoded line) | 1 | 1 | Required before CONTINUE; neither buffered nor synced. |
| 25 | ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND, CONTINUE) | 1 | 1 | Required notification protocol. |
| 26 | close(parentfd) when the resolved request leaves scope | 1 | 1 | May occur after C resumes. |
| | **Total for the stated successful path, including loop envelope** | **27** | **24** | **Capture/notification handling alone is 24 / 21 calls, excluding poll, signalfd read and wait4.** |

`journal::now()` also calls CLOCK_REALTIME twice for a kept unlink (effect time and quarantine name), once for a creation. On this platform clock_gettime normally uses the vDSO, so these are library calls without a kernel transition; add two or one syscalls if the clock must fall back to the kernel. DEBUG adds no per-effect timestamps. The deny-floor comparisons, seen-inode/creation sets, percent encoding and log formatting have no explicit syscalls. A new file gets no content capture or content-open syscall in S; its actual open/write is C's work.

The previous effect's outcome is normally charged to the next request by the same pid, or to task completion. For either of these operations, `complete` does one newfstatat of the canonical path and one write to outcomes.log. The steady-state cost is therefore **29 calls per deletion and 26 per creation** under the same assumptions, counting the loop envelope and that deferred outcome. These two calls are not removed by dropping fsyncs.

Other pending pids add variable work. `observed` calls `applied` for each pending record, statting its first path and a second path for a rename/exchange. If that check is false, kill(pid, 0) checks existence. When an outcome is observed, `complete` repeats those state reads and writes one outcome line for each associated effect. This can repeat the same path and process checks across notifications. Caching must preserve the rename-followed-by-removal behavior already tested.

With SAQTA_FORCE_PROC_MEM=1, omit process_vm_readv: the stated totals drop to 26 / 23, or 28 / 25 including deferred outcome. If process_vm_readv succeeds normally, omit the mem open, pread and close: 24 / 21, or 26 / 23 with deferred outcome. Extra pathname pages add another memory read per page. Long proc display paths can require repeated readlink calls; following a final symlink adds readlink of that entry and another resolve_from pass, plus a root open for an absolute target. EINTR, stale requests, filesystem errors, deny-floor matches and hard-link/copy fallbacks take different branches from these two successful cases.

These counts describe explicit kernel-facing calls, not an allocator trace. Vec/String/map growth can additionally trigger allocator-dependent brk or mmap/munmap calls, and libc/Rust implementation fallbacks can change the kernel entry point. No fixed count is asserted for those conditional runtime calls. Startup, final post-state snapshots, end writing and SQLite CLI import are outside this per-notification inventory. No strace was used.


## Known gaps and risks

- Two hard links to one inode modified in place can return as separate files. Surviving shared inodes are repaired, but cross-filesystem content copies cannot discover other aliases. Link counts are excluded from the tree hash as requested.
- Capture uses CONTINUE and cannot eliminate races with unrelated or hostile processes. Post-state conflict checks compare type, device, inode, size, mode and mtime, not content hashes.
- The requested append exception and skipped filesystem types intentionally leave those operations unprotected. Existing write descriptors, metadata-only syscalls and other calls outside the requested filter remain outside this milestone's interception boundary.
- Completed writes protect against supervisor death, not arbitrary power loss. R1 removes all exec-path syncs, including snapshots and end. Undo keeps its recovery-map, restored-file and undone-marker syncs. A damaged journal or partial undo requires manual recovery; the pre-images, recovery map and displaced current entries are retained.
- Missing post-states after supervisor death are frozen at the first recovery import after known tasks exit. Changes made before that observation cannot be distinguished from the command's own effects. Displaced current bytes are still retained by undo.
- This sandbox cannot mount tmpfs or bind a Unix socket. Both limitations print explicit SKIP reasons. FIFO capture ran and passed. The existing M1 i386 probe also prints SKIP because the outer sandbox terminates its bare probe with SIGSYS.
- Actual EXDEV and protected_hardlinks failures, and the final stream-copy fallback on a filesystem without copy_file_range, were not forced on this single ext4 filesystem. Their failure paths preserve existing entries and record errors. No per-filesystem quarantine is implemented.
- The benchmark covers the full 20,000-operation workloads, but only one repetition was run here. Capture costs are substantial; no M2 performance threshold was supplied.

## Questions for review

- Review the observed-outcome rules, especially directory renames followed by later removals and supervisor death between intent and CONTINUE.
- Review the shared-inode undo path and recovery-map ordering. Every discarded post-command version must remain recoverable.
- Exercise privileged tmpfs mounting and actual cross-filesystem fallback on a machine that supports them.

## Commit plan

After review passes, split the work into these eight logical commits. Group each helper with its first caller and validate each staged tree before committing.

1. `ci: run main pushes and pull requests once`.
2. `filter: intercept destructive calls and write-capable opens`, including the argument-aware BPF interpreter.
3. `capture: re-exec the supervisor and write command journals`, including memory reads, safe syscall wrappers, log/show and initial capture dispatch.
4. `restore: check post-states and reverse command effects`, including undo CLI and retained recovery data.
5. `capture: preserve inode versions across aliases and renames`, including outcome observation, creation-name reuse, shared-inode restoration and regression checks.
6. `test: verify byte-exact capture and crash recovery`, including Sandbox tree hashes, raw syscall scenarios and the forced proc-memory CI run.
7. `perf: measure release capture workloads in isolated fixtures`.
8. `docs: explain M2 capture, journals and undo safety`, including dependency target checks, README, this report and review packaging updates.

Review passed. The eight planned commits were authorized and created. Nothing was pushed.
