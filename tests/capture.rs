mod common;
use common::*;
use std::{fs, path::Path};

fn sample(p: &Path) {
    fs::create_dir_all(p.join("tree/nested/empty")).unwrap();
    fs::create_dir(p.join("tree/empty")).unwrap();
    fs::write(p.join("tree/nested/file"), b"original\0bytes\xff\n").unwrap();
}
fn restore_exact(s: &Sandbox, p: &Path, before: &std::collections::BTreeMap<Vec<u8>, u64>) {
    let id = last_id(s);
    let shown = shown(s, id);
    assert!(
        !shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["op"] == "not protected"),
        "{shown}"
    );
    assert_success(&undo(s, id));
    assert_tree(before, p);
}

#[test]
fn creation_then_overwrite_then_delete_needs_no_content() {
    let (s, p) = capture_fixture("ephemeral");
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo first > ephemeral; echo second > ephemeral; rm ephemeral",
    ));
    assert!(!p.join("ephemeral").exists());
    let shown = shown(&s, last_id(&s));
    assert!(
        shown["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["quarantine"] == "")
    );
    restore_exact(&s, &p, &before);
}
#[test]
fn mv_directory_and_later_deletion() {
    let (s, p) = capture_fixture("mv_directory");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "mv tree moved; rm -rf moved/nested"));
    assert!(p.join("moved").exists());
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn renamed_then_deleted_working_directory() {
    let (s, p) = capture_fixture("renamed_cwd");
    sample(&p);
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "cd tree/nested; mv ../../tree ../../moved; rm file; cd -P ..; rmdir nested/empty; rmdir nested; cd -P ..; rmdir moved/empty; rmdir moved",
    ));
    assert!(!p.join("tree").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn failed_rename_does_not_reverse_an_operation_that_never_happened() {
    let (s, p) = capture_fixture("failed_rename");
    fs::create_dir(p.join("a")).unwrap();
    fs::create_dir(p.join("b")).unwrap();
    fs::write(p.join("b/keep"), "keep").unwrap();
    let before = tree_hash(&p);
    let o = script(&s, &p, "mv -T a b");
    assert!(!o.status.success());
    restore_exact(&s, &p, &before);
}
#[test]
fn deleted_hard_link_is_frozen_before_other_alias_is_written() {
    let (s, p) = capture_fixture("freeze_link");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm a; echo changed > b"));
    assert!(!p.join("a").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn previously_created_name_can_later_hold_an_original_file() {
    let (s, p) = capture_fixture("created_name_reused");
    fs::write(p.join("original"), "keep original").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo temporary > reused; rm reused; mv original reused; rm reused",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn rename_over_created_name_then_write_keeps_original_source() {
    let (s, p) = capture_fixture("rename_over_created");
    fs::write(p.join("original"), "original bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "echo temporary > new; mv original new; echo changed > new; rm new",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn deleting_original_then_writing_new_hard_link_keeps_preimage() {
    let (s, p) = capture_fixture("new_link_after_delete");
    fs::write(p.join("original"), "keep bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(
        &s,
        &p,
        "ln original new; rm original; echo changed > new",
    ));
    assert!(!p.join("original").exists());
    restore_exact(&s, &p, &before);
}
#[test]
fn writing_new_hard_link_restores_original_name() {
    let (s, p) = capture_fixture("write_new_link");
    fs::write(p.join("original"), "keep bytes").unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "ln original new; echo changed > new"));
    assert_eq!(fs::read(p.join("original")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn in_place_write_restores_untouched_hard_link_alias() {
    let (s, p) = capture_fixture("untouched_alias");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo changed > a"));
    assert_eq!(fs::read(p.join("b")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn modified_then_deleted_hard_link_restores_untouched_alias() {
    let (s, p) = capture_fixture("modified_deleted_alias");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo changed > a; rm a"));
    assert!(!p.join("a").exists());
    assert_eq!(fs::read(p.join("b")).unwrap(), b"changed\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn two_written_hard_links_restore_both_names() {
    let (s, p) = capture_fixture("both_aliases");
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "echo first > a; echo second > b"));
    assert_eq!(fs::read(p.join("b")).unwrap(), b"second\n");
    restore_exact(&s, &p, &before);
}
#[test]
fn growing_truncate_restores_hard_link_alias_size() {
    let (s, p) = capture_fixture("truncate_alias");
    if !python(&s) {
        return;
    }
    fs::write(p.join("a"), "original").unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    let before = tree_hash(&p);
    let code = "import ctypes; l=ctypes.CDLL(None,use_errno=True); assert l.truncate(b'a',1024)==0";
    let mut c = s.exec(&["--", "python3", "-c", code]);
    c.current_dir(&p);
    assert_success(&run(c));
    assert_eq!(fs::metadata(p.join("b")).unwrap().len(), 1024);
    restore_exact(&s, &p, &before);
}
