# Validate test builds and synchronize fault injection

## Context

R1's standalone commit validation built a snapshot into the branch's Cargo
target directory. Its executable replaced the branch's executable. Cargo later
reported a fresh build while running that older binary, which lacked reply
observations. Namespace and process-memory differences did not cause the failure.
Short fault deadlines also raced startup and a fast one-file resume.

## Decision

Give each snapshot or worktree its own Cargo target directory. Review bundles
build in their checkout's `target/review-build`. Before creating test workloads,
validate the executable's compiled manifest directory and observation protocol,
once per test binary. The existing debug harness exposes this static information
without opening a fixture or store. Reject foreign or older executables with
explicit rebuild instructions. Do not silently accept missing observations.

Allow ten seconds per readiness condition, then start the existing 350 ms fault
window. Reports retain total elapsed time and separate setup and fault times.
Require successful replies before stopping a supervisor. Gate the tracee on
stdin, observe the supervisor stopped, release stdin, then observe seccomp wait.
A debug-only undo hook stops itself before any mutation, removing the race
between parent SIGSTOP and a completed resume. Wait for wrapper exit before
testing orphan pipe holders. Keep all existing diagnostic and restore assertions.

Report observation-file read errors with their actual path and errno instead
of guessing that logging was disabled.

## Alternatives considered

Increasing the 350 ms deadline would not fix stale executables or establish the
required fault state. Automatically rebuilding from test helpers would obscure
which code Cargo tested and add recursive build work.

## Consequences

Review builds use an additional cache. Provenance failure needs an explicit
rebuild. Metadata and pause hooks exist only in debug builds. These hooks do not
change capture or release behavior.
