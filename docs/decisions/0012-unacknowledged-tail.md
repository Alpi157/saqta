# Recover an unacknowledged trailing intent after supervisor death

## Context

The forced proc-memory check killed a supervisor during a regular-file write.
The log ended in a partial effect after 66 complete records. Decision 0005 refused
all partial logs, so the existing supervisor-kill test could not restore the
acknowledged effects.

## Decision

Keep the raw effects.log unchanged. Permit a complete-prefix projection only for
an exec journal with a recorded child, no end record, no journal-write error,
a dead supervisor and dead child and effect processes. Parse every complete
record before creating a synced recovery marker. The marker records the prefix
length and exact trailing bytes. Later reads must match that tail and still
parse the complete prefix. Show and undo explain the retained raw tail.

The partial record did not complete its write, so the notification loop did not
reach CONTINUE for that intent. Its syscall cannot have run through that
notification. Previously acknowledged effects still have complete records and
pre-images. Orphan captures remain in quarantine.

Completed journals, live trees, hook journals without an exec child, write-error
journals and malformed complete records continue to refuse automatic recovery.
This assumes process death on a healthy filesystem. Power loss and hostile edits
to the journal remain outside that guarantee.

## Alternatives considered

Ignoring partial records everywhere could hide corruption. Rewriting or truncating
the raw log would remove recovery evidence. Refusing the acknowledged prefix
would leave an ordinary supervisor crash unnecessarily unrecoverable.

## Consequences

The raw file and recovery marker remain available for review. Import fingerprints
include the marker. The regression verifies recovery of a dead incomplete exec
and refusal for the same trailing damage in a completed journal.
