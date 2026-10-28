//! DOC-1: a docs, research or review task is closed with `task accept`, and
//! `task result` shows what its worker delivered. Always the fake claude and a
//! temporary THOTH_HOME.

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;

const SUMMARY: &str = "fake worker completed the task";

fn spec(task_type: &str) -> String {
    format!(
        r#"{{"goal":"write the {task_type} document","acceptance_criteria":["AC1: the document is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"{task_type}"}}"#
    )
}

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

/// Creates a task of `task_type`, dispatches the fake's `ok` scenario and
/// waits for `in_review`. Returns (task_id, run_id).
fn delivered(f: &Fixture, task_type: &str) -> (String, String) {
    let (task_id, _) = f.create_task(&spec(task_type));
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    f.wait_for_task_status(&task_id, &["in_review"], 120);
    // The runner settles the run before it writes the task status; wait for
    // the run row too, so nothing below races a live run.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while run_status(f, &run_id) != "completed" {
        assert!(std::time::Instant::now() < deadline, "run {run_id} never completed");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    (task_id, run_id)
}

fn json_ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_slice(&out).expect("valid json")
}

/// Runs a command that must exit 1 and returns its stderr.
fn refused(f: &Fixture, args: &[&str]) -> String {
    let out = f.cmd().args(args).assert().code(1).get_output().clone();
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn set_status(f: &Fixture, task_id: &str, status: &str) {
    f.state_conn()
        .execute("UPDATE tasks SET status = ?1 WHERE task_id = ?2", [status, task_id])
        .unwrap();
}

fn run_status(f: &Fixture, run_id: &str) -> String {
    f.state_conn()
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap()
}

fn event_count(f: &Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

fn review_rows(f: &Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap()
}

/// (a) the whole path: read the delivery, accept it, and every entry that
/// would start more work or record a decision refuses afterwards.
#[test]
fn a_docs_task_is_read_accepted_and_then_closed_to_everything() {
    let f = setup();
    let (task_id, run_id) = delivered(&f, "docs");

    let r = json_ok(&f, &["task", "result", &task_id, "--json"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["task_id"], task_id.as_str());
    assert_eq!(r["task_status"], "in_review");
    assert_eq!(r["run_id"], run_id.as_str());
    assert_eq!(r["attempt"], 1);
    assert_eq!(r["role"], "implementer");
    assert_eq!(r["profile"], "claude-b");
    assert_eq!(r["model"], "sonnet");
    assert!(r["candidate_commit"].as_str().is_some(), "{r}");
    assert!(r["worktree"].as_str().is_some(), "{r}");
    assert!(r["ingested_at"].as_str().is_some(), "{r}");
    assert_eq!(r["result_status"], "completed");
    assert_eq!(r["summary"], SUMMARY);
    assert_eq!(r["claim_count"], 1);
    assert_eq!(r["claims"][0]["kind"], "completion");
    assert_eq!(r["claims"][0]["text"], "fake worker finished");
    assert_eq!(r["claims"][0]["evidence"], serde_json::json!([]));
    let candidate = r["candidate_commit"].clone();

    // The human form: the summary under `summary:` and one line per claim.
    let mut c = f.cmd();
    c.args(["task", "result", &task_id]);
    let text = String::from_utf8(f.run_ok(c, "task result")).unwrap();
    assert!(text.starts_with(&format!("{task_id}  in_review  ")), "{text}");
    assert!(text.contains(&format!("\nsummary:\n  {SUMMARY}\n")), "{text}");
    assert!(text.contains("claims (1):\n- completion: fake worker finished []"), "{text}");

    let events_before = event_count(&f);
    let a = json_ok(&f, &["task", "accept", &task_id, "--reason", "spec read and used", "--json"]);
    assert_eq!(a["ok"], true, "{a}");
    assert_eq!(a["task_id"], task_id.as_str());
    assert_eq!(a["previous_status"], "in_review");
    assert_eq!(a["status"], "accepted");
    assert_eq!(a["reason"], "spec read and used");
    assert_eq!(a["run_id"], run_id.as_str());
    assert_eq!(a["result_status"], "completed");
    assert_eq!(a["candidate_commit"], candidate);
    assert!(a["worktrees"]["removed"].is_array(), "{a}");
    assert!(a["worktrees"]["kept"].is_array(), "{a}");
    assert!(event_count(&f) > events_before);

    let ev = json_ok(&f, &["events", "--task", &task_id, "--json"]);
    let accepted: Vec<&Value> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "task_accepted")
        .collect();
    assert_eq!(accepted.len(), 1, "{ev}");
    let d = &accepted[0]["detail"];
    assert_eq!(d["from"], "in_review");
    assert_eq!(d["reason"], "spec read and used");
    assert_eq!(d["task_type"], "docs");
    assert_eq!(d["run_id"], run_id.as_str());
    assert_eq!(d["result_status"], "completed");
    assert_eq!(d["candidate_commit"], candidate);
    assert!(
        !ev["events"].as_array().unwrap().iter().any(|e| e["type"] == "verdict"),
        "{ev}"
    );

    let s = json_ok(&f, &["status", &task_id, "--json"]);
    assert_eq!(s["status"], "accepted");
    assert_eq!(s["review_count"], 0);
    assert!(s["latest_verdict"].is_null(), "{s}");
    let all = json_ok(&f, &["status", "--json"]);
    assert!(
        !all["tasks"].as_array().unwrap().iter().any(|t| t["task_id"] == task_id.as_str()),
        "{all}"
    );

    // Still readable after the task is closed.
    let again = json_ok(&f, &["task", "result", &task_id, "--json"]);
    assert_eq!(again["summary"], SUMMARY);
    assert_eq!(again["task_status"], "accepted");

    // Nothing that starts work or records a decision takes it any more.
    let spec_file = f.home_path().join("new-spec.json");
    fs::write(&spec_file, spec("docs")).unwrap();
    let pass_doc = {
        let list = json_ok(&f, &["evidence", "list", &task_id, "--json"]);
        let code = list["evidence"][0]["code"].as_str().expect("an evidence code").to_string();
        let path = f.home_path().join("pass.json");
        fs::write(
            &path,
            format!(
                r#"{{"verdict":"PASS","findings":{{"criteria":[{{"id":"AC1","verdict":"met","evidence":["{code}"]}}],"extra_findings":[]}}}}"#
            ),
        )
        .unwrap();
        path
    };
    let spec_path = spec_file.display().to_string();
    let pass_path = pass_doc.display().to_string();
    let reviews_before = review_rows(&f);
    let attempts: Vec<Vec<&str>> = vec![
        vec!["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"],
        vec!["task", "update-spec", &task_id, "-f", &spec_path],
        vec!["task", "close", &task_id, "--reason", "no"],
        vec!["task", "accept", &task_id, "--reason", "again"],
        vec!["review", "record", &task_id, "-f", &pass_path],
        vec!["spec", "review", "start", &task_id, "--profile", "claude-b", "--model", "haiku", "--effort", "medium"],
    ];
    for args in attempts {
        let err = refused(&f, &args);
        assert!(err.contains("'accepted'"), "{args:?}: {err}");
        assert_eq!(f.task_status(&task_id), "accepted", "{args:?}");
    }
    assert_eq!(review_rows(&f), reviews_before, "review record wrote a reviews row");
}

/// (b) a code task is not taken: it waits for a review.
#[test]
fn a_code_task_is_refused_by_accept_and_by_close_from_in_review() {
    let f = setup();
    let (task_id, _) = delivered(&f, "code");
    let before = event_count(&f);
    let err = refused(&f, &["task", "accept", &task_id, "--reason", "x", "--json"]);
    assert!(err.contains("review start") && err.contains("review record"), "{err}");
    let err = refused(&f, &["task", "close", &task_id, "--reason", "x"]);
    assert!(err.contains("docs, research or review"), "{err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(event_count(&f), before);
}

/// (c) research and review tasks are accepted; any status but in_review is refused.
#[test]
fn research_and_review_tasks_are_accepted_and_only_from_in_review() {
    let f = setup();
    for task_type in ["research", "review"] {
        let (task_id, _) = f.create_task(&spec(task_type));
        set_status(&f, &task_id, "in_review");
        let a = json_ok(&f, &["task", "accept", &task_id, "--reason", "read", "--json"]);
        assert_eq!(a["status"], "accepted", "{task_type}: {a}");
        assert!(a["run_id"].is_null(), "no run delivered anything: {a}");
        assert!(a["result_status"].is_null(), "{a}");
        assert!(a["candidate_commit"].is_null(), "{a}");
        assert_eq!(f.task_status(&task_id), "accepted");
    }
    let (task_id, _) = f.create_task(&spec("docs"));
    for status in [
        "draft", "queued", "running", "reject", "unverified", "interrupted", "failed", "pass",
        "cancelled", "accepted",
    ] {
        set_status(&f, &task_id, status);
        let before = event_count(&f);
        let err = refused(&f, &["task", "accept", &task_id, "--reason", "x", "--json"]);
        assert!(err.contains(&format!("'{status}'")), "{status}: {err}");
        assert!(err.contains("in_review"), "{status}: {err}");
        assert_eq!(f.task_status(&task_id), status);
        assert_eq!(event_count(&f), before, "{status}: an event was written");
    }
}

/// (d) a live run blocks the accept, with the shared live-run wording.
#[test]
fn accept_refuses_while_a_run_is_live() {
    let f = setup();
    let (task_id, run_id) = delivered(&f, "docs");
    f.state_conn()
        .execute("UPDATE runs SET status = 'queued' WHERE run_id = ?1", [&run_id])
        .unwrap();
    let before = event_count(&f);
    let err = refused(&f, &["task", "accept", &task_id, "--reason", "x"]);
    let expected = format!("task accept: run {run_id} (implementer) of task {task_id} is still 'queued'");
    assert!(err.contains(&expected), "{err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(event_count(&f), before);
}

/// (e) and `--run`: no result before a run delivered one; a run of another
/// task is refused.
#[test]
fn result_needs_an_ingested_result_of_this_task() {
    let f = setup();
    let (fresh, _) = f.create_task(&spec("docs"));
    let err = refused(&f, &["task", "result", &fresh, "--json"]);
    assert!(err.contains("'queued'"), "{err}");
    assert!(err.contains("result.json"), "{err}");
    assert!(err.contains(&format!("thoth events --task {fresh} --json")), "{err}");

    let (task_id, run_id) = delivered(&f, "docs");
    let r = json_ok(&f, &["task", "result", &task_id, "--run", &run_id, "--json"]);
    assert_eq!(r["run_id"], run_id.as_str());
    assert_eq!(r["summary"], SUMMARY);
    let err = refused(&f, &["task", "result", &fresh, "--run", &run_id, "--json"]);
    assert!(err.contains(&task_id), "{err}");
}

/// (f) a delivered docs task can still be abandoned instead.
#[test]
fn close_cancels_an_in_review_docs_task() {
    let f = setup();
    let (task_id, _) = delivered(&f, "docs");
    let v = json_ok(&f, &["task", "close", &task_id, "--reason", "not needed", "--json"]);
    assert_eq!(v["previous_status"], "in_review", "{v}");
    assert_eq!(v["status"], "cancelled");
    let ev = json_ok(&f, &["events", "--task", &task_id, "--json"]);
    let closed: Vec<&Value> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "task_closed")
        .collect();
    assert_eq!(closed.len(), 1, "{ev}");
    assert_eq!(closed[0]["detail"]["from"], "in_review");
}

/// (g) the reason is required.
#[test]
fn accept_refuses_an_empty_reason() {
    let f = setup();
    let (task_id, _) = f.create_task(&spec("docs"));
    set_status(&f, &task_id, "in_review");
    for reason in ["", "   \t "] {
        let before = event_count(&f);
        refused(&f, &["task", "accept", &task_id, "--reason", reason, "--json"]);
        assert_eq!(f.task_status(&task_id), "in_review");
        assert_eq!(event_count(&f), before);
    }
}

/// (h) an older interrupted run of an accepted task is not resumed.
#[test]
fn resume_refuses_a_run_of_an_accepted_task() {
    let f = setup();
    let (task_id, run_id) = delivered(&f, "docs");
    json_ok(&f, &["task", "accept", &task_id, "--reason", "done", "--json"]);
    f.state_conn()
        .execute(
            "UPDATE runs SET status = 'interrupted', interrupted_reason = 'crash' WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    let err = refused(&f, &["resume", &run_id, "--json"]);
    assert!(err.contains("not pass, cancelled or accepted"), "{err}");
    assert_eq!(f.task_status(&task_id), "accepted");
    assert_eq!(run_status(&f, &run_id), "interrupted");
}

/// (i) a worker cannot take its own delivery.
#[test]
fn accept_refuses_inside_a_run() {
    let f = setup();
    let (task_id, _) = f.create_task(&spec("docs"));
    set_status(&f, &task_id, "in_review");
    let before = event_count(&f);
    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01ANYRUNIDANYRUNIDANYRUN01")
        .args(["task", "accept", &task_id, "--reason", "x", "--json"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("THOTH_RUN_ID"), "{err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(event_count(&f), before);
    let a = json_ok(&f, &["task", "accept", &task_id, "--reason", "x", "--json"]);
    assert_eq!(a["status"], "accepted", "{a}");
}

/// AC8: an accepted run is used, not judged - records carry no verdict for
/// it and `thoth models` counts it as a completed run with no review.
#[test]
fn an_accepted_run_has_no_verdict_in_records_or_models() {
    let f = setup();
    let (task_id, run_id) = delivered(&f, "docs");
    json_ok(&f, &["task", "accept", &task_id, "--reason", "used it", "--json"]);

    let dir = f.home_path().join("records");
    fs::create_dir_all(&dir).unwrap();
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(&dir)
        .arg("--json")
        .assert()
        .success();
    f.cmd()
        .args(["config", "set", "machine-name", "alpha", "--json"])
        .assert()
        .success();
    let v = json_ok(&f, &["records", "export", "--json"]);
    assert_eq!(v["ok"], true, "{v}");
    let text = fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
    let line: Value = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|l| l["run_id"] == run_id.as_str())
        .unwrap_or_else(|| panic!("no line for {run_id}: {text}"));
    assert!(line["verdict"].is_null(), "{line}");
    assert_eq!(line["task_type"], "docs", "{line}");
    assert_eq!(line["status"], "completed", "{line}");

    // No key and no --refresh, and an empty PATH, so nothing reaches the network.
    let empty = tempfile::tempdir().unwrap();
    let out = f
        .cmd()
        .env("PATH", empty.path())
        .args(["models", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let m: Value = serde_json::from_slice(&out).unwrap();
    let profile = m["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "claude-b")
        .unwrap_or_else(|| panic!("{m}"));
    let model = profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["model"] == "sonnet")
        .unwrap_or_else(|| panic!("{profile}"));
    let docs = model["thoth_breakdown"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["task_type"] == "docs")
        .unwrap_or_else(|| panic!("{model}"));
    assert_eq!(docs["implementer_runs"], 1, "{docs}");
    assert_eq!(docs["completed"], 1, "{docs}");
    assert_eq!(docs["reviewed"], 0, "{docs}");
    // RJ-3 (AC10): `thoth models --json` has no pass or reject counts
    assert!(docs.get("pass").is_none(), "{docs}");
    assert!(docs.get("reject").is_none(), "{docs}");
}
