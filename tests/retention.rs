mod common;
use common::*;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn home(s: &Sandbox) -> PathBuf {
    PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")])
}
fn age(s: &Sandbox, id: u64, days: u64) {
    let p = home(s).join(format!("cmd/{id}/meta"));
    let mut m: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    m["start_ns"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            - days * 86_400_000_000_000
    );
    fs::write(p, serde_json::to_vec(&m).unwrap()).unwrap();
}
fn pad(s: &Sandbox, p: &Path) {
    for _ in 0..3 {
        assert_success(&script(s, p, "true"));
    }
}
fn gc(s: &Sandbox, args: &[&str]) -> Value {
    let mut c = s.saqta(&["gc", "--json"]);
    c.args(args);
    let o = run(c);
    assert_success(&o);
    serde_json::from_slice(&o.stdout).unwrap()
}
#[test]
fn age_size_dry_run_and_newest_three() {
    let (s, p) = capture_fixture("gc-limits");
    for i in 1u8..=6 {
        fs::write(p.join("source"), vec![i; 4096]).unwrap();
        assert_success(&script(&s, &p, "rm source"));
        age(&s, u64::from(i), 10);
    }
    let before = tree_hash(&home(&s));
    let plan = gc(&s, &["--dry-run", "--older-than", "7d", "--max-size", "0B"]);
    assert_eq!(plan["dry_run"], true);
    assert_tree(&before, &home(&s));
    let done = gc(&s, &["--older-than", "7d", "--max-size", "0B"]);
    assert!(
        done["after_bytes"].as_u64().unwrap() > 0,
        "newest three form a protected floor"
    );
    let kinds: Vec<_> = done["collections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert_eq!(&kinds[..3], &["regenerable"; 3]);
    assert!(kinds[3..].iter().all(|k| *k == "age"));
    for id in 1..=6 {
        assert_eq!(home(&s).join(format!("cmd/{id}")).exists(), id > 3);
    }
    let (s, p) = capture_fixture("gc-size");
    for _ in 0..6 {
        assert_success(&script(&s, &p, "true"));
    }
    let result = gc(&s, &["--older-than", "100d", "--max-size", "0"]);
    assert!(
        result["collections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["kind"] == "size")
    );
    for id in 1..=6 {
        assert_eq!(home(&s).join(format!("cmd/{id}")).exists(), id > 3);
    }
}
// Physical fixtures live under target/. Use a synthetic prefix only while
// testing classification. No syscall executes against these synthetic paths.
fn logical_paths(s: &Sandbox, id: u64, p: &Path) -> Vec<u8> {
    let log = home(s).join(format!("cmd/{id}/effects.log"));
    let original = fs::read(&log).unwrap();
    fs::write(
        log,
        String::from_utf8(original.clone())
            .unwrap()
            .replace(p.to_str().unwrap(), "/saqta-fixture-project"),
    )
    .unwrap();
    original
}
#[test]
fn regenerable_content_expires_first_and_undo_restores_other_content() {
    let (s, p) = capture_fixture("gc-content");
    fs::create_dir(p.join("node_modules")).unwrap();
    fs::write(p.join("node_modules/cache"), "cache version").unwrap();
    fs::write(p.join("source"), "source version").unwrap();
    assert_success(&script(
        &s,
        &p,
        "rm node_modules/cache source; mkdir created",
    ));
    let original = logical_paths(&s, 1, &p);
    age(&s, 1, 2);
    pad(&s, &p);
    let result = gc(&s, &["--older-than", "7d", "--max-size", "5GB"]);
    let items = result["collections"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "regenerable");
    let paths = items[0]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].as_str().unwrap().contains("node_modules"));
    fs::write(home(&s).join("cmd/1/effects.log"), original).unwrap();
    let o = undo(&s, 1);
    assert_success(&o);
    assert!(String::from_utf8_lossy(&o.stdout).contains("expired (collected by gc)"));
    assert_eq!(fs::read(p.join("source")).unwrap(), b"source version");
    assert!(!p.join("node_modules/cache").exists());
    assert!(!p.join("created").exists());
}
#[test]
fn cachedir_tag_and_shared_preimages_keep_nonregenerable_references() {
    let (s, p) = capture_fixture("gc-tag");
    fs::create_dir(p.join("cachearea")).unwrap();
    fs::write(
        p.join("cachearea/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(p.join("cachearea/file"), "cache").unwrap();
    assert_success(&script(&s, &p, "rm cachearea/file"));
    age(&s, 1, 2);
    pad(&s, &p);
    assert_eq!(gc(&s, &[])["collections"][0]["kind"], "regenerable");
    let (s, p) = capture_fixture("gc-shared");
    fs::create_dir(p.join("node_modules")).unwrap();
    fs::write(p.join("source"), "original").unwrap();
    fs::hard_link(p.join("source"), p.join("node_modules/alias")).unwrap();
    assert_success(&script(
        &s,
        &p,
        "printf changed > source; printf again > node_modules/alias",
    ));
    logical_paths(&s, 1, &p);
    age(&s, 1, 2);
    pad(&s, &p);
    assert!(gc(&s, &[])["collections"].as_array().unwrap().is_empty());
}
#[test]
fn running_and_interrupted_undo_are_protected() {
    let (s, p) = capture_fixture("gc-protection");
    fs::write(p.join("file"), "original").unwrap();
    let mut c = s.exec(&[
        "-c",
        "rm file && touch ready && timeout --foreground 30 sh -c 'until [ -e ../release ]; do sleep 0.01; done'",
    ]);
    c.current_dir(&p);
    let child = Running::spawn(c, None);
    wait_until(Duration::from_secs(3), || p.join("ready").exists());
    age(&s, 1, 10);
    pad(&s, &p);
    gc(&s, &["--max-size", "0"]);
    assert!(home(&s).join("cmd/1").exists());
    fs::write(p.with_file_name("release"), b"").unwrap();
    assert_success(&child.output());
    pad(&s, &p);
    let mut c = s.saqta(&["undo", "1"]);
    c.env("SAQTA_TEST_ABORT_AFTER", "1");
    assert_eq!(run(c).status.code(), Some(86));
    let deferred = gc(&s, &["--max-size", "0"]);
    assert!(
        deferred["deferred"]
            .as_str()
            .unwrap()
            .contains("undo in progress")
    );
    assert!(
        home(&s).join("cmd/2").exists(),
        "keep other undo folders that may hold inode handles"
    );
    assert!(home(&s).join("cmd/1/undo-in-progress").exists());
    assert_success(&run(s.saqta(&["undo", "1", "--resume"])));
    assert_eq!(fs::read(p.join("file")).unwrap(), b"original");
}
#[test]
fn incremental_history_does_not_reparse_seventy_thousand_effects() {
    let (s, p) = capture_fixture("history-speed");
    fs::write(p.join("file"), "original").unwrap();
    assert_success(&script(&s, &p, "rm file"));
    let folder = home(&s).join("cmd/0");
    fs::create_dir(&folder).unwrap();
    let mut m: Value =
        serde_json::from_slice(&fs::read(home(&s).join("cmd/1/meta")).unwrap()).unwrap();
    m["id"] = json!(0);
    fs::write(folder.join("meta"), serde_json::to_vec(&m).unwrap()).unwrap();
    fs::write(folder.join("end"), b"{\"status\":0,\"end_ns\":1}").unwrap();
    let mut text = String::new();
    for i in 1..=70_000 {
        text.push_str(&format!("{i}\t1\t0\tnot protected\t\t\t\t0\t{{\"before\":null,\"before2\":null,\"kept\":\"\",\"message\":\"fixture\"}}\n"));
    }
    fs::write(folder.join("effects.log"), text).unwrap();
    let cold = Instant::now();
    assert_success(&run(s.saqta(&["log", "--json"])));
    note(format!(
        "70,000 effects initial import: {:?}",
        cold.elapsed()
    ));
    let db = rusqlite::Connection::open(home(&s).join("journal.db")).unwrap();
    db.execute_batch("CREATE TRIGGER no_reimport BEFORE DELETE ON effects BEGIN SELECT RAISE(FAIL,'unchanged effects reimported'); END;").unwrap();
    for args in [
        vec!["log", "--json"],
        vec!["show", "1", "--json"],
        vec!["undo", "1"],
    ] {
        let start = Instant::now();
        assert_success(&run(s.saqta(&args)));
        let elapsed = start.elapsed();
        note(format!("70,000 effects history, {}: {elapsed:?}", args[0]));
        assert!(elapsed < Duration::from_secs(2), "loose CI bound");
    }
    assert_eq!(fs::read(p.join("file")).unwrap(), b"original");
}
#[test]
fn opportunistic_gc_runs_after_output_at_most_once_per_day() {
    let (s, p) = capture_fixture("gc-opportunistic");
    assert_success(&script(&s, &p, "true"));
    fs::write(home(&s).join("gc-attempt"), b"0").unwrap();
    let start = Instant::now();
    let o = run(s.saqta(&["log", "--json"]));
    assert_success(&o);
    serde_json::from_slice::<Value>(&o.stdout).unwrap();
    note(format!(
        "log scheduling background gc: {:?}",
        start.elapsed()
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
    wait_until(Duration::from_secs(5), || home(&s).join("last-gc").exists());
    let last = fs::read(home(&s).join("last-gc")).unwrap();
    for args in [
        vec!["log"],
        vec!["show", "1"],
        vec!["status"],
        vec!["undo", "1"],
    ] {
        assert_success(&run(s.saqta(&args)));
    }
    assert_eq!(fs::read(home(&s).join("last-gc")).unwrap(), last);
    let o = run(s.saqta(&["status"]));
    assert_success(&o);
    let text = String::from_utf8(o.stdout).unwrap();
    assert!(text.contains("store size:"));
    assert!(text.contains("last gc:"));
}
#[test]
fn gc_keeps_a_newer_copy_when_an_older_retained_hard_link_was_changed() {
    let (s, p) = capture_fixture("gc-old-link");
    fs::create_dir(p.join("node_modules")).unwrap();
    fs::write(p.join("source"), b"original source version").unwrap();
    fs::hard_link(p.join("source"), p.join("node_modules/alias")).unwrap();
    let before = tree_hash(&p);
    assert_success(&script(&s, &p, "rm source"));
    assert_success(&script(&s, &p, "printf newer > node_modules/alias"));
    age(&s, 1, 2);
    age(&s, 2, 2);
    pad(&s, &p);
    let o = run(s.saqta(&["gc"]));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("pre-image changed"));
    let copies = fs::read_dir(home(&s).join("q/main/2"))
        .unwrap()
        .map(|e| fs::read(e.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert!(copies.contains(&b"original source version".to_vec()));
    assert_success(&undo(&s, 2));
    assert_success(&undo(&s, 1));
    assert_tree(&before, &p);
}
