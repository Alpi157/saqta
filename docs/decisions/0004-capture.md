# Capture before continuing a syscall

## Context
Deletes and atomic saves remove names. Write-capable opens can change an inode's bytes. These need different pre-images. A supervisor must also survive ordinary capture errors.

## Decision
S re-execs the same binary before initializing its journal and forking C. This updates decision 0003: the new S may allocate, while fork children take only prepared, async-signal-safe paths until exec. The internal socket and debug descriptor cross that exec deliberately. Their CLOEXEC flags are restored immediately. C inherits neither after its exec.

Read pathname bytes a page at a time, with process_vm_readv and the /proc memory fallback. Validate the notification after reading and again after resolution. Pin the tracee's root and directory fds. Resolve parents with openat2 RESOLVE_IN_ROOT, following intermediate symlinks within that root. Follow the final symlink only for following opens and truncate. Store canonical parent names read through /proc/self/fd.

Hard-link disappearing entries into the main quarantine. Copy regular files when linking fails and before in-place writes. Try FICLONE, copy_file_range, then bounded stream copying. Preserve mode and mtime. Deduplicate write copies by device and inode. Freeze retained hard links before writing another alias. Track creations across removals, renames and exchanges.

Keep the pre-image, write one complete effect line, then CONTINUE. Errors become not-protected effects. The deny floor still returns EPERM. Skip the listed kernel and volatile filesystem types. Limit copies to SAQTA_MAX_COPY, including cross-filesystem captures. Never read a FIFO, socket or device as content.

## Alternatives considered
Copying every deletion costs unnecessary I/O. Keeping only hard links fails for in-place writes. Allocating in M1's original fork child weakens its safety boundary.

## Consequences
Capture is single-threaded. Notification CONTINUE retains filesystem races with unrelated processes. This protects accidental damage, not hostile syscall argument mutation. Undo repairs surviving shared inodes without enumerating aliases.
