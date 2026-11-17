# Import only changed command journals

## Context

Every log, show and undo reparsed all command effects and deleted and reinserted
their SQLite rows. One 65,899-effect command slowed later one-file undos.

## Decision

Cache a summary and a fingerprint for each command. The fingerprint includes
inode, length and nanosecond mtime for metadata, child, effects, outcomes, post
states, completion, undo, error and expiry files. Import effect rows only when
that fingerprint changes. Refresh running commands even with an unchanged stamp.
Remove cached rows for collected folders. Keep the original schema version and
BLOB path representation, adding a rebuildable imports table.

Show and undo refresh only the requested command. They recover missing post-state
observations without importing unrelated history. Log reads cached summaries for
unchanged commands. Deleting journal.db still rebuilds it from plain journals.

Pure append opens now enter capture. The model harness demonstrated that the
spike's append bypass left appended bytes after undo. This updates decision 0004's
filter policy without changing the supervisor's process model.

## Alternatives considered

Putting SQLite in the notification loop would slow every captured syscall.
Persisting a JSON cache of all effects would still parse the large command on
each invocation. Depending only on an end marker would miss undo and GC changes.

## Consequences

A first import or explicit rebuild still scales with effects. Repeated calls
avoid that work. Fingerprints assume ordinary filesystem changes; they do not
promise detection of hostile journal edits that restore all recorded metadata.
