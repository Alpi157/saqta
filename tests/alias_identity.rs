#![cfg(debug_assertions)]
mod common;
#[path = "../src/sys/mod.rs"]
#[allow(dead_code, unused_imports)]
mod sys;
use common::*;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

fn home(s: &Sandbox) -> std::path::PathBuf {
    s.env[std::ffi::OsStr::new("SAQTA_HOME")].clone().into()
}
fn before(s: &Sandbox, id: u64) -> Value {
    shown(s, id)["effects"][0]["note"]["before"].clone()
}
fn post(s: &Sandbox, id: u64) -> Value {
    let text = fs::read_to_string(home(s).join(format!("cmd/{id}/post.log"))).unwrap();
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find_map(|post| post["state"].as_object().map(|_| post["state"].clone()))
        .unwrap()
}
fn lineage(s: &Sandbox, path: &Path, origin: Value, actual: Value) {
    fs::write(
        home(s).join("undo-aliases.json"),
        serde_json::to_vec(&json!({encoded_path(path): [origin, actual]})).unwrap(),
    )
    .unwrap();
}

#[test]
fn recycled_inode_in_lineage_is_a_conflict_and_keeps_the_new_file() {
    let (s, root) = capture_fixture("alias-incarnation");
    let f = root.join("f");
    let alias = root.join("alias");
    fs::write(&f, b"original").unwrap();
    fs::hard_link(&f, &alias).unwrap();
    assert_success(&script(&s, &root, "printf changed > f"));
    let id = last_id(&s);
    let origin = before(&s, id);
    let actual = post(&s, id);
    let old_ino = fs::metadata(&f).unwrap().ino();
    fs::remove_file(&f).unwrap();
    fs::remove_file(&alias).unwrap();
    let mut reused = false;
    for attempt in 0..4096 {
        let candidate = root.join(format!("candidate-{attempt}"));
        fs::write(&candidate, b"new unrelated work").unwrap();
        if fs::metadata(&candidate).unwrap().ino() == old_ino {
            fs::rename(candidate, &alias).unwrap();
            reused = true;
            eprintln!("inode {old_ino} reused after {} attempts", attempt + 1);
            break;
        }
        fs::remove_file(candidate).unwrap();
    }
    if !reused {
        eprintln!("SKIP: inode {old_ino} was not reused within 4096 attempts");
        return;
    }
    fs::write(&f, b"changed").unwrap();
    let modified = std::time::UNIX_EPOCH
        + std::time::Duration::new(
            actual["mtime"].as_u64().unwrap(),
            actual["mtime_ns"].as_u64().unwrap() as u32,
        );
    fs::File::open(&f)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    lineage(&s, &alias, origin, actual);
    let snapshot = tree_hash(&root);
    let result = undo(&s, id);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("cannot verify alias"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_tree(&snapshot, &root);
    assert_eq!(fs::read(alias).unwrap(), b"new unrelated work");
}

#[test]
fn missing_incarnation_in_old_lineage_refuses_discovered_alias_repair() {
    for field in ["origin", "actual", "journal"] {
        let (s, root) = capture_fixture("alias-legacy");
        fs::write(root.join("f"), b"original").unwrap();
        fs::hard_link(root.join("f"), root.join("alias")).unwrap();
        assert_success(&script(&s, &root, "printf changed > f"));
        let id = last_id(&s);
        let mut origin = before(&s, id);
        let mut actual = post(&s, id);
        if field == "origin" {
            origin.as_object_mut().unwrap().remove("incarnation");
        } else if field == "actual" {
            actual.as_object_mut().unwrap().remove("incarnation");
        } else {
            let log = home(&s).join(format!("cmd/{id}/effects.log"));
            let text = fs::read_to_string(&log).unwrap();
            let mut fields: Vec<String> = text.trim_end().split('\t').map(str::to_owned).collect();
            let mut note: Value = serde_json::from_str(&fields[8]).unwrap();
            note["before"]
                .as_object_mut()
                .unwrap()
                .remove("incarnation");
            fields[8] = serde_json::to_string(&note).unwrap();
            fs::write(log, format!("{}\n", fields.join("\t"))).unwrap();
        }
        lineage(&s, &root.join("alias"), origin, actual);
        let snapshot = tree_hash(&root);
        let result = undo(&s, id);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("cannot verify alias"));
        assert_tree(&snapshot, &root);
    }
}

#[test]
fn ext4_identity_records_birth_time_and_generation_when_supported() {
    let (s, root) = capture_fixture("alias-marker-probe");
    fs::write(root.join("f"), b"original").unwrap();
    assert_success(&script(&s, &root, "printf changed > f"));
    let state = before(&s, last_id(&s));
    eprintln!("incarnation probe: {}", state["incarnation"]);
    if state["incarnation"]["birth"].is_null() && state["incarnation"]["generation"].is_null() {
        eprintln!("SKIP: this filesystem exposes neither birth time nor generation");
        return;
    }
    assert_success(&undo(&s, last_id(&s)));
    assert_recovery_data_only(&s);
}

#[test]
fn legacy_control_alias_entries_are_removed_on_undo() {
    let (s, root) = capture_fixture("alias-control-map");
    fs::write(root.join("f"), b"original").unwrap();
    assert_success(&script(&s, &root, "printf changed > f"));
    let id = last_id(&s);
    let origin = before(&s, id);
    let folder = home(&s).join("q/main/undo-999-test");
    fs::create_dir(&folder).unwrap();
    let control = folder.join("state.log");
    fs::write(&control, b"do not touch this bookkeeping").unwrap();
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(control.as_os_str().as_bytes()).unwrap();
    let stat = sys::stat_at(libc::AT_FDCWD, &name).unwrap().unwrap();
    let incarnation = sys::incarnation_at(libc::AT_FDCWD, &name, &stat);
    let actual = json!({"mode":stat.st_mode,"dev":stat.st_dev,"ino":stat.st_ino,"size":stat.st_size,"mtime":stat.st_mtime,"mtime_ns":stat.st_mtime_nsec,"incarnation":incarnation});
    lineage(&s, &control, origin, actual.clone());
    // Even a bogus registry entry cannot grant a bookkeeping filename a data role.
    fs::write(
        home(&s).join("undo-data-handles.json"),
        serde_json::to_vec(&json!({encoded_path(&control):actual})).unwrap(),
    )
    .unwrap();
    assert_success(&undo(&s, id));
    assert_eq!(fs::read(control).unwrap(), b"do not touch this bookkeeping");
    assert_recovery_data_only(&s);
}

#[test]
fn file_handle_generation_matches_the_pinned_ioctl() {
    let (s, root) = capture_fixture("alias-handle-generation");
    let file = root.join("file");
    fs::write(&file, b"original").unwrap();
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    let (stat, _, birth) = sys::stat_capture_at(libc::AT_FDCWD, &name).unwrap();
    let stat = stat.unwrap();
    let pinned = sys::incarnation_at(libc::AT_FDCWD, &name, &stat);
    let Some(generation) = sys::generation_at(libc::AT_FDCWD, &name, &stat) else {
        eprintln!("SKIP: filesystem does not expose the generic inode/generation file handle");
        return;
    };
    eprintln!("stat birth={birth:?}; handle generation={generation}; ioctl={pinned:?}");
    assert_eq!(birth, pinned.birth);
    assert_eq!(Some(generation), pinned.generation);
    assert!(sys::generation_at(libc::AT_FDCWD, c"/no-such-saqta-generation-file", &stat).is_none());
    drop(s);
}

#[test]
fn delete_capture_does_not_emit_file_open_events() {
    let (s, root) = capture_fixture("alias-no-open-events");
    if !python(&s) {
        return;
    }
    let text = r#"
import ctypes,os,pathlib,struct,subprocess,sys
libc=ctypes.CDLL(None,use_errno=True)
for i in range(20):pathlib.Path(f'f{i}').write_bytes(b'original')
fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
if fd<0:print('SKIP: inotify unavailable',ctypes.get_errno());sys.exit(0)
try:
    if libc.inotify_add_watch(fd,b'.',0x20)<0:
        print('SKIP: cannot watch fixture',ctypes.get_errno());sys.exit(0)
    subprocess.run([sys.argv[1],'exec','--',sys.executable,'-c',"import os\nfor i in range(20):os.unlink(f'f{i}')"],check=True)
    opens=[]
    while True:
        try:data=os.read(fd,65536)
        except BlockingIOError:break
        offset=0
        while offset<len(data):
            wd,mask,cookie,length=struct.unpack_from('iIII',data,offset)
            name=data[offset+16:offset+16+length].split(b'\0')[0];offset+=16+length
            if mask&0x20 and not mask&0x40000000 and name.startswith(b'f'):opens.append(name)
    print('capture victim IN_OPEN events:',len(opens))
    assert not opens,opens
finally:os.close(fd)
"#;
    let mut command = s.command("python3");
    command.args(["-c", text, BIN]).current_dir(&root);
    let result = run(command);
    note(String::from_utf8_lossy(&result.stdout));
    assert_success(&result);
}

#[test]
fn quarantine_pin_is_completed_before_undo_consumes_it() {
    let (s, root) = capture_fixture("alias-lazy-pin");
    fs::write(root.join("file"), b"original").unwrap();
    assert_success(&script(&s, &root, "rm file"));
    let command = home(&s).join("cmd/1");
    let line = fs::read_to_string(command.join("effects.log")).unwrap();
    let fields: Vec<_> = line.trim_end().split('\t').collect();
    let detail: Value = serde_json::from_str(fields[8]).unwrap();
    assert_eq!(detail["pinned"], true);
    assert!(detail["before"]["incarnation"]["generation"].is_null());
    let completed = before(&s, 1);
    assert!(!completed["incarnation"]["generation"].is_null());
    assert!(
        !command.join("inode-pins.json").exists(),
        "inspection must not consume or persist the pin"
    );
    assert_success(&undo(&s, 1));
    assert!(command.join("inode-pins.json").exists());
    assert_eq!(before(&s, 1), completed);
    assert_eq!(fs::read(root.join("file")).unwrap(), b"original");
    assert_recovery_data_only(&s);
}

#[test]
fn freeze_persists_the_original_generation_before_replacing_a_pin() {
    let (s, root) = capture_fixture("alias-freeze-pin");
    fs::write(root.join("file"), b"original").unwrap();
    fs::hard_link(root.join("file"), root.join("alias")).unwrap();
    let original_inode = fs::metadata(root.join("file")).unwrap().ino();
    assert_success(&script(&s, &root, "rm file; printf replacement > alias"));
    let pins: Value =
        serde_json::from_slice(&fs::read(home(&s).join("cmd/1/inode-pins.json")).unwrap()).unwrap();
    assert_eq!(pins.as_object().unwrap().len(), 1);
    let marker = pins.as_object().unwrap().values().next().unwrap();
    assert_eq!(marker["ino"], original_inode);
    assert!(!marker["incarnation"]["generation"].is_null());
    let name = pins.as_object().unwrap().keys().next().unwrap();
    assert_ne!(
        fs::metadata(home(&s).join("q/main/1").join(name))
            .unwrap()
            .ino(),
        original_inode
    );
    assert_eq!(before(&s, 1)["incarnation"], marker["incarnation"]);
    assert_success(&undo(&s, 1));
    assert_eq!(fs::read(root.join("alias")).unwrap(), b"original");
    assert_eq!(fs::read(root.join("file")).unwrap(), b"original");
    assert_recovery_data_only(&s);
}

#[test]
fn gc_persists_generation_before_collecting_a_quarantine_pin() {
    let (s, root) = capture_fixture("alias-gc-pin");
    fs::create_dir(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/file"), b"generated").unwrap();
    assert_success(&script(&s, &root, "rm node_modules/file"));
    let completed = before(&s, 1);
    for _ in 0..3 {
        assert_success(&s.run(&["--", "true"]));
    }
    let meta_path = home(&s).join("cmd/1/meta");
    let mut meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["start_ns"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            - 2 * 86_400_000_000_000
    );
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    assert_success(&run(s.saqta(&["gc", "--older-than", "365d"])));
    assert!(home(&s).join("cmd/1/inode-pins.json").exists());
    assert_eq!(fs::read_dir(home(&s).join("q/main/1")).unwrap().count(), 0);
    let effect = shown(&s, 1)["effects"][0].clone();
    assert_eq!(effect["op"], "expired");
    assert_eq!(effect["note"]["before"], completed);
}

#[test]
fn unavailable_handle_generation_keeps_versions_and_refuses_alias_repair() {
    let (mut s, root) = capture_fixture("alias-unavailable-generation");
    s.env
        .insert("SAQTA_TEST_NO_FILE_HANDLES".into(), "1".into());
    fs::write(root.join("file"), b"original").unwrap();
    fs::hard_link(root.join("file"), root.join("alias")).unwrap();
    assert_success(&script(&s, &root, "printf replacement > file"));
    let origin = before(&s, 1);
    assert!(
        !origin["incarnation"]["generation"].is_null(),
        "the already-open copy descriptor still provides generation"
    );
    let observed = post(&s, 1);
    assert!(observed["incarnation"]["generation"].is_null());
    lineage(&s, &root.join("alias"), origin, observed);
    let snapshot = tree_hash(&root);
    let result = undo(&s, 1);
    assert_eq!(result.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&result.stderr).contains("paths changed"));
    assert_tree(&snapshot, &root);
    assert!(
        fs::read_dir(home(&s).join("q/main/1"))
            .unwrap()
            .any(|entry| fs::read(entry.unwrap().path()).unwrap() == b"original")
    );
}

#[test]
fn unavailable_generation_cannot_authorize_birth_time_only_lineage() {
    let (mut s, root) = capture_fixture("alias-unverified-birth");
    if !python(&s) {
        return;
    }
    s.env
        .insert("SAQTA_TEST_NO_FILE_HANDLES".into(), "1".into());
    fs::write(root.join("file"), b"original").unwrap();
    fs::hard_link(root.join("file"), root.join("alias")).unwrap();
    assert_success(&script(
        &s,
        &root,
        "python3 -c 'import os; os.truncate(\"file\", 64)'",
    ));
    let origin = before(&s, 1);
    assert!(origin["incarnation"]["generation"].is_null());
    assert_eq!(origin["incarnation"]["unverified"], true);
    // Even verified current bytes cannot complete an unpinned historical
    // identity. This lineage must not be admitted using birth time alone.
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(root.join("alias").as_os_str().as_bytes()).unwrap();
    let stat = sys::stat_at(libc::AT_FDCWD, &name).unwrap().unwrap();
    let mut actual = post(&s, 1);
    actual["incarnation"] =
        serde_json::to_value(sys::incarnation_at(libc::AT_FDCWD, &name, &stat)).unwrap();
    lineage(&s, &root.join("alias"), origin, actual);
    let snapshot = tree_hash(&root);
    let result = undo(&s, 1);
    assert_eq!(result.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot verify alias"));
    assert_tree(&snapshot, &root);
}

#[test]
fn failed_unlink_does_not_mark_its_pinned_inode_as_deleted() {
    let (s, root) = capture_fixture("alias-failed-pinned-unlink");
    if !python(&s) {
        return;
    }
    let file = root.join("file");
    fs::write(&file, b"original").unwrap();
    let original_inode = fs::metadata(&file).unwrap().ino();
    let program = "import ctypes,errno; c=ctypes.CDLL(None,use_errno=True); assert c.unlinkat(-100,b'file',0x40000000)==-1; assert ctypes.get_errno()==errno.EINVAL";
    let mut command = s.saqta(&["exec", "--", "python3", "-c", program]);
    command.current_dir(&root);
    assert_success(&run(command));
    assert_eq!(
        fs::read_to_string(home(&s).join("cmd/1/outcomes.log")).unwrap(),
        "1\t0\n"
    );
    assert_success(&undo(&s, 1));
    assert_eq!(fs::metadata(&file).unwrap().ino(), original_inode);
    assert_eq!(fs::read(file).unwrap(), b"original");
    assert_recovery_data_only(&s);
}

#[test]
fn unavailable_generation_leaves_a_rename_outcome_unknown_and_keeps_versions() {
    let (mut s, root) = capture_fixture("alias-unverified-rename");
    if !python(&s) {
        return;
    }
    s.env
        .insert("SAQTA_TEST_NO_FILE_HANDLES".into(), "1".into());
    fs::write(root.join("source"), b"source version").unwrap();
    fs::write(root.join("dest"), b"destination version").unwrap();
    let original = tree_hash(&root);
    assert_success(&script(
        &s,
        &root,
        "python3 -c 'import os; os.replace(\"source\", \"dest\")'",
    ));
    assert_eq!(
        shown(&s, 1)["effects"][1]["note"]["before"]["incarnation"]["unverified"],
        true
    );
    assert_eq!(
        fs::read_to_string(home(&s).join("cmd/1/outcomes.log")).unwrap(),
        "",
        "unknown identity cannot prove the rename did not happen"
    );
    let result = undo(&s, 1);
    if result.status.success() {
        assert_tree(&original, &root);
    } else {
        assert_eq!(result.status.code(), Some(125));
    }
    let mut pending = vec![root, home(&s).join("q")];
    let mut versions = std::collections::BTreeSet::new();
    while let Some(folder) = pending.pop() {
        for entry in fs::read_dir(folder).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                versions.insert(fs::read(entry.path()).unwrap());
            }
        }
    }
    assert!(versions.contains(b"source version".as_slice()));
    assert!(versions.contains(b"destination version".as_slice()));
    assert_recovery_data_only(&s);
}
