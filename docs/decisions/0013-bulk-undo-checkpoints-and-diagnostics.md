# Bulk undo checkpoints and timeout evidence

## Context

The 5,000-file bind-mount test exceeded ten seconds
on Ubuntu 22.04 CI. The helper reported a pid. Two-CPU measurements with
synchronous wakeups disabled reproduced an undo slowdown, with no hang.
M4 wrote a plan and a completion checkpoint for each deleted file.

## Decision

Checkpoint the previous effect's completion together with the next effect's
plan, before the next mutation. Flush the final completion before marking undo
finished. Keep intermediate action checkpoints, move identities and the synced
displacement map. A process killed between effects replays the existing plan;
its identities recognize the completed mutation. This refines decision 0009
without changing its process-crash guarantee.

Give only the bulk deletion test an explicit sixty-second deadline for deletion
and undo. Keep the ordinary ten-second deadline and byte-exact tree assertions.

In test fixtures enable existing debug logging. Debug supervisors overwrite one
forty-byte observation record before and after handling a notification. Count
successful response ioctls separately from received notifications. Observation
errors never change capture or syscall decisions. Without debug logging, no
observation file is opened or written.

Give helper commands separate process groups to isolate job-control signals.
On timeout, sample process and recovery progress one second apart, print bounded
journal tails, then stop and kill the owned tree. Match orphaned pipe holders
and validate process start times before signalling. Bound reaping and keep a
private diagnostic artifact in the fixture.

## Alternatives considered

A larger global timeout would hide small-test hangs. Journal size alone cannot
prove that the supervisor answered the kernel. Batching multiple unplanned
mutations would need a broader recovery design.

## Consequences

Bulk undo still pays for per-effect recovery plans and identity checks. Further
batching belongs in the performance milestone. A silent observation window is
reported as observed inactivity, not proof of a permanent deadlock.
