mod common;
use common::*;
use std::{fs, os::unix::process::CommandExt, process::Stdio};

#[test]
fn x32_getpid_returns_enosys_under_filter() {
    let s = Sandbox::new("x32_getpid_returns_enosys_under_filter");
    if s.command("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        note("SKIP: python3 is missing for raw x32 syscall probe");
        return;
    }
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); r=l.syscall(0x40000000|39); print(ctypes.get_errno() if r<0 else 0)";
    let mut command = s.command("python3");
    command.args(["-c", code]);
    let bare = run(command);
    if !bare.status.success() || bare.stdout != b"0\n" {
        note(format!(
            "SKIP: bare x32 getpid unavailable: {}; {}",
            bare.status,
            String::from_utf8_lossy(&bare.stdout)
        ));
        return;
    }
    let o = s.run(&["--", "python3", "-c", code]);
    assert_success(&o);
    assert_eq!(o.stdout, b"38\n");
}
#[test]
fn i386_getpid_returns_enosys_under_filter() {
    let s = Sandbox::new("i386_getpid_returns_enosys_under_filter");
    fs::write(s.dir.join("abi.c"),"#include <stdio.h>\nint main(void){int r; __asm__ volatile(\"int $0x80\":\"=a\"(r):\"a\"(20):\"memory\"); printf(\"%d\\n\",r); return 0;}\n").unwrap();
    let mut cc = s.command("cc");
    cc.args(["abi.c", "-o", "abi"]);
    match cc.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            note("SKIP: cc is missing for i386 ABI probe");
            return;
        }
        Err(e) => panic!("cannot start cc: {e}"),
        Ok(o) if !o.status.success() => {
            note(format!(
                "SKIP: cc cannot compile i386 ABI probe: {}",
                String::from_utf8_lossy(&o.stderr)
            ));
            return;
        }
        Ok(_) => {}
    }
    let mut command = s.command("./abi");
    // SAFETY: the child only passes a stack rlimit to setrlimit before exec.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &limit) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let bare = run(command);
    if !bare.status.success()
        || String::from_utf8_lossy(&bare.stdout)
            .trim()
            .parse::<i32>()
            .is_ok_and(|v| v < 0)
    {
        note(format!(
            "SKIP: bare i386 getpid unavailable: {}; {}",
            bare.status,
            String::from_utf8_lossy(&bare.stdout)
        ));
        return;
    }
    assert!(
        String::from_utf8_lossy(&bare.stdout)
            .trim()
            .parse::<i32>()
            .unwrap()
            > 0
    );
    let o = s.run(&["--", "./abi"]);
    assert_success(&o);
    assert_eq!(o.stdout, b"-38\n");
}
