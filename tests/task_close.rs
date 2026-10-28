mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("valid json")
}

fn set_status(f: &Fixture, task_id: &str, status: &str) {
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = ?1 WHERE task_id = ?2",
            [status, task_id],
        )
        .unwrap();
}

fn event_count(f: &Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

fn closed_events(f: &Fixture, task_id: &str) -> Vec<Value> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE task_id = ?1 AND type = 'task_closed' ORDER BY seq")
        .unwrap();
    stmt.query_map([task_id], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

fn updated_at(f: &Fixture, task_id: &str) -> String {
    f.state_conn()
        .query_row("SELECT updated_at FROM tasks WHERE task_id = ?1", [task_id], |r| r.get(0))
        .unwrap()
}

fn new_task(f: &Fixture) -> String {
    f.create_task(Fixture::SPEC).0
}

#[test]
fn task_close_help_shows_reason() {
    let f = common::fixture();
    let out = f
        .cmd()
        .args(["task", "close", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("--reason"), "{text}");
}

#[test]
fn task_close_closes_each_accepted_status_with_a_task_closed_event() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    for from in ["failed", "interrupted", "unverified", "reject", "queued"] {
        let task_id = new_task(&f);
        set_status(&f, &task_id, from);
        f.state_conn()
            .execute(
                "UPDATE tasks SET updated_at = '2000-01-01T00:00:00Z' WHERE task_id = ?1",
                [&task_id],
            )
            .unwrap();
        // A prefix only while this is the repo's one task: tasks created in
        // the same millisecond share their ULID time part.
        let needle = if from == "failed" { &task_id[..8] } else { task_id.as_str() };
        let out = f
            .cmd()
            .args(["task", "close", needle, "--reason", "abandoned in test", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v = json(&out);
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["task_id"], task_id.as_str());
        assert_eq!(v["previous_status"], from);
        assert_eq!(v["status"], "cancelled");
        assert_eq!(v["reason"], "abandoned in test");

        assert_eq!(f.task_status(&task_id), "cancelled");
        assert_ne!(updated_at(&f, &task_id), "2000-01-01T00:00:00Z");
        let events = closed_events(&f, &task_id);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["from"], from);
        assert_eq!(events[0]["reason"], "abandoned in test");
    }
}

#[test]
fn task_close_refuses_every_other_status_and_writes_nothing() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    for from in ["draft", "running", "in_review", "pass", "cancelled", "accepted"] {
        let task_id = new_task(&f);
        set_status(&f, &task_id, from);
        let before = event_count(&f);
        let out = f
            .cmd()
            .args(["task", "close", &task_id, "--reason", "no", "--json"])
            .assert()
            .code(1)
            .get_output()
            .clone();
        let err = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        assert!(err.contains(&format!("'{from}'")), "{from}: {err}");
        for accepted in ["queued", "failed", "interrupted", "unverified", "reject"] {
            assert!(err.contains(accepted), "{from}: {err}");
        }
        assert_eq!(f.task_status(&task_id), from);
        assert_eq!(event_count(&f), before, "{from}: an event was written");
    }
}

const DOCS_SPEC: &str = r#"{"goal":"write the spec","acceptance_criteria":["AC1: the spec is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","task_type":"docs"}"#;
const CODE_SPEC: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","task_type":"code"}"#;

#[test]
fn task_close_closes_an_in_review_document_task() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let task_id = f.create_task(DOCS_SPEC).0;
    set_status(&f, &task_id, "in_review");
    let out = f
        .cmd()
        .args(["task", "close", &task_id, "--reason", "not needed after all", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["previous_status"], "in_review", "{v}");
    assert_eq!(v["status"], "cancelled");
    assert_eq!(f.task_status(&task_id), "cancelled");
    let events = closed_events(&f, &task_id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["from"], "in_review");
}

#[test]
fn task_close_refuses_an_in_review_code_task_and_says_which_tasks_it_takes() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    for spec in [CODE_SPEC, Fixture::SPEC] {
        let task_id = f.create_task(spec).0;
        set_status(&f, &task_id, "in_review");
        let before = event_count(&f);
        let out = f
            .cmd()
            .args(["task", "close", &task_id, "--reason", "no"])
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        let err = String::from_utf8_lossy(&out).to_string();
        assert!(err.contains("'in_review'"), "{err}");
        assert!(err.contains("docs, research or review"), "{err}");
        assert_eq!(f.task_status(&task_id), "in_review");
        assert_eq!(event_count(&f), before);
    }
}

#[test]
fn task_close_refuses_while_a_run_is_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", Fixture::SPEC);
    set_status(&f, &task_id, "failed");
    let before = event_count(&f);
    let out = f
        .cmd()
        .args(["task", "close", &task_id, "--reason", "no"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    let expected = format!(
        "task close: run {run_id} (implementer) of task {task_id} is still 'queued'"
    );
    assert!(err.contains(&expected), "{err}");
    assert_eq!(f.task_status(&task_id), "failed");
    assert_eq!(event_count(&f), before);
}

#[test]
fn task_close_refuses_an_empty_reason() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let task_id = new_task(&f);
    set_status(&f, &task_id, "failed");
    for reason in ["", "   \t "] {
        let before = event_count(&f);
        f.cmd()
            .args(["task", "close", &task_id, "--reason", reason, "--json"])
            .assert()
            .code(1);
        assert_eq!(f.task_status(&task_id), "failed");
        assert_eq!(event_count(&f), before);
    }
}

#[test]
fn task_close_removes_a_clean_worktree_and_keeps_its_branch() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    f.state_conn()
        .execute("UPDATE runs SET status = 'failed' WHERE run_id = ?1", [&run_id])
        .unwrap();
    set_status(&f, &task_id, "failed");
    let out = f
        .cmd()
        .args(["task", "close", &task_id, "--reason", "gone", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(f.task_status(&task_id), "cancelled");
    assert!(!wt.exists(), "worktree {} was not removed", wt.display());
    let list = common::git(&f.repo_path(), &["worktree", "list"]);
    let name = wt.file_name().unwrap().to_string_lossy().to_string();
    assert!(!list.contains(&name), "{list}");
    let branches = common::git(
        &f.repo_path(),
        &["branch", "--list", &format!("thoth/{task_id}/a1")],
    );
    assert!(!branches.is_empty(), "the branch must stay");
    let events = f.events_of(&task_id);
    assert_eq!(
        events.iter().filter(|e| *e == "worktree_removed").count(),
        1,
        "{events:?}"
    );
    assert!(!events.iter().any(|e| e == "worktree_kept"), "{events:?}");
    let v = json(&out);
    let removed = v["worktrees"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{v}");
    assert_eq!(removed[0]["path"], wt.display().to_string());
    assert_eq!(removed[0]["kind"], "worker");
    assert_eq!(removed[0]["branch"], format!("thoth/{task_id}/a1"));
    assert_eq!(v["worktrees"]["kept"], serde_json::json!([]));
}

#[test]
fn a_closed_task_leaves_status_and_counts_as_cancelled_in_the_report() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let task_id = new_task(&f);
    set_status(&f, &task_id, "failed");

    let out = f.cmd().args(["status", "--json"]).assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&out).contains(&task_id));

    f.cmd()
        .args(["task", "close", &task_id, "--reason", "gone", "--json"])
        .assert()
        .success();

    let out = f.cmd().args(["status", "--json"]).assert().success().get_output().stdout.clone();
    assert!(!String::from_utf8_lossy(&out).contains(&task_id), "{}", String::from_utf8_lossy(&out));

    let out = f.cmd().args(["report", "--json"]).assert().success().get_output().stdout.clone();
    let page = fs::read_to_string(json(&out)["path"].as_str().unwrap()).unwrap();
    assert!(page.contains("<b class=\"n bad\">0</b> 件需要處理"), "{page}");
    assert!(page.contains("<span class=\"pill muted\">已取消</span>"), "{page}");
}
