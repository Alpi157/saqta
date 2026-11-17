#![cfg(debug_assertions)]
mod common;
use common::*;
#[path = "../src/harness_ops.rs"]
mod ops;
use ops::{Batch, Op};
use proptest::{
    prelude::*,
    test_runner::{Config, FileFailurePersistence},
};
use std::{fs, path::Path};

fn operation() -> impl Strategy<Value = Op> {
    (
        0u8..15,
        0u8..18,
        0u8..18,
        any::<u8>(),
        proptest::collection::vec(any::<u8>(), 0..24),
    )
        .prop_map(|(kind, a, b, n, data)| Op {
            kind,
            a,
            b,
            n,
            data,
        })
}
fn config() -> Config {
    Config {
        cases: std::env::var("SAQTA_HARNESS_CASES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "tests/harness.proptest-regressions",
        ))),
        ..Config::default()
    }
}
fn seed(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for i in 0..6 {
        fs::create_dir(root.join(format!("d{i}"))).unwrap();
        for j in 0..if i < 3 {
            2
        } else if i < 5 {
            1
        } else {
            0
        } {
            fs::write(
                root.join(format!("d{i}/f{j}")),
                format!("original {i} {j}\0"),
            )
            .unwrap();
        }
    }
    fs::remove_file(root.join("d2/f1")).unwrap();
    fs::hard_link(root.join("d1/f0"), root.join("d2/f1")).unwrap();
}
fn execute(s: &Sandbox, root: &Path, processes: Vec<Vec<Op>>) -> u64 {
    let batch = Batch { processes };
    fs::write(
        s.dir.join("operations.json"),
        serde_json::to_vec_pretty(&batch).unwrap(),
    )
    .unwrap();
    let mut c = s.exec(&["--", BIN, "__harness"]);
    c.arg(root);
    let out = Running::spawn(c, Some(&serde_json::to_vec(&batch).unwrap())).output();
    assert_success(&out);
    // Keep the syscall results alongside a failing fixture for diagnosis.
    fs::write(s.dir.join("results.txt"), &out.stdout).unwrap();
    last_id(s)
}
fn check_sequence(ops: Vec<Op>, splits: usize) {
    let (s, root) = capture_fixture("harness-sequence");
    seed(&root);
    let before = tree_hash(&root);
    fs::write(
        s.dir.join("case.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"ops":ops,"splits":splits})).unwrap(),
    )
    .unwrap();
    let splits = splits.min(ops.len());
    let mut ids = Vec::new();
    let mut snapshots = vec![before];
    for i in 0..splits {
        let chunk = &ops[ops.len() * i / splits..ops.len() * (i + 1) / splits];
        ids.push(execute(&s, &root, vec![chunk.to_vec()]));
        snapshots.push(tree_hash(&root));
    }
    for (i, id) in ids.into_iter().enumerate().rev() {
        assert_success(&undo(&s, id));
        assert_tree(&snapshots[i], &root);
    }
}
fn versions(root: &Path) -> Vec<Vec<u8>> {
    fn walk(p: &Path, out: &mut Vec<Vec<u8>>) {
        let Ok(m) = fs::symlink_metadata(p) else {
            return;
        };
        if m.is_file() {
            out.push(fs::read(p).unwrap());
        } else if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}
fn check_interrupted(ops: Vec<Op>, stop: usize) -> usize {
    let (s, root) = capture_fixture("harness-interrupted");
    seed(&root);
    let before = tree_hash(&root);
    fs::write(
        s.dir.join("case.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"ops":ops,"stop":stop})).unwrap(),
    )
    .unwrap();
    let baseline = execute(&s, &root, vec![ops.clone()]);
    assert_success(&undo(&s, baseline));
    assert_tree(&before, &root);
    let uninterrupted = tree_hash(&root);
    let mut all = versions(&root);
    let id = execute(&s, &root, vec![ops]);
    all.extend(versions(&root));
    let mut c = s.saqta(&["undo", &id.to_string()]);
    c.env("SAQTA_TEST_ABORT_AFTER", stop.to_string());
    let result = run(c);
    assert!(
        result.status.success() || result.status.code() == Some(86),
        "unexpected undo failure: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let mut kept = versions(&root);
    kept.extend(versions(&home.join(format!("q/main/{id}"))));
    for entry in fs::read_dir(home.join("q/main")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&format!("undo-{id}-")))
        {
            kept.extend(versions(&entry.path()));
        }
    }
    for content in all {
        assert!(kept.contains(&content), "missing version: {:?}", content);
    }
    if !result.status.success() {
        assert_success(&run(s.saqta(&["undo", &id.to_string(), "--resume"])));
    }
    assert_tree(&uninterrupted, &root);
    let folder = fs::read_dir(home.join("q/main"))
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&format!("undo-{id}-")))
        })
        .unwrap();
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(folder.path().join("state.json")).unwrap()).unwrap();
    state["steps"].as_u64().unwrap() as usize
}
fn op(kind: u8, a: u8, b: u8) -> Op {
    Op {
        kind,
        a,
        b,
        n: 0,
        data: vec![],
    }
}
proptest! {
    #![proptest_config(config())]
    #[test]
    fn consecutive_commands_restore_every_snapshot(ops in proptest::collection::vec(operation(),1..=40), splits in 1usize..=3) { check_sequence(ops,splits); }
    #[test]
    fn concurrent_subtrees_restore_exactly(processes in proptest::collection::vec(proptest::collection::vec(operation(),1..=40),2..=4)) {
        let (s,root)=capture_fixture("harness-parallel");
        for i in 0..processes.len() { seed(&root.join(format!("p{i}"))); }
        let before=tree_hash(&root);
        let id=execute(&s,&root,processes);
        assert_success(&undo(&s,id)); assert_tree(&before,&root);
    }
    #[test]
    fn interrupted_undo_preserves_versions_and_resumes(ops in proptest::collection::vec(operation(),1..=40), stop in 1usize..=20) {
        check_interrupted(ops,stop);
    }

}

#[test]
fn regression_pure_append_is_undone() {
    check_sequence(
        vec![Op {
            kind: 3,
            a: 6,
            b: 0,
            n: 0,
            data: vec![0],
        }],
        1,
    );
}

#[test]
fn regression_failed_directory_rename_then_child_write() {
    check_sequence(
        vec![
            Op {
                kind: 7,
                a: 0,
                b: 1,
                n: 0,
                data: vec![],
            },
            Op {
                kind: 1,
                a: 6,
                b: 0,
                n: 0,
                data: vec![],
            },
        ],
        2,
    );
}

#[test]
fn regression_exchange_then_remove_a_created_input() {
    check_sequence(
        vec![op(9, 13, 0), op(0, 13, 0), op(8, 13, 5), op(9, 5, 0)],
        1,
    );
}
#[test]
fn regression_write_then_grow_then_write_does_not_restore_an_intermediate_size() {
    let mut first = op(1, 6, 0);
    first.data = vec![0; 14];
    check_sequence(vec![first, op(5, 6, 0), op(1, 6, 0)], 1);
}
#[test]
fn regression_grown_prefix_survives_interrupted_undo() {
    check_interrupted(vec![op(5, 6, 0), op(1, 6, 0), op(14, 6, 0)], 1);
}
#[test]
fn regression_grown_file_moves_with_exchanged_directory() {
    check_sequence(vec![op(5, 10, 0), op(8, 3, 2)], 1);
}
#[test]
fn regression_growth_then_a_later_command_replaces_the_inode() {
    check_sequence(vec![op(5, 7, 0), op(1, 7, 0)], 2);
}
#[test]
fn regression_removed_directory_replaced_by_a_symlink() {
    check_sequence(vec![op(9, 14, 0), op(10, 4, 0), op(13, 4, 0)], 1);
}
#[test]
fn regression_moved_parent_replaced_by_a_symlink_loop() {
    check_sequence(vec![op(7, 7, 6), op(7, 0, 15), op(13, 0, 0)], 1);
}
#[test]
fn regression_exchange_a_symlink_over_a_touched_parent() {
    check_interrupted(vec![op(13, 15, 0), op(7, 15, 6), op(8, 6, 4)], 1);
}
#[test]
fn regression_write_new_hard_link_after_original_name_is_exchanged() {
    check_sequence(vec![op(12, 11, 17), op(8, 11, 0), op(1, 17, 0)], 1);
}
#[test]
fn regression_growth_through_new_hard_link_then_replace_original_name() {
    check_sequence(
        vec![op(12, 7, 16), op(5, 16, 0), op(7, 8, 7), op(14, 16, 0)],
        1,
    );
}
#[test]
fn resume_replays_every_step_of_shared_inode_repair_and_exchange() {
    let mut write = op(1, 6, 0);
    write.data = b"post version".to_vec();
    let operations = vec![op(12, 6, 16), write, op(8, 6, 7), op(9, 7, 0)];
    let steps = check_interrupted(operations.clone(), 1);
    for stop in 2..=steps {
        check_interrupted(operations.clone(), stop);
    }
}
#[test]
fn resume_replays_deletes_with_deferred_completion_checkpoints() {
    for stop in 1..=3 {
        let (s, root) = capture_fixture("resume-deletes");
        for name in ["a", "b", "c"] {
            fs::write(root.join(name), format!("original {name}")).unwrap();
        }
        let before = tree_hash(&root);
        assert_success(&script(&s, &root, "rm a b c"));
        let id = last_id(&s);
        let mut command = s.saqta(&["undo", &id.to_string()]);
        command.env("SAQTA_TEST_ABORT_AFTER", stop.to_string());
        assert_eq!(run(command).status.code(), Some(86));
        assert_success(&run(s.saqta(&["undo", &id.to_string(), "--resume"])));
        assert_tree(&before, &root);
    }
}
#[test]
fn syscall_helper_exercises_creation_and_write_through_symlink() {
    let (s, root) = capture_fixture("harness-driver");
    seed(&root);
    let before = tree_hash(&root);
    let mut write = op(14, 15, 0);
    write.data = b"through link".to_vec();
    let id = execute(&s, &root, vec![vec![op(13, 15, 6), write, op(0, 16, 0)]]);
    assert_eq!(fs::read(root.join("d0/f0")).unwrap(), b"through link");
    assert!(
        fs::symlink_metadata(root.join("d4/f1"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(root.join("d5/f0").is_file());
    assert_success(&undo(&s, id));
    assert_tree(&before, &root);
}

#[test]
fn regression_grow_delete_and_write_a_preexisting_alias() {
    check_sequence(vec![op(5, 8, 0), op(9, 8, 0), op(1, 11, 0)], 1);
}

#[test]
fn regression_repeated_write_after_alias_deletion_repairs_both_names() {
    check_sequence(vec![op(1, 8, 0), op(9, 11, 0), op(1, 8, 0)], 1);
}
#[test]
fn regression_repair_alias_after_rename_and_later_write() {
    check_sequence(
        vec![op(1, 11, 0), op(7, 11, 17), op(9, 8, 0), op(1, 17, 0)],
        1,
    );
}

#[test]
fn regression_alias_lineage_survives_undo_of_a_newer_command() {
    check_sequence(
        vec![
            op(1, 8, 0),
            op(0, 0, 0),
            op(0, 0, 0),
            op(0, 0, 0),
            op(0, 0, 0),
            op(9, 8, 0),
            op(14, 11, 0),
        ],
        2,
    );
}
#[test]
fn regression_rename_over_alias_then_equal_length_truncate() {
    check_sequence(vec![op(1, 8, 0), op(7, 6, 8), op(4, 11, 0)], 1);
}

#[test]
fn undo_and_dry_run_print_only_nonzero_categories() {
    let (s, root) = capture_fixture("harness-summary");
    seed(&root);
    let mut append = op(3, 6, 0);
    append.data = vec![0];
    let id = execute(&s, &root, vec![vec![append]]);
    for dry in [true, false] {
        let mut c = s.saqta(&["undo", &id.to_string()]);
        if dry {
            c.arg("--dry-run");
        }
        let result = run(c);
        assert_success(&result);
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains(if dry {
            "would restore modified files: 1"
        } else {
            "restored modified files: 1"
        }));
        assert!(!text.lines().any(|l| l.ends_with(": 0")), "{text}");
        for zero in [
            "deleted files:",
            "created paths:",
            "recreate directories:",
            "recreated directories:",
            "reverse renames:",
            "reversed renames:",
        ] {
            assert!(!text.contains(zero), "{text}");
        }
    }
}

#[test]
fn preserved_created_hard_link_is_immutable_after_later_write() {
    let (s, root) = capture_fixture("harness-immutable-post");
    seed(&root);
    let id = execute(&s, &root, vec![vec![op(12, 6, 16)]]);
    assert_success(&undo(&s, id));
    let mut write = op(1, 6, 0);
    write.data = b"later write".to_vec();
    execute(&s, &root, vec![vec![write]]);
    let home = std::path::PathBuf::from(&s.env[std::ffi::OsStr::new("SAQTA_HOME")]);
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(home.join("q/main")).unwrap() {
        let entry = entry.unwrap();
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&format!("undo-{id}-")))
        {
            for version in fs::read_dir(entry.path()).unwrap() {
                let version = version.unwrap();
                if version
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u64>().is_ok())
                {
                    snapshots.push(fs::read(version.path()).unwrap());
                }
            }
        }
    }
    assert!(snapshots.contains(&b"original 0 0\0".to_vec()));
}
