# A per-command effect log with a rebuildable index

## Context
S must acknowledge a destructive call only after its pre-image and intent exist. SQLite work and locks do not belong in the notification loop.

## Decision
Allocate ids from next-id under flock, waiting at most five seconds. Never reuse command or quarantine directories. Create private cmd, q/main, run and logs. Files use mode 0600; directories use 0700. Preserve the existing home's permissions as in M1.

Write meta once. Record C's pid in a separate child file, which the CLI merges into the metadata on read. Meta, child and end use write-then-rename for atomic replacement.

S writes effects.log with one write per line before CONTINUE. Fields: sequence, time, pid, operation, both paths, quarantine name, flags and detail. Percent encoding preserves pathname bytes. Detail includes original metadata and capture method.

outcomes.log records success inferred from inode identities at the next notification or task exit. Observe successful renames before later calls obscure their destinations. Keep failed syscall intents.

Before reporting C's exit, append touched-path states to post.log. Append final states and end when the tree exits. Run markers identify live supervisors. Missing end with a missing or dead supervisor means incomplete. CLI recovery freezes missing post-states after known tasks exit; these are recovery observations.

Only the CLI imports folders into journal.db: WAL, five-second busy timeout, schema version 1, BLOB paths. The index is rebuildable.

## Alternatives considered
SQLite in S adds latency. Per-command fsyncs purchase power-loss durability outside this milestone's promise.

## Consequences
Completed writes survive a supervisor crash. Exec performs no fsync or fdatasync, including startup and finish. Power-loss durability is not promised. Undo retains its syncs. Damaged journals refuse automatic undo.
