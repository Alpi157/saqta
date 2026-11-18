# Cache task context behind explicit invalidation

## Context

Opening proc memory, root and cwd for every pathname dominates short calls.
A readable old memory descriptor can survive exec when another task holds that
mm. PID numbers and root display names alone are not identities.

## Decision

Notify on execve, execveat, chdir, fchdir, chroot, pivot_root, setns, unshare,
mount, umount2 and move_mount. Before CONTINUE, discard the single task cache.
Only that caller's next notification can repopulate it. Other tids use temporary
context. Thus another thread cannot cache an old mm or shared fs context while
the invalidating call is still running. Failed context calls invalidate too.

Use a pinned pidfd for liveness, falling back to start-time reads through a
pinned `/proc/tid/stat` descriptor. These handles reference the original task;
PID reuse cannot redirect the check. Discard on identity failure, reap, idle exit checks,
or command end. Reopen memory once after a failed proc read. Remember VM-read
EPERM or ENOSYS for the supervisor. Reads remain page bounded and both notification
validity checks remain. Page bytes are shared only within one notification.

Keep the root descriptor. Refresh its display path unless it is the immutable
filesystem root `/`. Refresh cached cwd display paths on every notification.
Open numbered dirfds afresh. Within one notification, share the pinned base for
simple filenames. Complex paths retain the restricted root walk and symlink rules.

Cache classification by pinned mount identity (decision 0021). Release
classification and observation pins before mount-context calls, so they cannot
keep mounts busy. New mounts use their actual identities.

## Alternatives considered

Read failure alone misses live old mms. Permanent caches for every tid need more
in-flight context tracking. Caching numbered dirfds misses descriptor reuse.

## Consequences

Parallel
tasks may lose cache hits. All descriptors remain CLOEXEC.
