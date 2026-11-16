# Retry transient parent resolution without relaxing restrictions

## Context

An unrestricted M2b review run exposed EAGAIN in the existing renamed-cwd fixture, whose paths contain `..`. openat2 documents this transient error when concurrent renames prevent proving that resolution stays inside the pinned root.

## Decision

Share the parent-opening syscall wrapper between capture and undo. Retry only EAGAIN, up to eight attempts total. Keep the same root descriptor, pathname and resolution flags on every attempt. Preserve CLOEXEC and all notification validity checks. Return the final error normally when retries are exhausted.

## Alternatives considered

Ignoring the failure leaves an avoidable capture gap. Unbounded retries could hang the supervisor. Dropping resolution restrictions could redirect capture or restoration.

## Consequences

Successful resolution usually still takes one syscall. A persistent race remains an explicit error. No journal format or path policy changes.
