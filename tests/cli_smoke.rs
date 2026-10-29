use assert_cmd::Command;
use predicates::prelude::*;

fn thoth() -> Command {
    Command::new(env!("CARGO_BIN_EXE_thoth"))
}

#[test]
fn version_prints_name_and_version() {
    thoth()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("thoth 0.1.11"));
}

#[test]
fn unknown_subcommand_fails() {
    thoth()
        .arg("no-such-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn init_error_is_json_when_json_flag_set() {
    let assert = thoth()
        .env_remove("THOTH_HOME")
        .env_remove("USERPROFILE")
        .env_remove("HOME")
        .arg("init")
        .arg("--json")
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let v: serde_json::Value =
        serde_json::from_str(stderr.trim()).expect("stderr should be valid JSON");
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "expected a non-empty error string, got: {stderr}"
    );
}

#[test]
fn init_error_is_plain_text_when_json_flag_absent() {
    thoth()
        .env_remove("THOTH_HOME")
        .env_remove("USERPROFILE")
        .env_remove("HOME")
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::starts_with("error:"));
}
