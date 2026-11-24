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
