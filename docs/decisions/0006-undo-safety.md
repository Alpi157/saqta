# Check every conflict before restoring

## Context
Undo must preserve newer work and finish independently of its output consumer.

## Decision
Refuse supervised undo, live trees, repeated undo and damaged journals. Hold a nonblocking command lock. Check post-states and pre-image metadata before restoring. Ignore blocked calls in conflict checks.

Open canonical parents from a pinned root with openat2 RESOLVE_NO_SYMLINKS and RESOLVE_BENEATH. Final operations use directory fds. Accept missing parents only for absent post-states. Recreate parents before children.

Changed `.git/index` and `.git/worktrees/*/index` never block undo. Restore the pre-command index matching the working tree. Keep the newer index and print its location, explaining that git status rewrites it. Other index paths get no exception.

Other conflicts refuse before restoration, listing ten paths and the remainder. Offer --force, retaining newer entries before replacement. Dry-run explains conflicts and retention. Name exclusive undo folders from command id/start time. Force cannot bypass safety checks. M2 journals stay compatible.

Reverse effects newest first. Rename pre-images back; copy when reused or across filesystems. Sync the recovery map before displacing entries. Refuse nonempty directory and special-file copies across filesystems. Retain conflicting move inputs before making working copies. Record reverse moves. Keep an in-progress marker until completion; partial failure requires manual review.

Undo ignores SIGPIPE. Output errors disable further printing without stopping restoration. Future file-changing commands, including release and gc, follow this rule. Read-only commands use default SIGPIPE. Default summaries show category counts, elapsed time, ten example paths at most, every unprotected item and recovery locations. --verbose lists effects. Dry-run uses the same format.

## Alternatives considered
Blind replacement loses work. Blocking Git refreshes prevents rescue. Output-driven termination leaves partial restores.

## Consequences
Hard links may separate. Back up shared inodes before repair. Recovery observations cannot detect earlier edits. Power loss requires reviewing the recovery map.
