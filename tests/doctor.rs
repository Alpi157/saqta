mod common;
use common::*;
use std::{ffi::OsStr, fs, os::unix::fs::PermissionsExt, path::Path};

fn doctor(s: &Sandbox) -> serde_json::Value {
    let mut command = s.command(BIN);
    command.args(["doctor", "--json"]);
    let o = run(command);
    assert_success(&o);
    serde_json::from_slice(&o.stdout).unwrap()
}
fn home_check(value: &serde_json::Value) -> &serde_json::Value {
    value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "SAQTA_HOME")
        .unwrap()
}
#[test]
fn doctor_json_reports_functional_probe_and_setuid_note() {
    let s = Sandbox::new("doctor_json_reports_functional_probe_and_setuid_note");
    let obj = doctor(&s);
    assert!(
        obj["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "functional probe" && c["status"] == "ok")
    );
    assert!(obj["note"].as_str().unwrap().contains("no_new_privs"));
}
#[test]
fn doctor_shows_filesystem_name_and_free_gib() {
    let s = Sandbox::new("doctor_shows_filesystem_name_and_free_gib");
    let obj = doctor(&s);
    let advice = home_check(&obj)["advice"].as_str().unwrap();
    assert!(advice.contains(" GiB free;"), "{advice}");
    assert!(
        ["ext4", "btrfs", "xfs", "tmpfs", "9p", "overlayfs", "0x"]
            .iter()
            .any(|name| advice.contains(&format!("filesystem {name}"))),
        "{advice}"
    );
    let kind = advice
        .split("filesystem ")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert!(
        ["ext4", "btrfs", "xfs", "tmpfs", "9p", "overlayfs"].contains(&kind)
            || kind.starts_with("0x"),
        "{advice}"
    );
    assert!(advice.contains("same filesystem: true"));
}
#[test]
fn existing_home_permissions_are_preserved_and_doctor_warns() {
    let mut s = Sandbox::new("existing_home_permissions_are_preserved_and_doctor_warns");
    // Model SAQTA_HOME=$HOME using only this test's isolated home directory.
    let home = s.env[OsStr::new("HOME")].clone();
    s.env.insert("SAQTA_HOME".into(), home.clone());
    for mode in [0o744, 0o722, 0o700, 0o750, 0o720, 0o704, 0o702, 0o755] {
        fs::set_permissions(Path::new(&home), fs::Permissions::from_mode(mode)).unwrap();
        assert_success(&s.run(&["--", "true"]));
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            mode
        );
        let obj = doctor(&s);
        let check = home_check(&obj);
        assert_eq!(check["status"], if mode == 0o700 { "ok" } else { "warn" });
        assert_eq!(
            check["advice"]
                .as_str()
                .unwrap()
                .contains("readable or writable by group or others"),
            mode != 0o700
        );
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}
#[test]
fn newly_created_home_is_private() {
    let mut s = Sandbox::new("newly_created_home_is_private");
    let home = s.dir.join("new/home");
    s.env
        .insert("SAQTA_HOME".into(), home.clone().into_os_string());
    assert_success(&s.run(&["--", "true"]));
    assert_eq!(
        fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
#[test]
fn home_falls_back_to_xdg_then_home() {
    let mut s = Sandbox::new("home_falls_back_to_xdg_then_home");
    s.env.remove(OsStr::new("SAQTA_HOME"));
    let xdg = Path::new(&s.env[OsStr::new("XDG_DATA_HOME")]).join("saqta");
    assert_success(&s.run(&["--", "true"]));
    assert!(xdg.is_dir());
    s.env.remove(OsStr::new("XDG_DATA_HOME"));
    let home = Path::new(&s.env[OsStr::new("HOME")]).join(".local/share/saqta");
    assert_success(&s.run(&["--", "true"]));
    assert!(home.is_dir());
}
#[test]
fn cli_help_describes_commands_and_arguments() {
    let s = Sandbox::new("cli_help_describes_commands_and_arguments");
    for args in [
        vec!["--help"],
        vec!["exec", "--help"],
        vec!["doctor", "--help"],
    ] {
        let mut command = s.command(BIN);
        command.args(&args);
        let o = run(command);
        assert_success(&o);
        let help = String::from_utf8(o.stdout).unwrap();
        match args[0] {
            "exec" => {
                for description in [
                    "Run a shell string",
                    "Shell for -c",
                    "Run STRING",
                    "Program and arguments",
                ] {
                    assert!(help.contains(description), "{help}");
                }
            }
            "doctor" => {
                assert!(help.contains("Check whether"));
                assert!(help.contains("Print checks and advice as JSON"));
            }
            _ => {
                assert!(help.contains("Run coding agent commands"));
                assert!(help.contains("Check whether"));
                assert!(help.contains("Run a shell string"));
            }
        }
    }
    let mut command = s.command(BIN);
    command.arg("--version");
    let o = run(command);
    assert_success(&o);
    assert_eq!(o.stdout, b"saqta 0.1.0\n");
}
