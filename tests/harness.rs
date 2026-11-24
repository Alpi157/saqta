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
        assert_recovery_data_only(&s);
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
    assert_recovery_data_only(&s);
    assert_tree(&before, &root);
    let uninterrupted = tree_hash(&root);
    let mut all = versions(&root);
    let id = execute(&s, &root, vec![ops]);
    all.extend(versions(&root));
    let mut c = s.saqta(&["undo", &id.to_string()]);
    c.env("SAQTA_TEST_ABORT_AFTER", stop.to_string());
    let result = run(c);
    assert_recovery_data_only(&s);
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
        assert_recovery_data_only(&s);
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
fn op_with_data(kind: u8, a: u8, b: u8, data: &[u8]) -> Op {
    let mut operation = op(kind, a, b);
    operation.data = data.to_vec();
    operation
}
fn check_parallel(processes: Vec<Vec<Op>>) {
    let (s, root) = capture_fixture("harness-parallel");
    for i in 0..processes.len() {
        seed(&root.join(format!("p{i}")));
    }
    let before = tree_hash(&root);
    let id = execute(&s, &root, processes);
    assert_success(&undo(&s, id));
    assert_recovery_data_only(&s);
    assert_tree(&before, &root);
}

#[test]
fn undo_exchange_after_later_undos_repair_a_child_alias() {
    let mut ops = vec![op(8, 5, 2)];
    ops.extend((0..7).map(|_| op(0, 0, 0)));
    ops.push(op(1, 8, 0));
    ops.extend((0..5).map(|_| op(0, 0, 0)));
    ops.push(op(1, 17, 0));
    check_sequence(ops, 3);
}

#[test]
fn alias_preflight_accepts_names_moved_by_a_directory_exchange() {
    check_interrupted(vec![op(8, 2, 1), op(1, 8, 0)], 1);
}

#[test]
fn alias_observations_follow_an_intervening_directory_undo() {
    check_sequence(vec![op(1, 8, 0), op(8, 0, 2), op(1, 7, 0)], 3);
}

#[test]
fn resumed_alias_repair_records_the_parent_and_still_refuses_later_edits() {
    for edit in [false, true] {
        let (s, root) = capture_fixture("alias-parent-resume");
        seed(&root);
        let before = tree_hash(&root);
        let older = execute(&s, &root, vec![vec![op(8, 5, 2)]]);
        let after_exchange = tree_hash(&root);
        let middle = execute(&s, &root, vec![vec![op(1, 8, 0)]]);
        let newer = execute(&s, &root, vec![vec![op(1, 17, 0)]]);
        assert_success(&undo(&s, newer));
        let mut command = s.saqta(&["undo", &middle.to_string()]);
        command.env("SAQTA_TEST_ABORT_AFTER", "1");
        assert_eq!(run(command).status.code(), Some(86));
        assert_success(&run(s.saqta(&["undo", &middle.to_string(), "--resume"])));
        assert_tree(&after_exchange, &root);
        if edit {
            let parent = root.join("d5");
            fs::write(parent.join("user-work"), b"keep this").unwrap();
            fs::File::open(parent)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
                .unwrap();
            let edited = tree_hash(&root);
            let result = undo(&s, older);
            assert!(!result.status.success());
            assert!(String::from_utf8_lossy(&result.stderr).contains("paths changed"));
            assert_tree(&edited, &root);
        } else {
            assert_success(&undo(&s, older));
            assert_tree(&before, &root);
        }
    }
}

#[test]
fn undo_truncation_through_new_link_restores_surviving_alias() {
    check_parallel(vec![
        vec![op(12, 8, 15), op(9, 8, 0), op(14, 15, 0)],
        vec![op(0, 0, 0)],
    ]);
}

#[test]
fn undo_repairs_surviving_alias_even_after_the_truncated_link_is_removed() {
    check_parallel(vec![
        vec![op(12, 8, 15), op(9, 8, 0), op(14, 15, 0), op(9, 15, 0)],
        vec![op(0, 0, 0)],
    ]);
}

#[test]
fn each_undo_refreshes_the_alias_lineage_for_the_next_undo() {
    let mut overwrite = op(1, 11, 0);
    overwrite.data = b"middle version".to_vec();
    check_sequence(
        vec![
            op(1, 11, 0),
            op(0, 0, 0),
            overwrite,
            op(0, 0, 0),
            op(9, 11, 0),
            op(1, 8, 0),
        ],
        3,
    );
}

proptest! {
    #![proptest_config(config())]
    #[test]
    fn consecutive_commands_restore_every_snapshot(ops in proptest::collection::vec(operation(),1..=40), splits in 1usize..=3) { check_sequence(ops,splits); }
    #[test]
    fn concurrent_subtrees_restore_exactly(processes in proptest::collection::vec(proptest::collection::vec(operation(),1..=40),2..=4)) {
        check_parallel(processes);
    }
    #[test]
    fn interrupted_undo_preserves_versions_and_resumes(ops in proptest::collection::vec(operation(),1..=40), stop in 1usize..=20) {
        check_interrupted(ops,stop);
    }

}

// Run 44513919850, Ubuntu 24.04, run 3: exactly 34 operations and stop = 14.
// SAQTA_INODE_REUSE_ITERATIONS controls the loop; retain every failing fixture.
#[test]
#[ignore = "timing-sensitive inode reuse stress test"]
fn interrupted_undo_inode_reuse_stress() {
    let replay_op = |kind, a, b, n, data: &[u8]| Op {
        kind,
        a,
        b,
        n,
        data: data.to_vec(),
    };
    let ops = vec![
        replay_op(
            4,
            11,
            15,
            70,
            &[
                215, 113, 53, 19, 194, 93, 79, 179, 222, 204, 37, 152, 133, 203, 84, 251, 138, 91,
                20, 154, 145, 249,
            ],
        ),
        replay_op(6, 9, 11, 183, &[38]),
        replay_op(
            12,
            6,
            16,
            184,
            &[
                144, 220, 104, 91, 190, 203, 199, 51, 66, 99, 11, 38, 15, 197, 196, 43, 139, 111,
            ],
        ),
        replay_op(0, 7, 5, 63, &[143, 192]),
        replay_op(
            12,
            1,
            7,
            103,
            &[
                38, 84, 241, 219, 246, 30, 64, 53, 245, 99, 136, 110, 197, 160, 55, 64, 9, 153,
            ],
        ),
        replay_op(1, 12, 6, 211, &[16, 119, 30, 218]),
        replay_op(
            5,
            13,
            13,
            69,
            &[99, 182, 252, 3, 89, 54, 200, 227, 183, 9, 192, 255, 248],
        ),
        replay_op(
            5,
            8,
            8,
            4,
            &[
                188, 139, 105, 157, 187, 57, 155, 110, 215, 18, 75, 198, 212, 160, 36, 171,
            ],
        ),
        replay_op(
            5,
            0,
            17,
            238,
            &[
                131, 179, 142, 158, 168, 137, 217, 86, 171, 148, 243, 194, 106, 74,
            ],
        ),
        replay_op(
            2,
            7,
            1,
            100,
            &[
                106, 21, 19, 194, 141, 232, 222, 184, 3, 117, 134, 1, 161, 49, 30, 172,
            ],
        ),
        replay_op(
            7,
            0,
            1,
            215,
            &[
                189, 160, 178, 204, 208, 140, 108, 243, 252, 248, 246, 16, 78, 75, 73, 49, 133,
            ],
        ),
        replay_op(
            3,
            1,
            1,
            225,
            &[
                236, 101, 75, 11, 95, 51, 156, 132, 245, 15, 83, 112, 234, 207,
            ],
        ),
        replay_op(
            8,
            5,
            9,
            163,
            &[
                89, 81, 145, 80, 62, 3, 228, 79, 241, 158, 177, 26, 11, 236, 77, 87, 232, 238, 143,
                140,
            ],
        ),
        replay_op(4, 9, 8, 188, &[3, 104, 121, 200, 131, 5, 207, 162]),
        replay_op(
            1,
            17,
            5,
            119,
            &[
                212, 237, 140, 96, 143, 170, 4, 216, 4, 45, 96, 43, 23, 70, 192, 237,
            ],
        ),
        replay_op(12, 8, 15, 33, &[71, 18, 162, 133, 254, 99, 221, 175]),
        replay_op(
            10,
            3,
            5,
            234,
            &[
                36, 58, 26, 88, 185, 4, 16, 55, 150, 115, 144, 190, 4, 203, 137, 49, 70, 193, 236,
                254,
            ],
        ),
        replay_op(
            9,
            8,
            12,
            196,
            &[
                198, 150, 40, 62, 0, 194, 91, 239, 0, 152, 205, 75, 23, 51, 53, 65, 52, 124, 50, 8,
                159,
            ],
        ),
        replay_op(
            8,
            16,
            4,
            126,
            &[
                130, 210, 153, 5, 56, 178, 181, 212, 161, 115, 155, 118, 121, 159, 114, 157, 24,
                30, 113, 196, 231, 43, 3,
            ],
        ),
        replay_op(
            5,
            4,
            11,
            69,
            &[
                184, 98, 164, 53, 4, 25, 25, 106, 147, 8, 214, 109, 132, 60, 14, 71,
            ],
        ),
        replay_op(4, 2, 9, 81, &[7, 189, 203, 23, 81]),
        replay_op(8, 0, 12, 165, &[253, 248]),
        replay_op(6, 1, 16, 85, &[26, 248, 95]),
        replay_op(4, 9, 0, 68, &[91]),
        replay_op(7, 14, 1, 143, &[20, 243, 186]),
        replay_op(11, 6, 8, 147, &[]),
        replay_op(11, 7, 5, 15, &[31, 214, 206, 105, 205]),
        replay_op(
            14,
            15,
            10,
            56,
            &[
                101, 116, 225, 148, 113, 5, 84, 55, 69, 46, 60, 160, 154, 52, 199, 248, 103, 112,
                91, 225,
            ],
        ),
        replay_op(
            1,
            9,
            15,
            216,
            &[
                126, 92, 201, 89, 0, 46, 137, 49, 187, 87, 7, 83, 240, 239, 51, 74, 157, 32, 242,
                253,
            ],
        ),
        replay_op(
            9,
            15,
            17,
            72,
            &[
                110, 147, 16, 176, 30, 149, 226, 230, 246, 42, 147, 2, 119, 67, 144, 94,
            ],
        ),
        replay_op(
            0,
            7,
            13,
            24,
            &[
                133, 75, 29, 240, 144, 123, 61, 232, 220, 142, 179, 255, 19, 114, 165, 161,
            ],
        ),
        replay_op(7, 13, 17, 227, &[70, 79, 240, 226, 14]),
        replay_op(12, 9, 17, 208, &[164, 53, 190]),
        replay_op(6, 15, 16, 28, &[96, 158, 183, 220]),
    ];
    let iterations: usize = std::env::var("SAQTA_INODE_REUSE_ITERATIONS")
        .map(|count| count.parse().expect("positive iteration count"))
        .unwrap_or(1);
    assert!(iterations > 0);
    let mut failures = 0;
    for iteration in 1..=iterations {
        // Keep counting after a panic so a loaded run measures all iterations.
        if std::panic::catch_unwind(|| check_interrupted(ops.clone(), 14)).is_err() {
            eprintln!("inode reuse stress failed on iteration {iteration}");
            failures += 1;
        }
    }
    eprintln!("inode reuse stress failures: {failures}/{iterations}");
    assert_eq!(failures, 0);
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
fn regression_growth_through_new_hard_link_after_unlinking_first_original() {
    check_sequence(vec![op(12, 8, 15), op(9, 8, 0), op(5, 15, 0)], 1);
}

#[test]
fn regression_growth_through_new_hard_link_after_unlink_resumes() {
    let operations = vec![op(12, 8, 13), op(9, 8, 0), op(5, 13, 0)];
    check_sequence(operations.clone(), 1);
    check_interrupted(operations, 1);
}

#[test]
fn regression_growth_through_new_hard_link_after_rename_over() {
    check_sequence(vec![op(12, 11, 16), op(7, 6, 11), op(5, 16, 0)], 1);
}

#[test]
fn regression_growth_through_new_hard_link_after_unlinking_second_original() {
    check_sequence(vec![op(12, 11, 17), op(9, 11, 0), op(5, 17, 0)], 1);
}

#[test]
fn regression_growth_through_chained_new_links_after_unlink() {
    check_sequence(
        vec![op(12, 8, 15), op(12, 15, 16), op(9, 8, 0), op(5, 16, 0)],
        1,
    );
}

#[test]
fn regression_write_surviving_alias_after_growth_through_new_link() {
    check_sequence(
        vec![
            op(12, 8, 15),
            op(9, 8, 0),
            op(5, 15, 0),
            op_with_data(1, 11, 0, b"later"),
        ],
        1,
    );
}

#[test]
fn regression_delete_new_link_after_growth_through_it() {
    check_sequence(
        vec![op(12, 8, 15), op(9, 8, 0), op(5, 15, 0), op(9, 15, 0)],
        1,
    );
}

#[test]
fn regression_shrink_surviving_alias_after_growth_through_new_link() {
    check_sequence(
        vec![op(12, 8, 15), op(9, 8, 0), op(5, 15, 0), op(4, 11, 0)],
        1,
    );
}

#[test]
fn regression_rename_new_link_over_surviving_alias_after_growth() {
    check_sequence(
        vec![op(12, 8, 15), op(9, 8, 0), op(5, 15, 0), op(7, 15, 11)],
        1,
    );
}

#[test]
fn regression_interrupt_each_step_of_growth_through_new_link() {
    let ops = vec![
        op(12, 8, 15),
        op(9, 8, 0),
        op(5, 15, 0),
        op_with_data(3, 11, 0, b"tail"),
    ];
    for stop in 1..=4 {
        check_interrupted(ops.clone(), stop);
    }
}

fn check_new_hard_link_after_original_removed(kind: u8, data: &[u8]) {
    let change = op_with_data(kind, 15, 0, data);
    check_sequence(vec![op(12, 8, 15), op(9, 8, 0), change.clone()], 1);
    check_sequence(vec![op(12, 8, 15), op(7, 6, 8), change], 1);
}

#[test]
fn regression_shrink_through_new_hard_link_after_original_removed() {
    check_new_hard_link_after_original_removed(4, &[]);
}

#[test]
fn regression_append_through_new_hard_link_after_original_removed() {
    check_new_hard_link_after_original_removed(3, b"append");
}

#[test]
fn regression_truncating_write_through_new_hard_link_after_original_removed() {
    check_new_hard_link_after_original_removed(1, b"replacement");
}

#[test]
fn regression_in_place_write_through_new_hard_link_after_original_removed() {
    check_new_hard_link_after_original_removed(2, b"write");
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
fn regression_directory_observations_follow_exchanges_of_ancestors() {
    let (s, root) = capture_fixture("directory-observation-lineage");
    seed(&root);
    let before = tree_hash(&root);
    let older = execute(&s, &root, vec![vec![op(8, 1, 6)]]);
    let middle = tree_hash(&root);
    let newer = execute(
        &s,
        &root,
        vec![vec![op(8, 4, 0), op(8, 5, 14), op(9, 16, 0)]],
    );
    assert_success(&undo(&s, newer));
    assert_tree(&middle, &root);
    assert_success(&undo(&s, older));
    assert_tree(&before, &root);
    check_sequence(
        vec![op(8, 1, 6), op(8, 4, 0), op(8, 5, 14), op(9, 16, 0)],
        3,
    );
}
#[test]
fn resumed_directory_observations_are_not_exchanged_twice() {
    let (s, root) = capture_fixture("directory-observation-resume");
    seed(&root);
    let before = tree_hash(&root);
    let older = execute(&s, &root, vec![vec![op(8, 1, 6)]]);
    let middle = execute(&s, &root, vec![vec![op(8, 4, 0)]]);
    let newer = execute(&s, &root, vec![vec![op(8, 5, 14), op(9, 16, 0)]]);
    assert_success(&undo(&s, newer));
    let mut command = s.saqta(&["undo", &middle.to_string()]);
    command.env("SAQTA_TEST_ABORT_UNDO_FINALIZE", "1");
    assert_eq!(run(command).status.code(), Some(86));
    assert_success(&run(s.saqta(&["undo", &middle.to_string(), "--resume"])));
    assert_success(&undo(&s, older));
    assert_tree(&before, &root);
}
#[test]
fn restored_directory_observations_reject_later_user_edits() {
    let (s, root) = capture_fixture("directory-observation-user-edit");
    seed(&root);
    let older = execute(&s, &root, vec![vec![op(8, 1, 6)]]);
    let newer = execute(
        &s,
        &root,
        vec![vec![op(8, 4, 0), op(8, 5, 14), op(9, 16, 0)]],
    );
    assert_success(&undo(&s, newer));
    let dir = root.join("d0/f0");
    fs::write(dir.join("user-work"), b"new user data").unwrap();
    fs::File::open(&dir)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let before_refusal = tree_hash(&root);
    let output = undo(&s, older);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("paths changed"));
    assert_tree(&before_refusal, &root);
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
    assert_recovery_data_only(&s);
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
    assert_recovery_data_only(&s);
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

#[test]
fn regression_write_new_alias_after_removing_origin_repairs_surviving_inode() {
    check_interrupted(vec![op(12, 11, 17), op(9, 11, 0), op(1, 17, 0)], 1);
}
