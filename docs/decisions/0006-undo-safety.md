# Check every conflict before restoring

## Context
Undo must preserve newer user work. A replaced directory or symlink must not redirect restoration elsewhere.

## Decision
Refuse supervised undo, live command trees, repeated undo and damaged journals. Hold a command-specific nonblocking undo lock. Compare every changed path against its latest post-state before changing anything. Blocked calls changed no paths and do not participate in conflicts. Check saved pre-image metadata as well.

Open canonical parents from a pinned root with openat2 RESOLVE_NO_SYMLINKS and RESOLVE_BENEATH. All final-component operations use directory fds and never follow symlinks. A missing parent is acceptable during conflict checking only when the recorded path is absent. During restoration its directory must already have been recreated by an earlier reverse effect.

Reverse effects in sequence order: recreate removed directories, restore kept files, remove creations, shrink extensions, reverse renames, and swap exchanges back. Consume a quarantine name by rename on the same filesystem. Copy across filesystems or when earlier effects still need that pre-image.

Before discarding a current entry, move it to a private undo quarantine. Sync its percent-encoded recovery map first. Across filesystems, copy before unlinking; refuse nonempty directory or special-file copies. Reverse renames and exchanges also get recovery instructions. An undo-in-progress marker makes partial failure require manual review. Mark undone only after the protected effects finish. Report unprotected effects explicitly.

## Alternatives considered
Blind replacement loses newer work. Following intermediate symlinks can restore into an unrelated tree. Keep-both conflict handling belongs to a later milestone.

## Consequences
Two hard links modified in place may return as separate files. Surviving shared inodes are repaired after retaining independent backups. Incomplete-command post-states cannot detect changes made before recovery observation. All entries displaced by undo remain available for manual recovery. Power loss during undo requires reviewing its recovery map.
