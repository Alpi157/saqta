# Append undo plans and batch checkpoint replacement

## Context

M4 atomically replaced state.json for each plan and intermediate action.
Replayable actions already recognize mutations that finished before their
completion record. Replacement creates many temporary files in a bulk restore.

## Decision

Append each complete recovery frame to state.log before its dependent mutation.
These writes remain unbuffered. Replace state.json every 128 frames or 250 ms,
and at completion. Publish the checkpoint before truncating the matching log.
Resume validates every complete frame and replays from the last one. It can
repeat an already completed action using the existing inode and publication
identities. An incomplete final frame cannot have authorized a mutation. Keep
its exact raw log under a separate name before continuing. Malformed complete
frames refuse resume. Old checkpoints remain readable.

The synced displacement map, copy staging markers, immutable sources and final
undo marker keep their existing order. No unsaved plan can authorize a mutation.
This preserves decision 0009's process-crash guarantee. Power-loss recovery
still requires manual review.

Remember absences established by preflight and completed reverse moves during
one run. Avoid checking those destinations again while planning a delete.
Invalidate this knowledge on copies, exchanges and directory moves. Publications
still use NOREPLACE, so newly appearing work causes refusal and remains intact.
Resume starts with no remembered absences.

## Alternatives considered

Batching mutations without write-ahead plans would lose resume identities.
Persisting a completion count alone cannot safely repeat exchanges. Retaining
unvalidated parent descriptors across effects could follow stale names.

Only command quarantine and private undo parents are reused across effects under
the command lock. Working-tree parents keep their secure walks.

## Consequences

state.log holds one checkpoint interval. Recovery state is
written once per plan, with far fewer file creations. Timeout diagnostics read
the latest complete log frame as well as the checkpoint.
