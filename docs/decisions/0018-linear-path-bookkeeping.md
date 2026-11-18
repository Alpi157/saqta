# Bound path bookkeeping and retain shared created aliases

## Context

Rescue exposed whole-set scans for every deletion and reverse creation.
The extended harness also found that a newly created alias could share an
unrecorded surviving pre-command name after its recorded origin was deleted.
M4 restored the deleted name but left that surviving inode changed.

## Decision

Use ordered subtree ranges for creation invalidation and undo path mapping.
Capture creation keys compare raw pathname bytes; subtree ranges start with the
directory separator so neighboring names such as `a-old` cannot hide `a/file`.
Use a hash set for touched paths, whose snapshot order has no semantic meaning.
Avoid redundant settings checks and duplicate requested paths when a simple
filename already has its canonical name.

For a created name with multiple links, capture its write even when its recorded
origin no longer exists. Undo repairs the shared inode before dropping the new
name. Single-link creations retain the existing optimization. Keep the harness
seed and add an interrupted-undo regression. The defect is reproducible with
the untouched M4 release binary.

Buffer only post-state serialization, flushing before reporting a snapshot or
publishing end. Missing post-states after supervisor death still use the existing
qualified recovery observations. Pre-images and complete effect lines remain
unbuffered and precede CONTINUE. Decision 0019 batches only positive outcomes.
This preserves decisions 0005 and 0012, including unacknowledged effect tails.

## Alternatives considered

Scanning all remembered paths is simple but grows quadratically. Assuming every
created name owns a new inode misses hard-link aliases. Delaying outcome writes
past dependent CONTINUE replies would need another recovery protocol.

## Consequences

Post-state line order can change. Shared created aliases may retain extra copies.
No new durability or filesystem-race guarantees are claimed.
