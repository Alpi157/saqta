use crate::sys::SeccompData;
use libc::{
    BPF_ABS, BPF_JEQ, BPF_JMP, BPF_JSET, BPF_K, BPF_LD, BPF_RET, BPF_W, SECCOMP_RET_ALLOW,
    SECCOMP_RET_ERRNO, SECCOMP_RET_USER_NOTIF, sock_filter,
};
use std::mem::offset_of;

// Linux audit architecture tag and the alternate x86_64 syscall ABI bit.
const AUDIT_ARCH_X86_64: u32 = 0xc000003e;
const X32_SYSCALL_BIT: u32 = 0x40000000;
const LOAD_WORD: u16 = (BPF_LD | BPF_W | BPF_ABS) as u16;
const JUMP_EQUAL: u16 = (BPF_JMP | BPF_JEQ | BPF_K) as u16;
const JUMP_BITS_SET: u16 = (BPF_JMP | BPF_JSET | BPF_K) as u16;
const RETURN_CONSTANT: u16 = (BPF_RET | BPF_K) as u16;

pub fn program() -> Vec<sock_filter> {
    let ins = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    let enosys = SECCOMP_RET_ERRNO | libc::ENOSYS as u32;
    vec![
        // Load the architecture tag from the kernel's syscall metadata.
        ins(LOAD_WORD, 0, 0, offset_of!(SeccompData, arch) as u32),
        // Native x86_64 skips the architecture rejection.
        ins(JUMP_EQUAL, 1, 0, AUDIT_ARCH_X86_64),
        // Reject foreign architectures, including i386, with ENOSYS.
        ins(RETURN_CONSTANT, 0, 0, enosys),
        // Load the syscall number from the same metadata.
        ins(LOAD_WORD, 0, 0, offset_of!(SeccompData, nr) as u32),
        // Numbers without the x32 ABI bit skip the next rejection.
        ins(JUMP_BITS_SET, 0, 1, X32_SYSCALL_BIT),
        // Reject x32 syscalls with ENOSYS.
        ins(RETURN_CONSTANT, 0, 0, enosys),
        // unlink jumps directly to the notification return.
        ins(JUMP_EQUAL, 1, 0, libc::SYS_unlink as u32),
        // unlinkat also notifies; every other number skips to allow.
        ins(JUMP_EQUAL, 0, 1, libc::SYS_unlinkat as u32),
        // Suspend the caller until the supervisor answers.
        ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_USER_NOTIF),
        // Leave all other native syscalls alone.
        ins(RETURN_CONSTANT, 0, 0, SECCOMP_RET_ALLOW),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interpret(nr: u32, arch: u32) -> u32 {
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
                        offset => panic!("unsupported load offset {offset}"),
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
    fn decisions() {
        for nr in [libc::SYS_unlink, libc::SYS_unlinkat] {
            assert_eq!(
                interpret(nr as u32, AUDIT_ARCH_X86_64),
                SECCOMP_RET_USER_NOTIF
            );
        }
        assert_eq!(
            interpret(libc::SYS_openat as u32, AUDIT_ARCH_X86_64),
            SECCOMP_RET_ALLOW
        );
        const AUDIT_ARCH_I386: u32 = 0x40000003;
        assert_eq!(
            interpret(20, AUDIT_ARCH_I386),
            SECCOMP_RET_ERRNO | libc::ENOSYS as u32
        );
        assert_eq!(
            interpret(X32_SYSCALL_BIT | libc::SYS_getpid as u32, AUDIT_ARCH_X86_64),
            SECCOMP_RET_ERRNO | libc::ENOSYS as u32
        );
    }
}
