# Verify the incarnation of alias repair targets

## Context

Recycled inode numbers made undo mistake recovery metadata for deleted data.

## Decision

Alias identity requires device, inode, at least one incarnation marker, and
equality of every available field. Size and mtime are independent observations.
Unverifiable lineage, including legacy records, means conflict. Keep both versions.

Capture requests birth time in its existing statx. A quarantine hard link pins
the inode, so deletes defer generation reads. Mark these effects as pinned.
Loaders complete their State through that link. Save it in inode-pins.json before
freeze_links replaces the link, undo consumes it, or gc collects it. Inspection
only reads. Copies use FS_IOC_GETVERSION on their already-open source descriptor.

Unpinned names need generation immediately. Read the kernel's generic
FILEID_INO32_GEN file handle through the existing parent descriptor; accept only
its exact format and matching inode. It supplies the ioctl's i_generation without
opening the file. Unknown formats or denied calls mark unpinned States unverified. Undo
reads full markers through a pinned descriptor and requires exact equality.
Never complete identity from an unpinned name. ext4 birth timestamps can coincide
within a clock tick. Generation distinguishes those lifetimes; birth time cannot
substitute for a missing generation read.

Search and publish only explicitly retained data handles inside SAQTA_HOME.
Record provenance before displacement and verify identity again before repair.
Checkpoint provenance and input alias lineage for resume. Exclude control files
and immutable size-N prefix sources.

## Alternatives considered

Size and mtime change through aliases. Filename exclusions miss future controls.
Opening every capture target adds watcher events and system calls.

## Consequences

Pin certificates and provenance use the existing checkpoint order.
Unsupported identity reads cause conservative conflicts. No capture-only opens
are needed for markers. Legacy maps never authorize unverified repair.
Unverifiable syscall outcomes stay unknown so undo cannot silently skip them.
