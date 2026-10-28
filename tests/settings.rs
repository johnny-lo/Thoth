use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn command(home: &TempDir) -> Command {
    command_path(home.path())
}

fn command_path(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_thoth"));
    command.env("THOTH_HOME", home);
    command.env_remove("THOTH_RUN_ID");
    command
}

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.success().get_output().stdout).unwrap()
}

#[test]
fn config_list_and_get_have_contract_and_are_read_only() {
    let home = tempfile::tempdir().unwrap();
    let before: Vec<_> = fs::read_dir(home.path()).unwrap().collect();
    let list = json_output(command(&home).args(["config", "list", "--json"]).assert());
    let settings = list["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 16);
    assert_eq!(settings[0]["key"], "live-quota");
    assert_eq!(settings[1]["key"], "scores-auto-refresh");
    assert_eq!(settings[2]["key"], "require-reason");
    for setting in &settings[..3] {
        assert_eq!(setting["value"], "off");
        assert_eq!(setting["default"], "off");
        assert!(setting["description"].is_string());
    }
    let get = json_output(command(&home).args(["config", "get", "live-quota", "--json"]).assert());
    assert_eq!(get, serde_json::json!({"key":"live-quota","value":"off"}));
    command(&home)
        .args(["config", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("live-quota: off"));
    assert_eq!(before.len(), fs::read_dir(home.path()).unwrap().count());
}

#[test]
fn config_set_writes_boolean_preserves_unknown_and_reports_previous() {
    let home = tempfile::tempdir().unwrap();
    let first = json_output(command(&home).args(["config", "set", "require-reason", "on", "--json"]).assert());
    assert_eq!(first["ok"], true);
    assert_eq!(first["previous"], "off");
    assert!(first["notice"].as_str().unwrap().contains("--reason"));
    assert!(first["notice"].as_str().unwrap().contains("require-reason off"));
    let mut object: Value = serde_json::from_str(&fs::read_to_string(home.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(object["require_reason"], true);
    object["unknown"] = serde_json::json!(42);
    fs::write(home.path().join("config.json"), serde_json::to_vec(&object).unwrap()).unwrap();
    let second = json_output(command(&home).args(["config", "set", "live-quota", "on", "--json"]).assert());
    assert_eq!(second["previous"], "off");
    assert!(second["notice"].as_str().unwrap().contains("official"));
    let stored: Value = serde_json::from_str(&fs::read_to_string(home.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(stored["unknown"], 42);
    assert_eq!(stored["live_quota"], true);
    let off = json_output(command(&home).args(["config", "set", "live-quota", "off", "--json"]).assert());
    assert_eq!(off["notice"], Value::Null);
}

#[test]
fn config_rejects_unknown_key_and_value_without_writing() {
    let home = tempfile::tempdir().unwrap();
    command(&home).args(["config", "set", "rotation", "on"]).assert().failure().stderr(predicates::str::contains("require-reason").and(predicates::str::contains("live-quota")).and(predicates::str::contains("on")).and(predicates::str::contains("off")));
    command(&home).args(["config", "set", "live-quota", "yes"]).assert().failure().stderr(predicates::str::contains("require-reason").and(predicates::str::contains("live-quota")));
    command(&home).args(["config", "get", "rotation"]).assert().failure().stderr(predicates::str::contains("require-reason").and(predicates::str::contains("live-quota")));
    assert!(!home.path().join("config.json").exists());
}

#[test]
fn config_set_rejects_invalid_existing_file_without_overwriting() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.json");
    fs::write(&path, "not json").unwrap();
    command(&home).args(["config", "set", "live-quota", "on"]).assert().failure().stderr(predicates::str::contains(path.display().to_string()));
    assert_eq!(fs::read_to_string(path).unwrap(), "not json");
    fs::write(home.path().join("config.json"), "[]").unwrap();
    command(&home).args(["config", "set", "live-quota", "on"]).assert().failure();
}

#[test]
fn config_reading_bad_or_missing_keys_defaults_to_false() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("config.json"), r#"{"live_quota":"yes","other":true}"#).unwrap();
    let value = json_output(command(&home).args(["config", "get", "live-quota", "--json"]).assert());
    assert_eq!(value["value"], "off");
    fs::write(home.path().join("config.json"), "broken").unwrap();
    let value = json_output(command(&home).args(["config", "get", "live-quota", "--json"]).assert());
    assert_eq!(value["value"], "off");
}

#[test]
fn config_set_is_refused_inside_a_run_without_creating_file() {
    let home = tempfile::tempdir().unwrap();
    command(&home).env("THOTH_RUN_ID", "01RUN").args(["config", "set", "live-quota", "on"]).assert().failure().stderr(predicates::str::contains("THOTH_RUN_ID"));
    assert!(!home.path().join("config.json").exists());
    json_output(command(&home).env("THOTH_RUN_ID", "01RUN").args(["config", "get", "live-quota", "--json"]).assert());
}

#[test]
fn config_human_notice_goes_to_stderr_and_off_is_silent() {
    let home = tempfile::tempdir().unwrap();
    let on = command(&home).args(["config", "set", "require-reason", "on"]).assert().success().get_output().clone();
    assert!(String::from_utf8_lossy(&on.stderr).contains("require-reason off"));
    assert!(String::from_utf8_lossy(&on.stdout).contains("set require-reason on"));
    let off = command(&home).args(["config", "set", "require-reason", "off"]).assert().success().get_output().clone();
    assert!(off.stderr.is_empty());
}

#[test]
fn config_set_creates_a_nonexistent_thoth_home() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("new-home");
    command_path(&home)
        .args(["config", "set", "live-quota", "on"])
        .assert()
        .success();
    assert!(home.join("config.json").exists());
}

#[test]
fn config_json_notices_are_only_in_json() {
    let home = tempfile::tempdir().unwrap();
    for key in ["scores-auto-refresh", "live-quota"] {
        let output = command(&home)
            .args(["config", "set", key, "on", "--json"])
            .assert()
            .success()
            .get_output()
            .clone();
        assert!(output.stderr.is_empty(), "stderr for {key}: {:?}", output.stderr);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value["notice"].as_str().is_some());
    }
}
