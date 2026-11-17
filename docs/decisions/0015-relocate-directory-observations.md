# Preserve directory observations through reverse moves

## Context

R2's final full-suite run found a saved-seed failure unrelated to timeout
observations. Undoing nested directory exchanges restored the tree, but the next
older undo refused a directory whose mtime had changed during that restoration.
Directory observations were saved under effect paths rather than final names.
An observation from a previous undo also stayed behind when its ancestor moved.

## Decision

Apply the same reverse path mapping to directory observations and restored inode
origins. Successful renames and exchanges relocate their descendants. Reversing a
creation drops observations for that created subtree. Carry existing observation
values unchanged, so relocation cannot bless later user edits. Add current
directory observations at final restored names and their parents.

Before mutations, save the input observation map once in the undo folder as
`directory-observations.json`. Resume always derives the final map from this
input. This makes publication idempotent if interruption occurs after writing
the home observations but before marking undo complete. Older interrupted undos
without this file snapshot the existing input before continuing.

Keep the strict conflict checks. Add exact-tree regressions for both two-command
and three-command forms, a debug abort after observation publication, and a
negative case proving later user edits still cause refusal. Preserve the
proptest seed.

## Alternatives considered

Ignoring directory post-state differences could accept unrelated edits. Recording
only original effect paths misses directories moved by their ancestors. Reusing
the published map as resume input applies exchanges twice.

## Consequences

Each undo saves one additional synced metadata file. Per-effect checkpoint counts
are unchanged. This retains the process-crash contract of decision 0009 without
claiming power-loss durability.
