# Retain recent work and label collected content

## Context

Quarantine grows without a retention policy. Collection must never compete with
capture or undo, and missing content must have a clear explanation.

## Decision

Collect regenerable content after one day, whole commands after seven days, then
oldest commands until retained commands and versions fit five decimal GB. CLI
options override the last two limits. Count logical bytes once per inode across
`cmd/` and `q/`; the rebuildable database and diagnostic logs are excluded.

Protect the newest three command ids, live commands, incomplete journals and
partial undos. Take the same command lock as undo, then check protection again.
One lock serializes collectors. Any partial undo defers collection because
older undo folders may hold shared inode handles. Dry-run takes no write locks and writes no
markers.

Collect shared pre-images only when every reference is regenerable.
Recognize named components and CACHEDIR.TAG signatures, including retained deleted tags. Sync expiry names before unlinking.
Undo reports each expired path and restores the remaining effects. Whole-command
collection removes metadata last so interrupted collection remains explainable.

After printing, log, show, status and undo may launch a collector with closed
standard streams. A timestamp reservation under the collector lock bounds this
to one attempt per day. New stores start their first interval at creation.
Status reports retained size and the last successful collection.

If retained pre-image metadata changed, keep the store and report it. Undoing
newer commands can recover shared hard links.

## Alternatives considered

Deleting missing content without expiry records looks like corruption. Running
an unbounded scan before interactive output would repeat the CLI latency problem.

## Consequences

Protected commands and orphaned recovery content can keep size above the limit.
Collection keeps these entries. Expired versions cannot be restored. Diagnostic
logs and the rebuildable index require separate accounting.
