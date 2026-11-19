# Capture growth through new aliases whose original name is gone

## Context

A command can create a hard link, remove or replace its source name, then
grow the file through the new link. Another pre-existing name can still
refer to the same inode. The created-file shortcut skipped the growth
effect once the recorded source name was gone. Freezing the original
name's quarantine entry protected that name alone.

## Decision

When a command-created name has no recorded source and the regular file
still has multiple links, capture `truncate` growth as a `modify` effect.
Use `keep_copy` to freeze prior deletion links and retain the pre-image
before allowing the system call. Undo uses the named link to repair the
shared inode before reversing the link's creation, as in decision 0022.

## Alternatives considered

A size-only record on the new name would require rebuilding the original
prefix from a later file version. Capturing the pre-image uses the existing
write, alias repair, and recovery paths. Searching for all original names
would add filesystem walks and could miss aliases outside the project.

## Consequences

Growth in this case uses the existing copy limit and capture failure
reporting. Other growth keeps the existing size-record and deduplication
rules. The journal and undo checkpoint formats stay compatible.
