//! REV-1: blocking and advisory findings. The verdict stays the reviewer's
//! (Thoth neither changes nor refuses it for its categories), and every code
//! review's `verdict` event records `has_blocking_findings`; a research
//! source check records null. Checked through `review record --json`,
//! `review show --json`, `wait --run --json` and `events --task --json`.
mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::Path;

/// Runs the `hooks` implementer scenario to completion and returns the task
/// id with the task sitting at `in_review` (as in tests/review_record.rs).
fn candidate_ready(f: &Fixture) -> String {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review");
    task_id
}

fn json_ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    serde_json::from_slice(&f.run_ok(c, &args.join(" "))).unwrap()
}

/// `review record TASK -f FILE --json` with `doc` written to a file.
fn record(f: &Fixture, task_id: &str, doc: &Value) -> Value {
    let file = f.home_path().join("review.json");
    std::fs::write(&file, doc.to_string()).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", task_id, "-f"]).arg(&file).arg("--json");
    serde_json::from_slice(&f.run_ok(c, "review record")).unwrap()
}

/// The `has_blocking_findings` of the task's only `verdict` event, as
/// `events --task TASK --json` shows it; panics when the key is missing.
fn blocking_flag(f: &Fixture, task_id: &str) -> Value {
    let verdicts: Vec<Value> = f
        .events_json(task_id)
        .into_iter()
        .filter(|e| e["type"] == "verdict")
        .collect();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    verdicts[0]["detail"]
        .get("has_blocking_findings")
        .cloned()
        .unwrap_or_else(|| panic!("no has_blocking_findings in {}", verdicts[0]))
}

fn met(id: &str) -> Value {
    json!({"id": id, "verdict": "met", "evidence": ["E001"]})
}

fn extra(category: Option<&str>) -> Value {
    let mut e = json!({"severity": "major", "note": "a finding", "evidence": ["E001"], "fix_size": "S"});
    if let Some(c) = category {
        e["category"] = json!(c);
    }
    e
}

/// Records `doc` on a fresh candidate and returns (`review record --json`,
/// the verdict event's `has_blocking_findings`).
fn record_on_candidate(doc: Value) -> (Value, Value) {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let v = record(&f, &task_id, &doc);
    let flag = blocking_flag(&f, &task_id);
    (v, flag)
}

/// (a) AC4: a REJECT whose findings are all advisory is taken as it is.
#[test]
fn a_reject_with_only_advisory_findings_settles_reject_without_a_blocking_finding() {
    let (v, flag) = record_on_candidate(json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "note": "a case is missing", "fix_size": "S", "category": "test_incomplete"},
            met("AC2"),
        ],
        "extra_findings": [extra(Some("docs")), extra(Some("test_interface")), extra(Some("evidence")), extra(Some("test_incomplete"))],
    }}));
    assert_eq!(v["verdict"], "REJECT", "{v}");
    assert_eq!(v["task_status"], "reject", "{v}");
    assert_eq!(flag, json!(false));
}

/// (b) one blocking extra finding among advisory ones.
#[test]
fn an_error_path_extra_finding_is_blocking() {
    let (v, flag) = record_on_candidate(json!({"verdict": "REJECT", "findings": {
        "criteria": [met("AC1"), met("AC2")],
        "extra_findings": [extra(Some("docs")), extra(Some("error_path"))],
    }}));
    assert_eq!(v["task_status"], "reject", "{v}");
    assert_eq!(flag, json!(true));
}

/// (c) a finding with no category counts as blocking.
#[test]
fn an_extra_finding_without_a_category_is_blocking() {
    let (v, flag) = record_on_candidate(json!({"verdict": "REJECT", "findings": {
        "criteria": [met("AC1"), met("AC2")],
        "extra_findings": [extra(Some("docs")), extra(None)],
    }}));
    assert_eq!(v["task_status"], "reject", "{v}");
    assert_eq!(flag, json!(true));
}

/// (d) an unmet criterion labelled test_missing is blocking.
#[test]
fn an_unmet_criterion_labelled_test_missing_is_blocking() {
    let (v, flag) = record_on_candidate(json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "note": "no test fails on the old code", "fix_size": "M", "category": "test_missing"},
            met("AC2"),
        ],
        "extra_findings": [],
    }}));
    assert_eq!(v["task_status"], "reject", "{v}");
    assert_eq!(flag, json!(true));
}

/// (e) a PASS with an advisory note settles pass.
#[test]
fn a_pass_with_a_docs_extra_finding_settles_pass_without_a_blocking_finding() {
    let (v, flag) = record_on_candidate(json!({"verdict": "PASS", "findings": {
        "criteria": [met("AC1"), met("AC2")],
        "extra_findings": [extra(Some("docs"))],
    }}));
    assert_eq!(v["verdict"], "PASS", "{v}");
    assert_eq!(v["task_status"], "pass", "{v}");
    assert_eq!(flag, json!(false));
}

/// AC4: a PASS that carries a blocking extra finding is still taken as a PASS;
/// only the event says a blocking finding came with it.
#[test]
fn a_pass_with_a_blocking_extra_finding_settles_pass_and_is_flagged() {
    let (v, flag) = record_on_candidate(json!({"verdict": "PASS", "findings": {
        "criteria": [met("AC1"), met("AC2")],
        "extra_findings": [extra(Some("security"))],
    }}));
    assert_eq!(v["verdict"], "PASS", "{v}");
    assert_eq!(v["task_status"], "pass", "{v}");
    assert_eq!(flag, json!(true));
}

/// (f) test_incomplete is a category `review record` takes, in any case, and
/// `review show` hands it back lower-cased.
#[test]
fn test_incomplete_is_accepted_and_stored_lower_case() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let v = record(&f, &task_id, &json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "note": "one branch untested", "fix_size": "S", "category": " Test_Incomplete "},
            met("AC2"),
        ],
        "extra_findings": [extra(Some("TEST_INCOMPLETE"))],
    }}));
    assert_eq!(v["task_status"], "reject", "{v}");
    let shown = json_ok(&f, &["review", "show", &task_id, "--json"]);
    assert_eq!(shown["findings"]["criteria"][0]["category"], "test_incomplete", "{shown}");
    assert_eq!(shown["findings"]["extra_findings"][0]["category"], "test_incomplete", "{shown}");
    assert_eq!(blocking_flag(&f, &task_id), json!(false));
}

/// (g) a reviewer run: fake-claude's `review_reject` labels its findings with
/// the first name on the prompt's category line (spec_mismatch, blocking).
#[test]
fn a_reviewer_runs_reject_with_a_blocking_finding_is_flagged() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let r = f.review_start(&task_id, "claude-a", "review_reject", &["--model", "haiku"]);
    let run = r["run_id"].as_str().unwrap();
    let w = json_ok(&f, &["wait", "--run", run, "--timeout", "180s", "--json"]);
    assert_eq!(w["verdict"], "REJECT", "{w}");
    assert_eq!(f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30), "reject");
    assert_eq!(blocking_flag(&f, &task_id), json!(true));
}

fn file_url(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    let s = s.replace('%', "%25").replace(' ', "%20").replace('#', "%23");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// (h) a research task's source check does not split findings, so its
/// verdict event records null, even next to an uncategorised extra finding.
#[test]
fn a_source_check_verdict_records_null() {
    const SPEC: &str = r#"{"goal":"research retries","task_type":"research","acceptance_criteria":["AC1: report retry behaviour"],"constraints":[],"non_goals":[],"excluded_alternatives":[]}"#;
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    // fake-claude's research_ok gives its first research claim the first
    // source and its second claim the rest, so it needs two.
    let urls: Vec<String> = (1..=2)
        .map(|i| {
            let page = f.home_path().join(format!("page{i}.txt"));
            std::fs::write(&page, format!("prefix\nquote {i}\nsuffix")).unwrap();
            file_url(&page)
        })
        .collect();
    let sources = json!([{"url": urls[0], "quote": "quote 1"}, {"url": urls[1], "quote": "quote 2"}]);
    let (task_id, _) = f.create_task(SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_SOURCES", sources.to_string())
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch research")).unwrap();
    let w = json_ok(&f, &["wait", "--run", d["run_id"].as_str().unwrap(), "--timeout", "120s", "--json"]);
    assert_eq!(w["task_status"], "in_review", "{w}\n{}", f.explain_task(&task_id));

    let v = record(&f, &task_id, &json!({"verdict": "PASS", "findings": {
        "sources": [
            {"id": "S1", "url": urls[0], "verdict": "supported", "reason": ""},
            {"id": "S2", "url": urls[1], "verdict": "supported", "reason": ""},
        ],
        "extra_findings": [{"severity": "major", "note": "n", "evidence": []}],
    }}));
    assert_eq!(v["task_status"], "pass", "{v}");
    assert_eq!(blocking_flag(&f, &task_id), Value::Null);
}
