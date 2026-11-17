# Resume undo from explicit steps

## Context

Decision 0006 retained displaced entries but required manual recovery after a
partial undo. Replaying an exchange twice reverses recovery. Repeated writes and missing
hard-link names also complicate reversal.

## Decision

Persist each effect's action plan before changing names. Atomic checkpoints hold
the effect, action cursor, inode identities and completed reverse prefix. Resume
checks whether a move or exchange already happened before repeating it. Copies
publish from staging entries with a separate publication record. Partial copies
are retained before retrying.

Keep the synced recovery map before displacing current entries. Checkpoints use
atomic replacement without a sync per action. This extends the existing process
crash guarantee; power loss still requires manual review. A completed undo marker
can finish its remaining cleanup on resume. Older partial undos without an M4
plan still require manual recovery.

Preserve regular-file identity through temporary alias absence. Keep immutable
snapshots beside mutable inode handles. Before the first mutation, materialize original
prefixes from size-only growth records. Follow later moves to find these prefixes. Repair recorded surviving aliases when reversing growth. Retain alias lineage
across commands, including inode handles in undo folders.

Post-state snapshots never traverse symlink parents. An inaccessible child is
absent only when the recorded ancestor still matches. Successful undo records
directory observations so undoing an earlier command accepts directory changes
caused by that undo, while later user changes still conflict.

## Alternatives considered

A completion counter alone cannot distinguish a crash after a rename. Copying a
whole project before undo would duplicate unrelated data and require tree scans.

## Consequences

Recovery needs extra small files. The debug abort hook exercises the interval
between a mutation and its completion checkpoint. Cross-filesystem copies of
nonempty directories still refuse and retain the entries.
