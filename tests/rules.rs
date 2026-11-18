mod common;
use common::*;
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn fixture(name: &str) -> (Sandbox, PathBuf) {
    let (mut s, p) = capture_fixture(name);
    s.env.remove(OsStr::new("SAQTA_RULES"));
    (s, p)
}

fn git(s: &Sandbox, p: &Path, args: &[&str]) -> Output {
    let mut c = s.command("git");
    c.current_dir(p).arg("--no-optional-locks").args(args);
    let o = run(c);
    assert_success(&o);
    o
}

fn repo(s: &Sandbox, p: &Path) {
    git(s, p, &["init", "-q"]);
    git(s, p, &["config", "user.name", "Saqta"]);
    git(s, p, &["config", "user.email", "saqta@example.invalid"]);
    fs::create_dir(p.join("src")).unwrap();
    fs::write(p.join("src/app.ts"), "committed\n").unwrap();
    fs::write(p.join(".gitignore"), ".env\n*.db\n").unwrap();
    git(s, p, &["add", "."]);
    git(s, p, &["commit", "-qm", "initial"]);
}

fn err(o: &Output) -> &str {
    std::str::from_utf8(&o.stderr).unwrap()
}

fn silent(o: Output) {
    assert_success(&o);
    assert_eq!(err(&o), "");
}

fn held(o: &Output, prefix: &str) {
    assert_eq!(o.status.code(), Some(77), "{}", err(o));
    assert!(err(o).starts_with(prefix), "{}", err(o));
    assert!(
        err(o).contains("they can run: saqta release 1\n"),
        "{}",
        err(o)
    );
    assert_eq!(err(o).lines().count(), 4, "{}", err(o));
}

#[test]
fn checkout_restores_edits_and_records_hold() {
    let (s, p) = fixture("rules-checkout");
    repo(&s, &p);
    fs::write(p.join("src/app.ts"), "local edit\n").unwrap();
    let o = script(&s, &p, "git --no-optional-locks checkout -- .");
    let expected = "[saqta 1] Restored 1 file with uncommitted edits that this command discarded:\n  src/app.ts. Git doesn't have these changes.\n  To set changes aside, use: git stash -u\n  If the user really wants them discarded, they can run: saqta release 1\n";
    assert_eq!(o.status.code(), Some(77), "{}", err(&o));
    assert_eq!(err(&o), expected);
    assert_eq!(
        fs::read_to_string(p.join("src/app.ts")).unwrap(),
        "local edit\n"
    );
    assert_eq!(shown(&s, 1)["held"], true);
    let status = run(s.saqta(&["status"]));
    assert!(String::from_utf8_lossy(&status.stdout).contains("open hold: 1"));
    let log = run(s.saqta(&["log", "--json"]));
    assert_eq!(
        serde_json::from_slice::<Value>(&log.stdout).unwrap()[0]["held"],
        true
    );
}

#[test]
fn stash_commit_and_soft_reset_keep_git_versions() {
    let (s, p) = fixture("rules-stash");
    repo(&s, &p);
    for name in ["a", "b", "c", ".env"] {
        fs::write(p.join(name), name).unwrap();
    }
    fs::write(p.join("src/app.ts"), "stashed one\n").unwrap();
    silent(script(&s, &p, "git --no-optional-locks stash -u -qm first"));
    assert!(!p.join("a").exists());
    assert!(p.join(".env").exists());
    silent(script(&s, &p, "true"));
    fs::write(p.join("src/app.ts"), "stashed two\n").unwrap();
    silent(script(&s, &p, "git --no-optional-locks stash -qm second"));
    fs::write(p.join("src/app.ts"), "stashed one\n").unwrap();
    silent(script(&s, &p, "git --no-optional-locks checkout -- ."));
    fs::write(p.join("src/app.ts"), "next commit\n").unwrap();
    silent(script(
        &s,
        &p,
        "git --no-optional-locks add src/app.ts && git --no-optional-locks commit -qm next",
    ));
    silent(script(&s, &p, "git --no-optional-locks reset --soft HEAD^"));
    assert_eq!(
        fs::read_to_string(p.join("src/app.ts")).unwrap(),
        "next commit\n"
    );
}

#[test]
fn circuit_breaker_stops_home_wipe_and_restores_files() {
    let (s, p) = fixture("rules-breaker");
    let home = PathBuf::from(&s.env[OsStr::new("HOME")]);
    for i in 0..350 {
        fs::write(home.join(format!("decoy-{i:03}")), format!("data {i}")).unwrap();
    }
    let o = script(&s, &p, "rm -rf -- \"$HOME\"");
    held(
        &o,
        "[saqta 1] Stopped this command and restored 201 files after too much data was discarded:",
    );
    let record = shown(&s, 1);
    let count = record["effects"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["op"] == "delete" && e["path"].as_str().is_some_and(|p| p.contains("/decoy-"))
        })
        .count();
    note(format!(
        "circuit breaker recorded {count} decoy deletions at threshold 200"
    ));
    assert!((201..=205).contains(&count));
    for i in 0..350 {
        assert_eq!(
            fs::read_to_string(home.join(format!("decoy-{i:03}"))).unwrap(),
            format!("data {i}")
        );
    }
}

#[test]
fn git_clean_holds_user_files_and_leaves_generated_files_gone() {
    let (s, p) = fixture("rules-clean");
    repo(&s, &p);
    for f in [".env", "local.db", "a", "b", "c"] {
        fs::write(p.join(f), "private").unwrap();
    }
    for dir in ["node_modules", "target", "__pycache__"] {
        fs::create_dir(p.join(dir)).unwrap();
        fs::write(p.join(dir).join("out"), "generated").unwrap();
    }
    let o = script(&s, &p, "git --no-optional-locks clean -qfdx");
    held(
        &o,
        "[saqta 1] Restored 5 files that this command discarded:",
    );
    for f in [".env", "local.db", "a", "b", "c"] {
        assert_eq!(fs::read_to_string(p.join(f)).unwrap(), "private");
    }
    for dir in ["node_modules", "target", "__pycache__"] {
        assert!(!p.join(dir).exists());
    }

    let (s, p) = fixture("rules-clean-generated");
    repo(&s, &p);
    silent(script(
        &s,
        &p,
        "mkdir node_modules target __pycache__; touch node_modules/out target/out __pycache__/out",
    ));
    silent(script(&s, &p, "git --no-optional-locks clean -qfdx"));
    assert!(!p.join("node_modules").exists());
}

#[test]
fn deploy_restores_dirty_source_and_preserves_unrelated_effects() {
    let (s, p) = fixture("rules-deploy");
    repo(&s, &p);
    fs::write(p.join("src/app.ts"), "uncommitted").unwrap();
    fs::write(p.join("old-cache"), "replace").unwrap();
    let o = script(
        &s,
        &p,
        "rm -r src; rm old-cache; echo published > deploy.log",
    );
    held(&o, "[saqta 1] Restored 1 file with uncommitted edits");
    assert_eq!(
        fs::read_to_string(p.join("src/app.ts")).unwrap(),
        "uncommitted"
    );
    assert!(!p.join("old-cache").exists());
    assert!(p.join("deploy.log").exists());
    assert_success(&undo(&s, 1));
    assert_eq!(
        fs::read_to_string(p.join("src/app.ts")).unwrap(),
        "uncommitted"
    );
    assert_eq!(fs::read_to_string(p.join("old-cache")).unwrap(), "replace");
    assert!(!p.join("deploy.log").exists());
}

#[test]
fn install_restores_precious_and_project_overrides_apply() {
    let (s, p) = fixture("rules-precious");
    fs::write(p.join(".env"), "secret").unwrap();
    fs::write(p.join("private.data"), "sensitive").unwrap();
    fs::write(p.join("demo.pem"), "fake").unwrap();
    fs::write(
        p.join(".saqta.toml"),
        "precious = ['*.data']\nprecious_exclude = ['demo.pem']\n",
    )
    .unwrap();
    let o = script(&s, &p, "echo default > .env; rm -- private.data demo.pem");
    held(&o, "[saqta 1] Restored 2 precious files");
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "secret");
    assert_eq!(
        fs::read_to_string(p.join("private.data")).unwrap(),
        "sensitive"
    );
    assert!(!p.join("demo.pem").exists());
}

#[test]
fn same_session_uses_file_identity_and_missing_session_does_not_share() {
    let (s, p) = fixture("rules-session");
    repo(&s, &p);
    let exec = |session: Option<&str>, code: &str| {
        let mut c = s.exec(&[]);
        if let Some(id) = session {
            c.args(["--session", id]);
        }
        c.current_dir(&p).args(["-c", code]);
        run(c)
    };
    silent(exec(Some("a"), "echo one > .env.agent"));
    silent(exec(Some("a"), "rm .env.agent"));
    assert!(!p.join(".env.agent").exists());
    silent(exec(None, "echo two > .env.user"));
    let o = exec(None, "rm .env.user");
    assert_eq!(o.status.code(), Some(77), "{}", err(&o));
    assert_eq!(fs::read_to_string(p.join(".env.user")).unwrap(), "two\n");
}

#[test]
fn repository_database_and_irreplaceable_home_have_distinct_messages() {
    let (s, p) = fixture("rules-database");
    repo(&s, &p);
    fs::write(p.join(".git/description"), "description").unwrap();
    let o = script(&s, &p, "rm .git/description");
    assert_eq!(
        err(&o),
        "[saqta 1] Restored 1 repository database file that a non-git process discarded:\n  .git/description\n  Use git to change the repository database.\n  If the user really wants them discarded, they can run: saqta release 1\n"
    );
    held(&o, "[saqta 1] Restored 1 repository database file");
    assert!(p.join(".git/description").exists());

    let (s, p) = fixture("rules-documents");
    let docs = PathBuf::from(&s.env[OsStr::new("HOME")]).join("Documents");
    fs::create_dir(&docs).unwrap();
    fs::write(docs.join("my draft"), "draft").unwrap();
    let o = script(&s, &p, "rm -- \"$HOME/Documents/my draft\"");
    held(&o, "[saqta 1] Restored 1 file in irreplaceable folders");
    let shown_path = docs.join("my draft").to_string_lossy().into_owned();
    let shown_path = if shown_path.chars().count() > 77 {
        format!("{}...", shown_path.chars().take(77).collect::<String>())
    } else {
        format!("'{shown_path}'")
    };
    assert_eq!(
        err(&o),
        format!(
            "[saqta 1] Restored 1 file in irreplaceable folders that this command discarded:\n  {shown_path}\n  Ask the user before changing files in these folders.\n  If the user really wants them discarded, they can run: saqta release 1\n"
        )
    );
    assert_eq!(fs::read_to_string(docs.join("my draft")).unwrap(), "draft");
}

#[test]
fn paths_are_bounded_and_controls_are_stripped_from_stderr() {
    let (s, p) = fixture("rules-message");
    repo(&s, &p);
    for name in [
        "a name".to_owned(),
        "b\n\u{1b}'quoted".to_owned(),
        format!("c{}", "x".repeat(160)),
        "d".to_owned(),
    ] {
        fs::write(p.join(name), "user data").unwrap();
    }
    let o = script(&s, &p, "git --no-optional-locks clean -qfd");
    held(&o, "[saqta 1] Restored 4 untracked user files");
    assert_eq!(
        err(&o),
        format!(
            "[saqta 1] Restored 4 untracked user files that this command discarded:\n  'a name', 'b'\\''quoted', c{}... (+1 more)\n  Move the files aside or ask the user before discarding them.\n  If the user really wants them discarded, they can run: saqta release 1\n",
            "x".repeat(76)
        )
    );
}

#[test]
fn release_is_refused_without_terminal_and_inside_supervision() {
    let (s, p) = fixture("rules-release-deny");
    for o in [
        run(s.saqta(&["release", "1"])),
        script(&s, &p, "saqta release 1"),
        script(&s, &p, "env -u SAQTA_SUPERVISED saqta release 1"),
    ] {
        assert_eq!(o.status.code(), Some(125));
        assert_eq!(
            err(&o),
            "saqta: release must be run by a person in their own terminal\n"
        );
    }
    let config = PathBuf::from(&s.env[OsStr::new("HOME")]).join(".config/saqta");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "release_without_terminal = true\n",
    )
    .unwrap();
    let o = script(&s, &p, "env -u SAQTA_SUPERVISED saqta release 1");
    assert_eq!(o.status.code(), Some(125));
    assert_eq!(
        err(&o),
        "saqta: release must be run by a person in their own terminal\n"
    );
}

#[test]
fn release_applies_agent_version_and_can_be_undone_after_a_delayed_undo() {
    let (s, p) = fixture("rules-release");
    fs::write(p.join(".env"), "human").unwrap();
    let o = script(&s, &p, "echo agent > .env; echo built > artifact");
    held(&o, "[saqta 1] Restored 1 precious file");
    // Move the journal clock back one hour without sleeping.
    let meta = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]).join("cmd/1/meta");
    let mut value: Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    value["start_ns"] = (value["start_ns"].as_u64().unwrap() - 3_600_000_000_000).into();
    fs::write(meta, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_success(&undo(&s, 1));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "human");
    assert!(!p.join("artifact").exists());

    if fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .any(|s| s.split_whitespace().eq(["Seccomp:", "2"]))
    {
        note("SKIP: positive release requires an unfiltered process");
        return;
    }
    let config = PathBuf::from(&s.env[OsStr::new("HOME")]).join(".config/saqta");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "release_without_terminal = true\n",
    )
    .unwrap();
    assert_success(&run(s.saqta(&["release", "1"])));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "agent\n");
    assert_success(&undo(&s, 2));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "human");
}

#[test]
fn config_deny_floor_and_background_notice() {
    let (s, p) = fixture("rules-background");
    let config = PathBuf::from(&s.env[OsStr::new("HOME")]).join(".config/saqta");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "# trusted\n").unwrap();
    let o = script(
        &s,
        &p,
        "printf hacked > \"$HOME/.config/saqta/config.toml\"",
    );
    assert!(!o.status.success());
    assert_eq!(
        fs::read_to_string(config.join("config.toml")).unwrap(),
        "# trusted\n"
    );
    fs::write(p.join(".env"), "keep").unwrap();
    let mut c = s.exec(&[
        "--session",
        "server",
        "-c",
        "(sleep .15; echo ready > heartbeat; sleep .3; rm .env) </dev/null >/dev/null 2>&1 &",
    ]);
    c.current_dir(&p);
    let started = Instant::now();
    silent(run(c));
    assert!(started.elapsed() < Duration::from_secs(5));
    let marker = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]).join("holds/2/ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(p.join("heartbeat")).unwrap(), "ready\n");
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "keep");
    let o = run(s.exec(&["--session", "server", "true"]));
    assert_success(&o);
    assert_eq!(
        err(&o),
        "[saqta 2] Restored files discarded by a background job (details: saqta show 2)\n"
    );
    silent(run(s.exec(&["--session", "server", "true"])));
}

#[test]
fn benign_build_outputs_and_tracked_atomic_saves_are_silent() {
    let (s, p) = fixture("rules-benign");
    repo(&s, &p);
    silent(script(
        &s,
        &p,
        "mkdir node_modules target .pytest_cache; echo cache > node_modules/out; echo cache > target/out; echo cache > .pytest_cache/out",
    ));
    silent(script(
        &s,
        &p,
        "echo updated > node_modules/out; echo updated > target/out; rm .pytest_cache/out",
    ));
    silent(script(
        &s,
        &p,
        "printf 'formatted\\n' > src/.app.tmp; mv src/.app.tmp src/app.ts",
    ));
    fs::write(p.join("untouched"), "same").unwrap();
    silent(script(&s, &p, "exec 8<>untouched"));
    fs::write(p.join("loose"), "old").unwrap();
    let o = script(&s, &p, "echo new > loose");
    assert_success(&o);
    assert_eq!(
        err(&o),
        "[saqta 5] kept 1 edited file (undo: saqta undo 5)\n"
    );
}

fn tool_available(s: &Sandbox, name: &str) -> bool {
    s.command(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn foreground_hold_precedes_return_with_a_live_background_job() {
    let (s, p) = fixture("rules-two-phases");
    fs::write(p.join(".env"), "foreground").unwrap();
    fs::write(p.join("service.key"), "background").unwrap();
    let mut c = s.exec(&[
        "--session",
        "two-phases",
        "-c",
        "(n=0; while [ ! -f advance ] && [ \"$n\" -lt 200 ]; do n=$((n+1)); sleep .02; done; rm service.key; echo done > heartbeat) </dev/null >/dev/null 2>&1 & rm .env",
    ]);
    c.current_dir(&p);
    let output = run(c);
    held(&output, "[saqta 1] Restored 1 precious file");
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "foreground");
    fs::write(p.join("advance"), "").unwrap();
    let home = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !home.join("holds/2/ready").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read_to_string(p.join("heartbeat")).unwrap(), "done\n");
    assert_eq!(
        fs::read_to_string(p.join("service.key")).unwrap(),
        "background"
    );
    let o = run(s.exec(&["--session", "two-phases", "true"]));
    assert_success(&o);
    assert_eq!(
        err(&o),
        "[saqta 2] Restored files discarded by a background job (details: saqta show 2)\n"
    );
}

#[test]
fn background_hold_keeps_foreground_changes_on_the_same_path() {
    let (s, p) = fixture("rules-background-overlap");
    fs::write(p.join(".env"), "begin\n").unwrap();
    let mut c = s.exec(&[
        "-c",
        "echo extra >> .env; (sleep .25; rm .env) </dev/null >/dev/null 2>&1 &",
    ]);
    c.current_dir(&p);
    let o = run(c);
    assert_success(&o);
    assert_eq!(
        err(&o),
        "[saqta 1] kept 1 edited file (undo: saqta undo 1)\n"
    );
    let marker = PathBuf::from(&s.env[OsStr::new("SAQTA_HOME")]).join("holds/1/ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(p.join(".env")).unwrap(),
        "begin\nextra\n"
    );
    assert_success(&undo(&s, 1));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "begin\n");
}

#[test]
fn interrupted_automatic_restore_resumes_and_gc_protects_the_hold() {
    let (s, p) = fixture("rules-resume");
    fs::write(p.join(".env"), "keep").unwrap();
    fs::write(p.join("server.key"), "also keep").unwrap();
    let mut c = s.exec(&["-c", "rm .env server.key"]);
    c.current_dir(&p).env("SAQTA_TEST_ABORT_AFTER", "1");
    assert_eq!(run(c).status.code(), Some(125));
    let blocked = undo(&s, 1);
    assert!(!blocked.status.success());
    assert!(err(&blocked).contains("undo 1 --resume"));
    let gc = run(s.saqta(&["gc", "--dry-run", "--json"]));
    assert_success(&gc);
    assert!(String::from_utf8_lossy(&gc.stdout).contains("collection deferred"));
    assert_success(&run(s.saqta(&["undo", "1", "--resume"])));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "keep");
    assert_eq!(
        fs::read_to_string(p.join("server.key")).unwrap(),
        "also keep"
    );
    assert_eq!(shown(&s, 1)["held"], true);
    for _ in 0..4 {
        silent(run(s.exec(&["true"])));
    }
    let gc = run(s.saqta(&["gc", "--older-than", "0s", "--max-size", "0B", "--json"]));
    assert_success(&gc);
    assert_eq!(shown(&s, 1)["held"], true);
}

#[test]
fn byte_and_irreplaceable_thresholds_are_configurable() {
    let (s, p) = fixture("rules-byte-threshold");
    fs::write(p.join(".saqta.toml"), "max_bytes = 10\n").unwrap();
    fs::write(p.join("large"), "01234567890").unwrap();
    fs::write(p.join("next"), "next").unwrap();
    held(
        &script(&s, &p, "rm large next"),
        "[saqta 1] Stopped this command and restored 1 file",
    );
    assert_eq!(fs::read_to_string(p.join("large")).unwrap(), "01234567890");
    assert_eq!(fs::read_to_string(p.join("next")).unwrap(), "next");

    let (s, p) = fixture("rules-home-threshold");
    fs::write(p.join(".saqta.toml"), "max_irreplaceable = 2\n").unwrap();
    let docs = PathBuf::from(&s.env[OsStr::new("HOME")]).join("Documents");
    fs::create_dir(&docs).unwrap();
    for name in ["a", "b", "c", "d", "e"] {
        fs::write(docs.join(name), "data").unwrap();
    }
    held(
        &script(&s, &p, "rm \"$HOME/Documents/\"*"),
        "[saqta 1] Stopped this command and restored 3 files",
    );
    assert_eq!(fs::read_dir(docs).unwrap().count(), 5);
}

#[test]
fn npm_and_pnpm_install_are_silent_with_offline_fixtures() {
    let (s, p) = fixture("rules-package-install");
    repo(&s, &p);
    fs::write(
        p.join("package.json"),
        "{\"name\":\"saqta-decoy\",\"version\":\"1.0.0\"}\n",
    )
    .unwrap();
    for (tool, flags) in [
        (
            "npm",
            &["--ignore-scripts", "--no-audit", "--no-fund", "--silent"][..],
        ),
        ("pnpm", &["--ignore-scripts", "--reporter=silent"][..]),
    ] {
        if !tool_available(&s, tool) {
            note(format!("SKIP: {tool} unavailable"));
            continue;
        }
        for _ in 0..2 {
            let mut c = s.exec(&[tool, "install", "--offline"]);
            c.args(flags)
                .current_dir(&p)
                .env("npm_config_cache", p.join(".cache/npm"));
            silent(run(c));
        }
    }
}

#[test]
fn cargo_build_and_rustfmt_are_silent() {
    let (s, p) = fixture("rules-cargo");
    repo(&s, &p);
    let cargo = Path::new(env!("CARGO"));
    let rustc = cargo.with_file_name("rustc");
    let rustfmt = cargo.with_file_name("rustfmt");
    fs::write(
        p.join("Cargo.toml"),
        "[package]\nname = 'saqta-decoy'\nversion = '0.1.0'\nedition = '2024'\n",
    )
    .unwrap();
    fs::write(
        p.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"saqta-decoy\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(p.join("src/main.rs"), "fn main(){println!(\"fixture\");}\n").unwrap();
    git(&s, &p, &["add", "Cargo.toml", "Cargo.lock", "src/main.rs"]);
    git(&s, &p, &["commit", "-qm", "fixture"]);
    for _ in 0..2 {
        let mut c = s.exec(&[]);
        c.arg(cargo)
            .args(["build", "--offline", "--locked", "-q"])
            .current_dir(&p)
            .env("RUSTC", &rustc)
            .env("CARGO_HOME", p.join(".cache/cargo"))
            .env("CARGO_TARGET_DIR", p.join("target"));
        silent(run(c));
    }
    if rustfmt.is_file() {
        let mut c = s.exec(&[]);
        c.arg(rustfmt).arg("src/main.rs").current_dir(&p);
        silent(run(c));
        assert_eq!(
            fs::read_to_string(p.join("src/main.rs")).unwrap(),
            "fn main() {\n    println!(\"fixture\");\n}\n"
        );
    } else {
        note("SKIP: rustfmt unavailable");
    }
}

#[test]
fn pytest_is_silent_when_available() {
    let (s, p) = fixture("rules-pytest");
    if !tool_available(&s, "pytest") {
        note("SKIP: pytest unavailable");
        return;
    }
    repo(&s, &p);
    fs::write(
        p.join("test_fixture.py"),
        "def test_fixture():\n    assert 2 + 2 == 4\n",
    )
    .unwrap();
    for _ in 0..2 {
        let mut c = s.exec(&["pytest", "-q"]);
        c.current_dir(&p).env("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1");
        silent(run(c));
    }
}

#[test]
fn clean_git_line_endings_and_dirty_crlf_content_are_distinguished() {
    let (s, p) = fixture("rules-crlf");
    repo(&s, &p);
    git(&s, &p, &["config", "core.autocrlf", "true"]);
    fs::write(p.join("src/app.ts"), "committed\r\n").unwrap();
    silent(script(&s, &p, "sed -i s/committed/formatted/ src/app.ts"));
    fs::write(p.join("src/app.ts"), "uncommitted\r\n").unwrap();
    let o = script(&s, &p, "git --no-optional-locks checkout -- .");
    assert_eq!(o.status.code(), Some(77), "{}", err(&o));
    assert!(err(&o).starts_with("[saqta 2] Restored 1 file with uncommitted edits"));
    assert_eq!(fs::read(p.join("src/app.ts")).unwrap(), b"uncommitted\r\n");
}

#[test]
fn release_from_a_terminal_preserves_aliases_and_rejects_newer_content() {
    let (s, p) = fixture("rules-release-terminal");
    if fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .any(|s| s.split_whitespace().eq(["Seccomp:", "2"]))
    {
        note("SKIP: terminal release requires an unfiltered process");
        return;
    }
    if !tool_available(&s, "script") {
        note("SKIP: util-linux script for a PTY is unavailable");
        return;
    }
    fs::write(p.join(".env"), "human").unwrap();
    fs::hard_link(p.join(".env"), p.join("alias")).unwrap();
    held(
        &script(&s, &p, "echo agent > .env"),
        "[saqta 1] Restored 1 precious file",
    );
    assert_eq!(fs::read_to_string(p.join("alias")).unwrap(), "human");
    let terminal = |command: &str| {
        let mut c = s.command("script");
        c.args(["-qefc", command, "/dev/null"]).current_dir(&p);
        run(c)
    };
    let denied = terminal("SAQTA_SUPERVISED=1 saqta release 1");
    assert_eq!(denied.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&denied.stdout)
            .contains("release must be run by a person in their own terminal")
    );
    assert_success(&terminal("saqta release 1"));
    for name in [".env", "alias"] {
        assert_eq!(fs::read_to_string(p.join(name)).unwrap(), "agent\n");
    }
    assert_success(&undo(&s, 2));
    for name in [".env", "alias"] {
        assert_eq!(fs::read_to_string(p.join(name)).unwrap(), "human");
    }
    let o = script(&s, &p, "echo agent-again > .env");
    assert_eq!(o.status.code(), Some(77), "{}", err(&o));
    fs::write(p.join(".env"), "new user edit").unwrap();
    assert_eq!(terminal("saqta release 3").status.code(), Some(125));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "new user edit");
    assert_eq!(shown(&s, 3)["released"], false);
}

#[test]
fn interrupted_release_resumes_with_a_single_undo_id() {
    let (s, p) = fixture("rules-release-resume");
    if fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .any(|s| s.split_whitespace().eq(["Seccomp:", "2"]))
    {
        note("SKIP: release recovery requires an unfiltered process");
        return;
    }
    fs::write(p.join(".env"), "human").unwrap();
    held(
        &script(&s, &p, "echo agent > .env"),
        "[saqta 1] Restored 1 precious file",
    );
    let config = PathBuf::from(&s.env[OsStr::new("HOME")]).join(".config/saqta");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "release_without_terminal = true\n",
    )
    .unwrap();
    let mut release = s.saqta(&["release", "1"]);
    release.env("SAQTA_TEST_ABORT_AFTER", "1");
    assert_eq!(run(release).status.code(), Some(86));
    assert!(!undo(&s, 2).status.success());
    assert_success(&run(s.saqta(&["release", "1"])));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "agent\n");
    assert_eq!(last_id(&s), 2);
    assert_success(&undo(&s, 2));
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "human");
}

#[test]
fn pid_one_seals_hold_before_the_namespace_exits() {
    let (s, p) = fixture("rules-pid-one");
    let flags = [
        "--user",
        "--map-root-user",
        "--pid",
        "--fork",
        "--mount-proc",
    ];
    let mut probe = s.command("unshare");
    probe.args(flags).arg("true");
    let probe = match probe.output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            note("SKIP: unshare unavailable");
            return;
        }
        Err(e) => panic!("namespace probe failed: {e}"),
    };
    if !probe.status.success()
        && [
            "Operation not permitted",
            "Permission denied",
            "Invalid argument",
        ]
        .iter()
        .any(|reason| err(&probe).contains(reason))
    {
        note(format!(
            "SKIP: PID namespace unavailable: {}",
            err(&probe).trim()
        ));
        return;
    }
    assert_success(&probe);
    fs::write(p.join(".env"), "keep").unwrap();
    let mut c = s.command("unshare");
    c.args(flags)
        .args([
            BIN,
            "exec",
            "-c",
            "rm .env; sleep 5 </dev/null >/dev/null 2>&1 &",
        ])
        .current_dir(&p);
    held(&run(c), "[saqta 1] Restored 1 precious file");
    assert_eq!(fs::read_to_string(p.join(".env")).unwrap(), "keep");
    assert_eq!(shown(&s, 1)["exit_status"], 77);
}

#[test]
fn index_and_head_copies_prevent_a_hold() {
    let (s, p) = fixture("rules-index");
    repo(&s, &p);
    fs::write(p.join("src/app.ts"), "staged version\n").unwrap();
    git(&s, &p, &["add", "src/app.ts"]);
    let o = script(&s, &p, "printf 'rewritten\\n' > src/app.ts");
    assert_success(&o);
    assert_eq!(
        err(&o),
        "[saqta 1] kept 1 edited file (undo: saqta undo 1)\n"
    );
    assert_eq!(
        git(&s, &p, &["show", ":src/app.ts"]).stdout,
        b"staged version\n"
    );
}
