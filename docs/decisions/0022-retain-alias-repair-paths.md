# Retain alias repair paths and record their parent changes

## Context

A command can link a pre-existing inode under a new name, remove its original
name, write through the new link, then remove it too. Another pre-existing
link can survive outside the effects. Capture has the old bytes, but undo
needs a name that still reaches the inode. Across commands, undo can also
repair an alias discovered in prior observations. Moving that alias out and
back changes a directory that the current effect log may never mention.
Repair can also leave that alias's saved lineage out of date for older undo.

## Decision

Keep a pre-image for a removed command-created name if the inode still has
multiple links. Use the existing hard-link capture and freeze rules. Reverse
deletion first, repair through that name, then reverse its creation. Keep the
single-link shortcut for names created by the command.

When planning repair of a discovered alias outside the private undo folder,
record the alias lineage and the parent's identity in the recovery frame.
Carry those names
through subsequent reverse renames and exchanges. Drop them when undo removes
their created subtree. After all actions complete, publish the final directory
state and alias lineage only if their recorded identities still match.
The existing conflict and lineage checks still require current state to
match the observation.

## Alternatives considered

Scanning the tree for every alias costs time and misses links outside that
tree. Ignoring directory timestamp differences could accept newer user work.

## Consequences

Deleting a newly created shared link can now retain its inode in quarantine.
Ordinary unshared creates still record metadata only. The additional recovery
fields default to empty in existing checkpoints. Resume reuses the saved
identities and keeps the original recovery and conflict rules.
