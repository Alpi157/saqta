# Key filesystem decisions by pinned mount identity

## Context

A device number alone is not a filesystem-type identity. OverlayFS can report
a backing filesystem's device number for a regular file while its directories
report the overlay device. A cached tmpfs exclusion could therefore skip an
overlay file that M4 protected. The kernel documents this
[mixed identity behavior](https://docs.kernel.org/filesystems/overlayfs.html).

## Decision

Request basic metadata and STATX_MNT_ID together with statx. This replaces the
capture path's existing metadata syscall rather than adding a syscall for every
notification. Key filesystem decisions by mount ID. Retain a descriptor to the
classified mount so its ID cannot be recycled while cached. Validate the opened
descriptor's mount identity on a cache miss. Release pins before mount-context
calls as in decision 0016.

If mount identity is unavailable, use the original fresh classification for
each call. If statx is unavailable or denied, fall back to the original stat
wrappers. Never infer filesystem type from a backing device or parent mount.
Normal undo metadata calls keep their existing wrappers.

Add an OverlayFS regression with ext4 lower and tmpfs upper, xino disabled.
Prime a tmpfs skip decision, then modify an overlay file reporting the same
device. Verify capture and byte-exact undo. Repeat without mount IDs to exercise
the uncached fallback. Explicitly skip unavailable mount configurations.

## Alternatives considered

Pinning a device does not distinguish an overlay view from its backing view.
Using the parent device misses file bind mounts. Classifying every target is
safe but loses the optimization on ordinary filesystems.

## Consequences

The cache is per mount rather than per device. Older or restricted kernels keep
M4 classification behavior. No new dependency is needed.
