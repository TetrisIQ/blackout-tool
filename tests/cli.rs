//! Runs the real `kit` binary. Uses only static fixtures (no network) and a temp data dir.

use std::process::{Command, Output};

fn kit(args: &[&str]) -> Output {
    let catalog = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalog_static");
    let cache = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_kit"))
        .args(["--catalog", catalog, "--offline"])
        .args(args)
        .env("KIT_CACHE_DIR", cache.path())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn plan_succeeds_for_small_selection() {
    let data = tempfile::tempdir().unwrap();
    let out = kit(&[
        "plan",
        "--select",
        "tiny",
        "--data-dir",
        data.path().to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("tiny") && stdout.contains("1.0 kB") && stdout.contains("OK: fits"),
        "{stdout}"
    );
}

#[test]
fn plan_refuses_with_exit_code_3_when_disk_is_too_small() {
    let data = tempfile::tempdir().unwrap();
    let out = kit(&[
        "plan",
        "--select",
        "huge",
        "--data-dir",
        data.path().join("not/yet/created").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3));
    assert!(text(&out.stdout).contains("INSUFFICIENT DISK SPACE"));
}

#[test]
fn unknown_package_exits_with_1_and_message_on_stderr() {
    let out = kit(&["plan", "--select", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("unknown package"));
}

#[test]
fn list_and_show_work() {
    let out = kit(&["list"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("tiny"));
    let out = kit(&["show", "tiny"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("CC0-1.0"));
}
