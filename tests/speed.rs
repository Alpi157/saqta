mod common;
use common::*;
use std::{fs, path::Path, time::Duration};

fn restore_exact(s: &Sandbox, p: &Path, before: &std::collections::BTreeMap<Vec<u8>, u64>) {
    let id = last_id(s);
    let value = shown(s, id);
    assert!(
        value["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["op"] != "not protected"),
        "{value}"
    );
    assert_success(&undo(s, id));
    assert_tree(before, p);
}

fn namespace(s: &Sandbox, pid: bool) -> bool {
    let mut c = s.command("unshare");
    c.args(["--user", "--map-root-user", "--mount"]);
    if pid {
        c.args(["--pid", "--fork", "--mount-proc"]);
    }
    c.arg("true");
    match c.output() {
        Ok(o) if o.status.success() => true,
        other => {
            note(format!("SKIP: namespace unavailable: {other:?}"));
            false
        }
    }
}

#[test]
fn exec_between_deletes_reads_the_new_address_space() {
    for forced in [false, true] {
        let (s, p) = capture_fixture("speed-exec");
        if !python(&s) {
            return;
        }
        fs::write(p.join("a"), b"before exec").unwrap();
        fs::write(p.join("b"), b"after exec").unwrap();
        let before = tree_hash(&p);
        let mut c = s.exec(&["--", "python3", "-c", "import os,sys; os.unlink('a'); os.execv(sys.executable,[sys.executable,'-c',\"import os; os.unlink('b')\"])"]);
        c.current_dir(&p)
            .env("SAQTA_FORCE_PROC_MEM", if forced { "1" } else { "0" });
        assert_success(&run(c));
        restore_exact(&s, &p, &before);
    }
}

#[test]
fn reused_pid_reads_the_new_process_memory() {
    for fallback in [false, true] {
        let (mut s, p) = capture_fixture("speed-pid-reuse");
        if fallback {
            s.env.insert("SAQTA_TEST_NO_PIDFD".into(), "1".into());
        }
        if !python(&s) || !namespace(&s, true) {
            return;
        }
        for name in ["a", "b"] {
            fs::write(p.join(name), name).unwrap();
        }
        fs::create_dir(p.join("first")).unwrap();
        fs::rename(p.join("a"), p.join("first/file")).unwrap();
        fs::rename(p.join("b"), p.join("file")).unwrap();
        let before = tree_hash(&p);
        let text = r#"
import os,sys,subprocess
try:
    last='/proc/sys/kernel/ns_last_pid'
    with open(last) as f: n=f.read()
    with open(last,'w') as f: f.write(n)
except OSError as e:
    print('SKIP: PID reuse control unavailable:',e,flush=True); sys.exit(77)
# The unfiltered parent controls allocation. No notification between the first
# child's exit and the reused PID's unlink may evict the warmed task cache.
payload = """
import os
first=os.fork()
if first==0:
    os.chdir('first'); os.unlink('file'); os._exit(0)
assert os.waitpid(first,0)[1]==0
print(first,flush=True)
os.read(0,1)
second=os.fork()
if second==0:
    os.unlink('file'); os._exit(0)
assert second==first,(first,second)
assert os.waitpid(second,0)[1]==0
"""
child=subprocess.Popen([sys.argv[1],'exec','--',sys.executable,'-c',payload],stdin=subprocess.PIPE,stdout=subprocess.PIPE)
first=int(child.stdout.readline())
with open(last,'w') as f: f.write(str(first-1))
child.stdin.write(b'x'); child.stdin.flush()
assert child.wait()==0
print('reused cached PID',first,flush=True)
"#;

        let mut c = s.command("unshare");
        c.args([
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "--mount-proc",
            "python3",
            "-c",
            text,
            BIN,
        ])
        .current_dir(&p);
        let o = run(c);
        if o.status.code() == Some(77) {
            note(String::from_utf8_lossy(&o.stdout));
            return;
        }
        assert_success(&o);
        note(String::from_utf8_lossy(&o.stdout));
        restore_exact(&s, &p, &before);
    }
}

#[test]
fn chroot_between_deletes_uses_the_new_root() {
    let (s, p) = capture_fixture("speed-chroot");
    if !python(&s) || !namespace(&s, false) {
        return;
    }
    fs::create_dir(p.join("root")).unwrap();
    fs::write(p.join("a"), b"outside root").unwrap();
    fs::write(p.join("root/b"), b"inside root").unwrap();
    let before = tree_hash(&p);
    let text = "import os; os.unlink('a'); os.chroot('root'); os.chdir('/'); os.unlink('/b')";
    let mut c = s.command("unshare");
    c.args([
        "--user",
        "--map-root-user",
        "--mount",
        BIN,
        "exec",
        "--",
        "python3",
        "-c",
        text,
    ])
    .current_dir(&p);
    assert_success(&run(c));
    restore_exact(&s, &p, &before);
}

#[test]
fn bind_mount_change_on_cached_device_still_captures_the_new_file() {
    let (s, p) = capture_fixture("speed-mount");
    if !python(&s) || !namespace(&s, false) {
        return;
    }
    for dir in ["seed", "source", "slot"] {
        fs::create_dir(p.join(dir)).unwrap();
    }
    fs::write(p.join("seed/a"), b"seed filesystem decision").unwrap();
    fs::write(p.join("source/b"), b"same device, new mount").unwrap();
    let before = tree_hash(&p);
    let driver = p.parent().unwrap().join("mount.py");
    fs::write(
        &driver,
        r#"
import ctypes,os,subprocess,sys
libc=ctypes.CDLL(None,use_errno=True)
os.unlink('seed/a')
assert libc.mount(b'source',b'slot',None,4096,None)==0,ctypes.get_errno()
assert os.stat('source').st_dev==os.stat('slot').st_dev
os.unlink('slot/b')
"#,
    )
    .unwrap();
    let mut c = s.command("unshare");
    c.args([
        "--user",
        "--map-root-user",
        "--mount",
        "sh",
        "-c",
        "\"$1\" exec -- python3 \"$2\" && \"$1\" undo 1",
        "sh",
        BIN,
    ])
    .arg(&driver)
    .current_dir(&p);
    assert_success(&Running::spawn(c, None).output_with_timeout(Duration::from_secs(60)));
    assert_tree(&before, &p);
}

#[test]
fn directory_fd_reuse_and_cwd_rename_use_fresh_parents() {
    let (s, p) = capture_fixture("speed-dirfd");
    if !python(&s) {
        return;
    }
    for dir in ["a", "b"] {
        fs::create_dir(p.join(dir)).unwrap();
        fs::write(p.join(dir).join("file"), dir).unwrap();
    }
    let before = tree_hash(&p);
    let text = r#"
import os
first=os.open('a',os.O_RDONLY|os.O_DIRECTORY)
os.unlink('file',dir_fd=first)
os.close(first)
second=os.open('b',os.O_RDONLY|os.O_DIRECTORY)
assert first==second
os.rename('b','moved')
os.unlink('file',dir_fd=second)
os.close(second)
"#;
    let mut c = s.exec(&["--", "python3", "-c", text]);
    c.current_dir(&p);
    assert_success(&run(c));
    restore_exact(&s, &p, &before);
}

#[test]
fn fchdir_between_deletes_invalidates_cached_cwd() {
    let (s, p) = capture_fixture("speed-fchdir");
    if !python(&s) {
        return;
    }
    for dir in ["a", "b"] {
        fs::create_dir(p.join(dir)).unwrap();
        fs::write(p.join(dir).join("file"), dir).unwrap();
    }
    let before = tree_hash(&p);
    let text = "import os; b=os.open('b',os.O_RDONLY|os.O_DIRECTORY); os.chdir('a'); os.unlink('file'); os.fchdir(b); os.unlink('file'); os.close(b)";
    let mut c = s.exec(&["--", "python3", "-c", text]);
    c.current_dir(&p);
    assert_success(&run(c));
    restore_exact(&s, &p, &before);
}

#[test]
fn exec_invalidates_readable_old_mm_retained_by_clone_vm() {
    let (s, p) = capture_fixture("speed-old-mm");
    if s.command("cc").arg("--version").output().is_err() {
        note("SKIP: cc is missing for the retained-mm exec fixture");
        return;
    }
    let source = s.dir.join("old-mm.c");
    let binary = s.dir.join("old-mm");
    fs::write(&source, r#"
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
static char name[16]="a";
static char stack[65536] __attribute__((aligned(16)));
static int pipefd[2];
static int hold(void *unused) {
    (void)unused; char byte; if(read(pipefd[0],&byte,1)!=1) _exit(2); _exit(0);
}

int main(int argc,char **argv) {
    if(argc==1) {
        if(unlink(name)) return 2;
        if(pipe(pipefd)) return 77;
        int child=clone(hold,stack+sizeof stack,CLONE_VM|SIGCHLD,NULL);
        if(child<0) return 77;
        char pid[32],fd[32]; snprintf(pid,sizeof pid,"%d",child); snprintf(fd,sizeof fd,"%d",pipefd[1]);
        char *next[]={argv[0],"second",pid,fd,NULL}; execv(argv[0],next); return 2;
    }
    strcpy(name,"b");
    int result=unlink(name), status=0;
    if(write(atoi(argv[3]),"x",1)!=1) return 2;
    if(waitpid(atoi(argv[2]),&status,0)<0) return 2;
    return result || status ? 2 : 0;
}
"#).unwrap();
    let mut cc = s.command("cc");
    cc.args(["-fno-pie", "-no-pie", "-O0", "-o"])
        .arg(&binary)
        .arg(&source);
    let compiled = run(cc);
    if !compiled.status.success() {
        note(format!(
            "SKIP: cc cannot build non-PIE clone fixture: {}",
            String::from_utf8_lossy(&compiled.stderr)
        ));
        return;
    }
    fs::write(p.join("a"), b"old mm pathname").unwrap();
    fs::write(p.join("b"), b"new mm pathname").unwrap();
    let before = tree_hash(&p);
    let mut c = s.exec(&["--"]);
    c.arg(&binary)
        .current_dir(&p)
        .env("SAQTA_FORCE_PROC_MEM", "1");
    let o = run(c);
    if o.status.code() == Some(77) {
        note("SKIP: CLONE_VM unavailable");
        return;
    }
    assert_success(&o);
    restore_exact(&s, &p, &before);
}

#[test]
fn unpublished_positive_batch_keeps_failed_outcomes_and_recovers_successes() {
    let (s, p) = capture_fixture("speed-positive-crash");
    if !python(&s) {
        return;
    }
    fs::create_dir(p.join("directory")).unwrap();
    for name in ["victim", "source", "directory/keep"] {
        fs::write(p.join(name), name).unwrap();
    }
    let before = tree_hash(&p);
    let text = "import os; os.unlink('victim')\ntry: os.rename('source','directory')\nexcept OSError: pass";
    let mut c = s.exec(&["--", "python3", "-c", text]);
    c.current_dir(&p);
    assert_success(&run(c));
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let folder = home.join("cmd/1");
    let positives: Vec<u64> =
        serde_json::from_slice(&fs::read(folder.join("outcomes-positive.json")).unwrap()).unwrap();
    assert!(!positives.is_empty());
    assert!(
        fs::read_to_string(folder.join("outcomes.log"))
            .unwrap()
            .contains("\t0\n")
    );
    // Model death before the atomic success publication. Failed observations
    // remain durable, and unknown successes keep the existing replay behavior.
    fs::remove_file(folder.join("outcomes-positive.json")).unwrap();
    fs::remove_file(folder.join("end")).unwrap();
    assert_success(&undo(&s, 1));
    assert_tree(&before, &p);
}

#[test]
fn damaged_published_positive_batch_refuses_undo() {
    let (s, p) = capture_fixture("speed-positive-damage");
    fs::write(p.join("victim"), b"original").unwrap();
    assert_success(&script(&s, &p, "rm victim"));
    let before = tree_hash(&p);
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    fs::write(home.join("cmd/1/outcomes-positive.json"), b"[broken").unwrap();
    assert!(!undo(&s, 1).status.success());
    assert_tree(&before, &p);
}

#[test]
fn unacknowledged_rename_batch_prefix_preserves_both_original_names() {
    let (s, p) = capture_fixture("speed-group-tail");
    if !python(&s) {
        return;
    }
    fs::write(p.join("a"), b"original a").unwrap();
    fs::write(p.join("b"), b"original b").unwrap();
    let before = tree_hash(&p);
    let mut c = s.exec(&["--", "python3", "-c", "import os; os.replace('a','b')"]);
    c.current_dir(&p);
    assert_success(&run(c));
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let folder = home.join("cmd/1");
    let raw = fs::read_to_string(folder.join("effects.log")).unwrap();
    let lines: Vec<_> = raw.lines().collect();
    assert_eq!(lines.len(), 2);
    let fields: Vec<_> = lines[0].split('\t').collect();
    assert_eq!(fields[3], "delete");
    // Model the stopped syscall before CONTINUE, with a partial group write.
    fs::rename(p.join("b"), p.join("a")).unwrap();
    fs::hard_link(home.join("q/main/1").join(fields[6]), p.join("b")).unwrap();
    let damaged = format!("{}\n{}", lines[0], &lines[1][..lines[1].len() / 2]);
    fs::write(folder.join("effects.log"), &damaged).unwrap();
    for file in ["end", "post.log", "outcomes-positive.json"] {
        fs::remove_file(folder.join(file)).unwrap();
    }
    assert_success(&undo(&s, 1));
    assert_tree(&before, &p);
    assert_eq!(
        fs::read_to_string(folder.join("effects.log")).unwrap(),
        damaged
    );
    assert!(folder.join("tail-recovery.json").exists());
}

#[test]
fn skipped_filesystem_cache_does_not_keep_a_mount_busy() {
    let (s, p) = capture_fixture("speed-unmount");
    if !python(&s) || !namespace(&s, false) {
        return;
    }
    fs::create_dir(p.join("slot")).unwrap();
    let text = r#"
import ctypes,os,sys
libc=ctypes.CDLL(None,use_errno=True)
if libc.mount(b'tmpfs',b'slot',b'tmpfs',0,b'size=1m')!=0:
    print('SKIP: tmpfs mount unavailable:',ctypes.get_errno(),flush=True);sys.exit(77)
with open('slot/file','wb') as f:f.write(b'temporary')
os.unlink('slot/file')
assert libc.umount2(b'slot',0)==0,ctypes.get_errno()
"#;
    let mut c = s.command("unshare");
    c.args([
        "--user",
        "--map-root-user",
        "--mount",
        BIN,
        "exec",
        "--",
        "python3",
        "-c",
        text,
    ])
    .current_dir(&p);
    let o = run(c);
    if o.status.code() == Some(77) {
        note(String::from_utf8_lossy(&o.stdout));
        return;
    }
    assert_success(&o);
}

#[test]
fn overlay_backing_device_does_not_reuse_tmpfs_skip_decision() {
    for fallback in [false, true] {
        let (mut s, p) = capture_fixture("speed-overlay");
        if !python(&s) || !namespace(&s, false) {
            return;
        }
        if fallback {
            s.env.insert("SAQTA_TEST_NO_MOUNT_ID".into(), "1".into());
        }
        let text = r#"
import ctypes,os,pathlib,subprocess,sys,json
libc=ctypes.CDLL(None,use_errno=True)
p=pathlib.Path.cwd()
for name in ('tmp','lower','merged'):(p/name).mkdir()
if libc.mount(b'tmpfs',b'tmp',b'tmpfs',0,b'size=8m')!=0:
    print('SKIP: tmpfs unavailable:',ctypes.get_errno(),flush=True);sys.exit(77)
for name in ('upper','work'):(p/'tmp'/name).mkdir()
(p/'tmp/upper/file').write_bytes(b'original overlay bytes')
(p/'tmp/skip').write_bytes(b'volatile')
options=f'lowerdir={p}/lower,upperdir={p}/tmp/upper,workdir={p}/tmp/work,xino=off,index=off,userxattr'.encode()
if libc.mount(b'overlay',b'merged',b'overlay',0,options)!=0:
    print('SKIP: overlay unavailable:',ctypes.get_errno(),flush=True);sys.exit(77)
if os.stat('tmp/skip').st_dev!=os.stat('merged/file').st_dev:
    print('SKIP: backing-device collision is unavailable; continuing overlay capture and uncached fallback',flush=True)
code="import os; fd=os.open('tmp/skip',os.O_WRONLY); os.close(fd); open('merged/file','wb').write(b'new overlay bytes')"
subprocess.run([sys.argv[1],'exec','--',sys.executable,'-c',code],check=True)
record=json.loads(subprocess.check_output([sys.argv[1],'show','1','--json']))
assert any(e['op']=='modify' and e['path'].endswith('/merged/file') for e in record['effects']),record
subprocess.run([sys.argv[1],'undo','1'],check=True,stdout=subprocess.DEVNULL)
assert (p/'merged/file').read_bytes()==b'original overlay bytes'
"#;
        let mut c = s.command("unshare");
        c.args([
            "--user",
            "--map-root-user",
            "--mount",
            "python3",
            "-c",
            text,
            BIN,
        ])
        .current_dir(&p);
        let o = run(c);
        if o.status.code() == Some(77) {
            note(String::from_utf8_lossy(&o.stdout));
            return;
        }
        assert_success(&o);
        if !o.stdout.is_empty() {
            note(String::from_utf8_lossy(&o.stdout));
        }
    }
}
