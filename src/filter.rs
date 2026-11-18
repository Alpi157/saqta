use crate::sys::SeccompData;
use libc::{
    BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
    SECCOMP_RET_ERRNO, SECCOMP_RET_USER_NOTIF, sock_filter,
};
use std::mem::offset_of;

const AUDIT_ARCH_X86_64: u32 = 0xc000003e;
const X32_SYSCALL_BIT: u32 = 0x40000000;
const LOAD_WORD: u16 = (BPF_LD | BPF_W | BPF_ABS) as u16;
const JUMP_EQUAL: u16 = (BPF_JMP | BPF_JEQ | BPF_K) as u16;
const JUMP_BITS_SET: u16 = (BPF_JMP | BPF_JSET | BPF_K) as u16;
const RETURN_CONSTANT: u16 = (BPF_RET | BPF_K) as u16;
const NOTIFY: &[i64] = &[
    libc::SYS_unlink,
    libc::SYS_unlinkat,
    libc::SYS_rmdir,
    libc::SYS_rename,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_truncate,
    libc::SYS_creat,
    libc::SYS_mkdir,
    libc::SYS_mkdirat,
    libc::SYS_link,
    libc::SYS_linkat,
    libc::SYS_symlink,
    libc::SYS_symlinkat,
    libc::SYS_mknod,
    libc::SYS_mknodat,
];

// These calls invalidate task memory or filesystem context before CONTINUE.
pub const CONTEXT_CALLS: &[i64] = &[
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_chroot,
    libc::SYS_pivot_root,
    libc::SYS_setns,
    libc::SYS_unshare,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_move_mount,
];

pub fn program() -> Vec<sock_filter> {
    let ins = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    let enosys = SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
    let mut p = vec![
        // Load the architecture tag.
        ins(LOAD_WORD, 0, 0, offset_of!(SeccompData, arch) as u32),
        // Native x86_64 skips the rejection.
        ins(JUMP_EQUAL, 1, 0, AUDIT_ARCH_X86_64),
        // Reject foreign architectures with ENOSYS.
        ins(RETURN_CONSTANT, 0, 0, enosys),
        // Load the syscall number.
        ins(LOAD_WORD, 0, 0, offset_of!(SeccompData, nr) as u32),
        // Native numbers skip the x32 rejection.
        ins(JUMP_BITS_SET, 0, 1, X32_SYSCALL_BIT),
        // Reject the x32 ABI with ENOSYS.
        ins(RETURN_CONSTANT, 0, 0, enosys),
    ];
    for (nr, arg) in [(libc::SYS_open, 1), (libc::SYS_openat, 2)] {
        p.extend([
            // Other syscalls skip this entire flags block.
            ins(JUMP_EQUAL, 0, 5, nr as u32),
            // Load the low word of this syscall's flags argument.
            ins(
                LOAD_WORD,
                0,
                0,
                (offset_of!(SeccompData, args) + arg * size_of::<u64>()) as u32,
            ),
            // Creation and truncation notify even with O_APPEND.
            ins(JUMP_BITS_SET, 1, 0, (libc::O_CREAT | libc::O_TRUNC) as u32),
            // Write access notifies; read-only opens skip to allow.
            ins(JUMP_BITS_SET, 0, 1, (libc::O_WRONLY | libc::O_RDWR) as u32),
            // Suspend the write-capable open.
            ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_USER_NOTIF),
            // Allow the read-only open.
            ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_ALLOW),
        ]);
    }
    for &nr in NOTIFY.iter().chain(CONTEXT_CALLS) {
        p.extend([
            // Matching syscalls enter the notification return.
            ins(JUMP_EQUAL, 0, 1, nr as u32),
            // Wait for the supervisor's capture and answer.
            ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_USER_NOTIF),
        ]);
    }
    for nr in [libc::SYS_openat2, libc::SYS_io_uring_setup] {
        p.extend([
            // Only this unsupported interface takes the rejection.
            ins(JUMP_EQUAL, 0, 1, nr as u32),
            // Force a fallback to visible syscalls.
            ins(RETURN_CONSTANT, 0, 0, enosys),
        ]);
    }
    // Leave all other native syscalls alone.
    p.push(ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_ALLOW));
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interpret(nr: u32, arch: u32, args: [u64; 6]) -> u32 {
        let p = program();
        let (mut pc, mut accumulator) = (0, 0);
        loop {
            let i = p[pc];
            pc += 1;
            match i.code {
                LOAD_WORD => {
                    accumulator = match i.k as usize {
                        offset if offset == offset_of!(SeccompData, nr) => nr,
                        offset if offset == offset_of!(SeccompData, arch) => arch,
                        offset if offset >= offset_of!(SeccompData, args) => {
                            let byte = offset - offset_of!(SeccompData, args);
                            (args[byte / size_of::<u64>()] >> ((byte % size_of::<u64>()) * 8))
                                as u32
                        }
                        offset => panic!("unsupported load {offset}"),
                    }
                }
                JUMP_EQUAL => pc += if accumulator == i.k { i.jt } else { i.jf } as usize,
                JUMP_BITS_SET => pc += if accumulator & i.k != 0 { i.jt } else { i.jf } as usize,
                RETURN_CONSTANT => return i.k,
                code => panic!("unsupported BPF opcode {code}"),
            }
        }
    }
    #[test]
    fn every_syscall_rule() {
        for nr in 0..512 {
            let expected = if NOTIFY.contains(&(nr as i64)) || CONTEXT_CALLS.contains(&(nr as i64))
            {
                SECCOMP_RET_USER_NOTIF
            } else if [libc::SYS_openat2, libc::SYS_io_uring_setup].contains(&(nr as i64)) {
                SECCOMP_RET_ERRNO | libc::ENOSYS as u32
            } else {
                SECCOMP_RET_ALLOW
            };
            assert_eq!(
                interpret(nr, AUDIT_ARCH_X86_64, [0; 6]),
                expected,
                "nr {nr}"
            );
            assert_eq!(
                interpret(nr, 0x40000003, [0; 6]),
                SECCOMP_RET_ERRNO | libc::ENOSYS as u32
            );
            assert_eq!(
                interpret(nr | X32_SYSCALL_BIT, AUDIT_ARCH_X86_64, [0; 6]),
                SECCOMP_RET_ERRNO | libc::ENOSYS as u32
            );
        }
    }
    #[test]
    fn every_open_flags_combination() {
        let bits = [
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_CREAT,
            libc::O_TRUNC,
            libc::O_APPEND,
            libc::O_EXCL,
            libc::O_NOFOLLOW,
        ];
        for mask in 0..(1 << bits.len()) {
            let flags = bits
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .fold(libc::O_CLOEXEC, |f, (_, bit)| f | bit);
            let modifying =
                flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) != 0;
            for (nr, arg) in [(libc::SYS_open, 1), (libc::SYS_openat, 2)] {
                let mut args = [u64::MAX; 6];
                args[arg] = flags as u64;
                assert_eq!(
                    interpret(nr as u32, AUDIT_ARCH_X86_64, args),
                    if modifying {
                        SECCOMP_RET_USER_NOTIF
                    } else {
                        SECCOMP_RET_ALLOW
                    },
                    "flags {flags:#x}"
                );
            }
        }
    }
}
