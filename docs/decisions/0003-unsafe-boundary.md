# Unsafe syscall boundary

## Context
Seccomp listeners, descriptor transfer, fork and signal setup require libc calls. Rust allocation and inherited locks must not run in a forked child before exec.

## Decision
Keep every unsafe block in src/sys/mod.rs, with a SAFETY comment explaining the pointers, storage and lifetime. Core modules use safe wrappers. Prepare CString owners, null terminated argv and environment pointer arrays, BPF and debug descriptors before fork. Forked S and C use stack buffers and syscall wrappers until _exit or execve. S scans /proc with getdents64 rather than allocating through directory iterators.

Descriptors use CLOEXEC at creation or reception. S replaces standard streams only after creating C. SCM_RIGHTS reception validates the ancillary header and uses MSG_CMSG_CLOEXEC. Socket communication uses read, write, sendmsg and recvmsg.

## Alternatives considered
Spreading libc calls across the supervisor obscures pointer lifetimes. std::process::Command cannot implement the listener installation and transfer between fork and exec. Allocating inside the forked process would weaken the audit boundary.

## Consequences
The syscall module is Linux specific. ABI size tests and a small BPF interpreter test the boundary without requiring live foreign ABI support. The safe modules remain suitable for explaining the process model independently of libc details.
