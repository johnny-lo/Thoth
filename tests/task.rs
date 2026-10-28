mod common;

use predicates::prelude::*;

const SPEC: &str = r#"{
  "goal": "Add a retry to the uploader",
  "acceptance_criteria": ["AC1: retries three times", "AC2: logs each attempt"],
  "constraints": ["no new dependencies"],
  "non_goals": ["rewriting the transport"],
  "excluded_alternatives": ["backoff crate - evaluated, too heavy"],
  "risk": "low"
}"#;

fn write_spec(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("spec.json");
    std::fs::write(&p, SPEC).unwrap();
    p
}

#[test]
fn task_create_returns_a_ulid_and_records_base_commit_and_an_event() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let spec = write_spec(&f.home_path());
    let head = common::git(&f.repo_path(), &["rev-parse", "HEAD"]);

    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec)
        .args(["--title", "uploader retry", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let task_id = v["task_id"].as_str().unwrap().to_string();
    assert_eq!(task_id.len(), 26, "task_id must be a ULID: {task_id}");

    let conn = f.state_conn();
    let (status, base, risk, title): (String, String, String, String) = conn
        .query_row(
            "SELECT status, base_commit, risk, title FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(base, head);
    assert_eq!(risk, "low");
    assert_eq!(title, "uploader retry");

    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert_eq!(kinds, vec!["task_created".to_string()]);
}

#[test]
fn task_show_accepts_a_six_char_prefix() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let spec = write_spec(&f.home_path());
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let task_id = v["task_id"].as_str().unwrap().to_string();

    f.cmd()
        .args(["task", "show", &task_id[..6], "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&task_id))
        .stdout(predicate::str::contains("Add a retry to the uploader"));
}

#[test]
fn task_show_rejects_a_too_short_prefix() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd()
        .args(["task", "show", "01A"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("at least 6 characters"));
}

#[test]
fn task_create_rejects_an_invalid_spec() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let bad = f.home_path().join("bad.json");
    std::fs::write(&bad, r#"{"goal":"g","acceptance_criteria":[]}"#).unwrap();
    f.cmd()
        .args(["task", "create", "-f"])
        .arg(&bad)
        .assert()
        .failure()
        .stderr(predicate::str::contains("at least one entry"));

    let conn = f.state_conn();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "a rejected spec must not create a task row");
}
