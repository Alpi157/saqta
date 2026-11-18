# Apply automatic holds with the undo engine

## Context

Automatic restoration must preserve both versions and keep M5 recovery
guarantees. Ordinary edits and Git operations often replace files. Agents
can allocate PTYs and container seccomp filters also apply to people.

## Decision

Keep one initial Git index per repository. Use its size and mtime to skip
clean files. Batch uncertain comparisons against Git near a breaker limit.
Count unrecoverable deletions and precious or outside-project overwrites.
Stop and kill the tree on a trip. At command end, use the retained and
current blobs to distinguish dirty deletions and Git reversions from edits.
Honor copies in the index, HEAD, and stashes without running Git filters.

Save the selected effect numbers, checked post-states, and independent
agent versions in `holds/ID/`. Run existing undo steps and checkpoints on
that selection. Keep interrupted Git's observed locks in recovery.
Finish the hold before reporting. Release uses the same
engine and creates a normal inverse journal. Session IDs track creations
and route background notices. A foreground hold can rotate a live journal.

Release walks PPids through `/proc`. A Saqta executable with
`__supervisor` in argv[1] or an unreadable ancestor blocks release. The
subreaper remains an ancestor of detached jobs. Release also checks the
supervised environment marker and stdin. The protected devcontainer
setting relaxes only the stdin check.

## Alternatives considered

Counting every replacement interrupts clean branch switches and formatters.
One Git process per path scales poorly. Whole-command rollback would
reverse unrelated work. Direct writes from quarantine would bypass
conflict checks, alias repair, and resume checkpoints.

## Consequences

Independent copies can grow the store. Manual undo keeps its Git index
conflict exception. Automatic restore and release require matching
identities. Recovery covers process interruption.
