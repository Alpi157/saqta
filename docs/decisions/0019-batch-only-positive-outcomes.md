# Batch positive outcomes without changing crash replay

## Context

Each observed outcome added a write while the tracee waited. Buffering a failed
outcome across a dependent CONTINUE could make recovery replay an effect that
the existing journal had proved unsuccessful.

## Decision

Keep false outcomes in outcomes.log, written immediately as before. Buffer only
true outcomes. Atomically publish the complete positive sequence list in
outcomes-positive.json before publishing a snapshot or end. Include that file
in the rebuildable index fingerprint. Read both formats, rejecting contradictory
or invalid positive sequences. Existing journals remain readable.

This is safe even when the next CONTINUE depends on the observed success.
Undo, origin mapping, expiry and conflict checks already treat an unknown
outcome exactly like true. Losing any unpublished positive prefix after
supervisor death therefore changes no recovery decision. A false observation
still reaches its log before any dependent CONTINUE. No buffered information
can turn a known failure into a replayed success.

Publish positives by write-then-rename instead of appending a larger outcomes.log
write. A killed publication leaves either the previous complete list or the new
complete list. A partial temporary file authorizes nothing and stays available.
Malformed published lists refuse recovery. Effect lines remain separate,
complete writes after pre-images and before CONTINUE, preserving decisions
0005 and 0012 and their trailing-intent recovery rules.

## Alternatives considered

Batching false outcomes needs a stronger dependency and crash protocol. Appending
positive batches to the old log would need another partial-tail format. Writing
each positive line at command end keeps the syscall count unchanged.

## Consequences

Live journals may show successful outcomes as unknown until the next snapshot.
This does not change replay. The buffered list grows with successful effects.
Exec still performs no durability syncs.
