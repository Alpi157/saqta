# Write one notification's effect lines together

## Context

A rename-over records a destination pre-image and a reverse rename. Both belong
to one stopped syscall, but each effect previously needed its own write.

## Decision

Build all effect lines for one notification after retaining its pre-images.
Append those lines with one write before CONTINUE. Ordinary single-effect calls
keep their existing path. Never combine notifications or buffer intent past a
reply. Line encoding, sequence order and outcomes stay unchanged.

A killed group write may leave complete lines followed by a partial line.
Decision 0012 already qualifies and preserves such a tail. No part of the group
authorized CONTINUE until the whole write finished. The complete prefix remains
safe to reverse: its pre-images exist, and existing undo retains any displaced
current entries before restoring. This is the same interval that previously
existed between writing the first and second lines. A regression models that
interval for a rename-over and verifies both original names byte for byte.

## Alternatives considered

Buffering across notifications would weaken write-ahead ordering. Combining
false outcomes with intent would need another journal format. Multiple writes
retain ordering but add avoidable kernel transitions.

## Consequences

Rename-over usually saves one write. Malformed complete records still refuse
recovery. Partial writes still fail capture and retain their quarantine entries.
