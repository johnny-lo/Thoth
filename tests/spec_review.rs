//! SR-1: `thoth spec review record|show`, the first-dispatch gate for
//! important/high tasks, the approval bound to the spec bytes, and the stop
//! after three REVISE in a row.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const IMPORTANT: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;
const HIGH: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"high"}"#;
const APPROVE_DOC: &str = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]},{"id":"AC2","verdict":"ok","findings":[{"check":"steps","location":"spec","note":"AC2 could name the log line to look for"}]}]}"#;
const REVISE_DOC: &str = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"revise","findings":[{"check":"exists","location":"src/ledger.rs:40","note":"there is no reject_double_spend function"}]},{"id":"AC2","verdict":"ok","findings":[]}]}"#;
const REWRITTEN: &str = r#"{"goal":"guard the ledger, rejecting a double spend before it is written","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;

/// `COUNT(*)` of `task_id`'s events of exactly one `type`.
fn count_events(f: &Fixture, task_id: &str, event_type: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND type = ?2",
            [task_id, event_type],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(run_id, detail)` of every event of one type of a task, in seq order.
fn event_rows(f: &Fixture, task_id: &str, event_type: &str) -> Vec<(Option<String>, Value)> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT run_id, detail FROM events WHERE task_id=?1 AND type=?2 ORDER BY seq")
        .unwrap();
    stmt.query_map([task_id, event_type], |r| {
        let detail: String = r.get(1)?;
        Ok((r.get::<_, Option<String>>(0)?, serde_json::from_str(&detail).unwrap()))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn last_event(f: &Fixture, task_id: &str, event_type: &str) -> (Option<String>, Value) {
    event_rows(f, task_id, event_type).pop().unwrap()
}

fn write_json(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

fn record_ok(f: &Fixture, task: &str, file: &Path) -> Value {
    let mut c = f.cmd();
    c.args(["spec", "review", "record", task, "-f"]).arg(file).arg("--json");
    let out = f.run_ok(c, "spec review record");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn record_err(f: &Fixture, task: &str, file: &Path) -> String {
    let out = f
        .cmd()
        .args(["spec", "review", "record", task, "-f"])
        .arg(file)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(out).unwrap()
}

fn record_human(f: &Fixture, task: &str, file: &Path) -> String {
    let mut c = f.cmd();
    c.args(["spec", "review", "record", task, "-f"]).arg(file);
    String::from_utf8(f.run_ok(c, "spec review record")).unwrap()
}

fn show_json(f: &Fixture, task: &str) -> Value {
    let mut c = f.cmd();
    c.args(["spec", "review", "show", task, "--json"]);
    let out = f.run_ok(c, "spec review show");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn show_human(f: &Fixture, task: &str) -> String {
    let out = f
        .cmd()
        .args(["spec", "review", "show", task])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

fn dispatch_err(f: &Fixture, task: &str, extra: &[&str]) -> String {
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", task)
        .args(["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .args(extra)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(out).unwrap()
}

fn run_count(f: &Fixture, task: &str) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE task_id = ?1", [task], |r| r.get(0))
        .unwrap()
}

fn sha(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

fn update_spec_json(f: &Fixture, task: &str, file: &Path) -> Value {
    let mut c = f.cmd();
    c.args(["task", "update-spec", task, "-f"]).arg(file).arg("--json");
    let out = f.run_ok(c, "task update-spec");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn set_status(f: &Fixture, task: &str, status: &str) {
    f.state_conn()
        .execute("UPDATE tasks SET status = ?2 WHERE task_id = ?1", [task, status])
        .unwrap();
}

fn standard() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

fn worktree_a1(f: &Fixture, task: &str) -> PathBuf {
    f.home_path()
        .join("repos")
        .join(f.repo_id())
        .join("worktrees")
        .join(format!("{task}-a1"))
}

/// A `thoth` for the background, with `f.cmd()`'s environment, that pauses
/// at the debug-build test seam `pause_var` until `pause` is deleted.
fn paused_cmd(f: &Fixture, pause_var: &str, pause: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    for var in [
        "THOTH_RUN_ID",
        "THOTH_INBOX_DIR",
        "THOTH_EXE",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "ARTIFICIALANALYSIS_API_KEY",
        "AISTUPIDLEVEL_API_KEY",
        "THOTH_RECORDS_DIR",
        "THOTH_MACHINE_NAME",
    ] {
        c.env_remove(var);
    }
    c.current_dir(f.repo_path())
        .env("THOTH_HOME", f.home_path())
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
        .env(pause_var, pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

/// Waits until `child` has created `pause`; panics if it exits first.
fn wait_for_pause(child: &mut Child, pause: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !pause.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            panic!("thoth exited {status} before it paused: {err}");
        }
        assert!(std::time::Instant::now() < deadline, "thoth never paused");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn an_approve_of_the_current_spec_lets_an_important_task_dispatch() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let r = record_ok(&f, &task, &approve);
    assert_eq!(r["verdict"], "APPROVE");
    assert_eq!(r["approved"], true);
    assert_eq!(r["required_before_dispatch"], true);
    assert_eq!(r["revise_streak"], 0);
    assert_eq!(r["stalled"], false);
    assert_eq!(r["source"], "external");
    assert_eq!(r["task_status"], "queued");
    assert_eq!(r["spec_sha256"], sha(IMPORTANT));
    assert_eq!(
        sorted_keys(&r),
        [
            "approved",
            "required_before_dispatch",
            "revise_streak",
            "risk",
            "source",
            "spec_review_id",
            "spec_sha256",
            "stalled",
            "task_id",
            "task_status",
            "verdict"
        ]
    );
    let id = r["spec_review_id"].as_str().unwrap().to_string();

    assert_eq!(count_events(&f, &task, "spec_review"), 1);
    let (run_id, detail) = last_event(&f, &task, "spec_review");
    assert_eq!(detail["verdict"], "APPROVE");
    assert_eq!(detail["spec_sha256"], sha(IMPORTANT));
    assert_eq!(
        detail["criteria"],
        json!([{"id":"AC1","verdict":"ok","findings":0},{"id":"AC2","verdict":"ok","findings":1}])
    );
    assert_eq!(detail["findings"], 1);
    assert_eq!(detail["source"], "external");
    assert!(detail["reviewer_run_id"].is_null());
    assert_eq!(detail["document"]["verdict"], "APPROVE");
    assert_eq!(run_id, None);

    let s = show_json(&f, &task);
    assert_eq!(s["approved"], true);
    assert_eq!(s["review_count"], 1);
    assert_eq!(s["newest"]["current"], true);
    assert_eq!(s["newest"]["spec_review_id"], id.as_str());
    assert_eq!(s["required_before_dispatch"], true);

    let d = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(
        d["spec_review"],
        json!({"required":true,"approved":true,"skipped":false,"spec_review_id":id})
    );
    assert_eq!(d["warnings"], json!([]));
    assert_eq!(count_events(&f, &task, "spec_review_skipped"), 0);
    f.wait_for(&task, "120s");
}

#[test]
fn an_important_task_with_no_spec_review_is_refused_and_nothing_is_written() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let err = dispatch_err(&f, &task, &[]);
    assert!(err.contains("has no APPROVE spec review (no spec review is recorded)"), "{err}");
    assert!(err.contains("--skip-spec-review"), "{err}");
    assert!(err.contains(&task), "{err}");
    assert!(err.contains("thoth spec review record"), "{err}");
    assert_eq!(run_count(&f, &task), 0);
    assert_eq!(f.task_status(&task), "queued");
    assert_eq!(f.events_of(&task), ["task_created"]);
    let wt = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("worktrees")
        .join(format!("{task}-a1"));
    assert!(!wt.exists(), "{}", wt.display());

    let err = dispatch_err(&f, &task, &["--skip-spec-review", "  "]);
    assert!(err.contains("--skip-spec-review must not be empty"), "{err}");
    assert_eq!(run_count(&f, &task), 0);

    let d = f.dispatch(
        &task,
        "claude-b",
        "crash",
        &["--skip-spec-review", "low blast radius, user said go"],
    );
    assert_eq!(
        d["spec_review"],
        json!({"required":true,"approved":false,"skipped":true,"spec_review_id":null})
    );
    assert_eq!(d["warnings"], json!([]));
    assert_eq!(count_events(&f, &task, "spec_review_skipped"), 1);
    let (run_id, detail) = last_event(&f, &task, "spec_review_skipped");
    assert_eq!(
        detail,
        json!({
            "reason": "low blast radius, user said go",
            "required": true,
            "risk": "important",
            "spec_sha256": sha(IMPORTANT),
            "newest_spec_review": null,
            "newest_verdict": null,
            "newest_current": false
        })
    );
    assert_eq!(run_id.as_deref(), d["run_id"].as_str());
    let types = f.events_of(&task);
    let at = |t: &str| types.iter().position(|x| x == t).unwrap();
    assert!(at("spec_review_skipped") > at("worktree_created"), "{types:?}");
    assert!(at("spec_review_skipped") < at("launch_choice"), "{types:?}");
    f.wait_for(&task, "120s");
}

#[test]
fn a_revise_blocks_dispatch_until_an_approve_follows() {
    let f = standard();
    let (task, _) = f.create_task(HIGH);
    let revise = write_json(&f, "revise.json", REVISE_DOC);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let r = record_ok(&f, &task, &revise);
    assert_eq!(r["verdict"], "REVISE");
    assert_eq!(r["approved"], false);
    assert_eq!(r["revise_streak"], 1);
    assert_eq!(r["stalled"], false);
    let err = dispatch_err(&f, &task, &[]);
    assert!(err.contains("is REVISE"), "{err}");
    assert!(err.contains("risk 'high'"), "{err}");
    assert_eq!(run_count(&f, &task), 0);
    let r = record_ok(&f, &task, &approve);
    assert_eq!(r["revise_streak"], 0);
    let s = show_json(&f, &task);
    assert_eq!(s["review_count"], 2);
    assert_eq!(s["approved"], true);
    assert_eq!(s["newest"]["verdict"], "APPROVE");
    let d = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(d["spec_review"]["approved"], true);
    f.wait_for(&task, "120s");
}

#[test]
fn the_approval_is_bound_to_the_spec_bytes() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let id = record_ok(&f, &task, &approve)["spec_review_id"]
        .as_str()
        .unwrap()
        .to_string();
    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    let u = update_spec_json(&f, &task, &rewritten);
    let warnings = u["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{u}");
    assert!(
        warnings[0]
            .as_str()
            .unwrap()
            .starts_with(&format!("spec review {id} (APPROVE) no longer applies: the spec changed")),
        "{u}"
    );
    assert_eq!(last_event(&f, &task, "spec_updated").1["spec_sha256"], sha(REWRITTEN));
    assert_eq!(last_event(&f, &task, "task_created").1["spec_sha256"], sha(IMPORTANT));

    let s = show_json(&f, &task);
    assert_eq!(s["spec_sha256"], sha(REWRITTEN));
    assert_eq!(s["approved"], false);
    assert_eq!(s["newest"]["current"], false);
    assert_eq!(s["newest"]["spec_sha256"], sha(IMPORTANT));

    let err = dispatch_err(&f, &task, &[]);
    assert!(err.contains("is for an older spec"), "{err}");
    assert!(err.contains(&format!("sha256 {}", &sha(IMPORTANT)[..12])), "{err}");

    record_ok(&f, &task, &approve);
    assert_eq!(show_json(&f, &task)["approved"], true);
    f.dispatch(&task, "claude-b", "crash", &[]);
    f.wait_for(&task, "120s");
}

#[test]
fn three_revises_in_a_row_stop_dispatch_even_with_the_skip_flag() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let revise = write_json(&f, "revise.json", REVISE_DOC);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let r1 = record_ok(&f, &task, &revise);
    assert_eq!(r1["revise_streak"], 1);
    assert_eq!(r1["stalled"], false);
    let r2 = record_ok(&f, &task, &revise);
    assert_eq!(r2["revise_streak"], 2);
    assert_eq!(r2["stalled"], false);
    let r3 = record_ok(&f, &task, &revise);
    assert_eq!(r3["revise_streak"], 3);
    assert_eq!(r3["stalled"], true);

    // The same third REVISE without --json, on a second task: the stuck tail
    // comes after the closing parenthesis.
    let (task_h, _) = f.create_task(IMPORTANT);
    record_ok(&f, &task_h, &revise);
    record_ok(&f, &task_h, &revise);
    let line = record_human(&f, &task_h, &revise);
    assert!(
        line.lines().any(|l| l.ends_with(
            "findings); stuck: 3 REVISE in a row, dispatch is stopped until an APPROVE is recorded"
        )),
        "{line}"
    );

    let human = show_human(&f, &task);
    assert!(human.contains("stuck: 3 REVISE in a row"), "{human}");

    let err = dispatch_err(&f, &task, &[]);
    assert!(err.contains("is stuck: its spec was sent back by 3 spec reviews in a row"), "{err}");
    for r in [&r1, &r2, &r3] {
        assert!(err.contains(r["spec_review_id"].as_str().unwrap()), "{err}");
    }
    assert!(err.contains("--skip-spec-review does not apply"), "{err}");
    let err = dispatch_err(&f, &task, &["--skip-spec-review", "x"]);
    assert!(err.contains("is stuck"), "{err}");
    assert_eq!(run_count(&f, &task), 0);
    assert_eq!(
        f.events_of(&task),
        ["task_created", "spec_review", "spec_review", "spec_review"]
    );

    let r = record_ok(&f, &task, &approve);
    assert_eq!(r["revise_streak"], 0);
    assert_eq!(r["stalled"], false);
    f.dispatch(&task, "claude-b", "crash", &[]);
    f.wait_for(&task, "120s");

    // An APPROVE between REVISEs ends the streak.
    let f2 = standard();
    let (task2, _) = f2.create_task(IMPORTANT);
    let revise2 = write_json(&f2, "revise.json", REVISE_DOC);
    let approve2 = write_json(&f2, "approve.json", APPROVE_DOC);
    record_ok(&f2, &task2, &revise2);
    record_ok(&f2, &task2, &revise2);
    record_ok(&f2, &task2, &approve2);
    let r4 = record_ok(&f2, &task2, &revise2);
    assert_eq!(r4["revise_streak"], 1);
    let s = show_json(&f2, &task2);
    assert_eq!(s["stalled"], false);
    assert_eq!(s["approved"], false);

    // The stuck message offers `task close`; the stuck task is still queued.
    let f3 = standard();
    let (task3, _) = f3.create_task(IMPORTANT);
    let revise3 = write_json(&f3, "revise.json", REVISE_DOC);
    record_ok(&f3, &task3, &revise3);
    record_ok(&f3, &task3, &revise3);
    let r = record_ok(&f3, &task3, &revise3);
    assert_eq!(r["stalled"], true);
    assert_eq!(f3.task_status(&task3), "queued");
    let mut c = f3.cmd();
    c.args(["task", "close", &task3, "--reason", "spec keeps bouncing", "--json"]);
    let out = f3.run_ok(c, "task close");
    let closed: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(closed["status"], "cancelled");
    let err = record_err(&f3, &task3, &revise3);
    assert!(err.contains("is 'cancelled'"), "{err}");
}

#[test]
fn an_invalid_document_is_refused_with_one_event_and_nothing_else() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let cases: [(&str, &str, &str); 6] = [
        (
            r#"{"verdict":"MAYBE","criteria":[]}"#,
            "schema",
            "must be APPROVE or REVISE",
        ),
        (
            r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}]}"#,
            "coverage",
            "does not judge acceptance criterion 'AC2'",
        ),
        (
            r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]},{"id":"AC2","verdict":"revise","findings":[{"check":"steps","location":"spec","note":"write the steps"}]}]}"#,
            "schema",
            "says APPROVE but marks criterion 'AC2' revise",
        ),
        (
            r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"revise","findings":[]},{"id":"AC2","verdict":"ok","findings":[]}]}"#,
            "schema",
            "is revise but lists no finding",
        ),
        (
            r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"revise","findings":[{"check":"vibes","location":"spec","note":"n"}]},{"id":"AC2","verdict":"ok","findings":[]}]}"#,
            "schema",
            "is not one of verifiable, exists, consistent, steps",
        ),
        (
            r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"revise","findings":[{"check":"exists","location":"","note":"n"}]},{"id":"AC2","verdict":"ok","findings":[]}]}"#,
            "schema",
            "has a finding with no location",
        ),
    ];
    for (i, (doc, stage, fragment)) in cases.iter().enumerate() {
        let file = write_json(&f, &format!("bad-{i}.json"), doc);
        let err = record_err(&f, &task, &file);
        assert!(err.contains(&format!("invalid at stage {stage}")), "{err}");
        assert!(err.contains(fragment), "{err}");
        assert_eq!(count_events(&f, &task, "spec_review_invalid"), i as i64 + 1);
        assert_eq!(count_events(&f, &task, "spec_review"), 0);
        let (_, detail) = last_event(&f, &task, "spec_review_invalid");
        assert_eq!(detail["stage"], *stage);
        assert_eq!(detail["source"], "external");
        if *stage == "coverage" {
            assert_eq!(detail["expected"], json!(["AC1", "AC2"]));
        }
    }
    assert_eq!(f.task_status(&task), "queued");
}

#[test]
fn preconditions_refuse_and_write_nothing() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);

    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01SOMERUNIDSOMERUNIDSOME1")
        .args(["spec", "review", "record", &task, "-f"])
        .arg(&approve)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("THOTH_RUN_ID"), "{err}");
    assert_eq!(f.events_of(&task), ["task_created"]);

    set_status(&f, &task, "running");
    let err = record_err(&f, &task, &approve);
    assert!(
        err.contains("is 'running'; spec review record needs a task that is not running, pass, cancelled or accepted"),
        "{err}"
    );
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 0);

    set_status(&f, &task, "pass");
    let err = record_err(&f, &task, &approve);
    assert!(err.contains("is 'pass'"), "{err}");

    set_status(&f, &task, "cancelled");
    let err = record_err(&f, &task, &approve);
    assert!(err.contains("is 'cancelled'"), "{err}");

    set_status(&f, &task, "accepted");
    let err = record_err(&f, &task, &approve);
    assert!(err.contains("is 'accepted'"), "{err}");
    assert_eq!(count_events(&f, &task, "spec_review"), 0);

    set_status(&f, &task, "reject");
    let r = record_ok(&f, &task, &approve);
    assert_eq!(r["task_status"], "reject");

    set_status(&f, &task, "in_review");
    record_ok(&f, &task, &approve);
}

#[test]
fn low_and_normal_risk_are_not_gated() {
    let f = standard();
    let (normal, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&normal, "claude-b", "crash", &[]);
    assert_eq!(
        d["spec_review"],
        json!({"required":false,"approved":false,"skipped":false,"spec_review_id":null})
    );
    f.wait_for(&normal, "120s");

    let (low, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&low, "claude-b", "crash", &["--skip-spec-review", "not needed"]);
    assert_eq!(d["spec_review"]["skipped"], true);
    assert_eq!(d["spec_review"]["required"], false);
    assert_eq!(last_event(&f, &low, "spec_review_skipped").1["required"], false);
    f.wait_for(&low, "120s");
}

#[test]
fn only_the_first_dispatch_is_gated() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let id = record_ok(&f, &task, &approve)["spec_review_id"]
        .as_str()
        .unwrap()
        .to_string();
    let d = f.dispatch(&task, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_run(&run_id));

    let reject = write_json(
        &f,
        "reject.json",
        r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet","note":"never fires"},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#,
    );
    let mut c = f.cmd();
    c.args(["review", "record", &task, "-f"]).arg(&reject).arg("--json");
    f.run_ok(c, "review record");
    assert_eq!(f.task_status(&task), "reject");

    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    update_spec_json(&f, &task, &rewritten);
    let s = show_json(&f, &task);
    assert_eq!(s["approved"], false);
    assert_eq!(s["required_before_dispatch"], false);

    let d = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"], 2);
    assert_eq!(
        d["spec_review"],
        json!({"required":false,"approved":false,"skipped":false,"spec_review_id":id})
    );
    assert_eq!(d["warnings"], json!([]));
    f.wait_for(&task, "180s");
}

#[test]
fn show_with_no_review_has_the_same_keys() {
    let f = standard();
    let (task, _) = f.create_task(HIGH);
    let s = show_json(&f, &task);
    assert_eq!(
        sorted_keys(&s),
        [
            "approved",
            // BRIEF-1: the newest spec review's short conclusion, null here
            "brief",
            "newest",
            "pending_edits",
            "required_before_dispatch",
            "review_count",
            "revise_streak",
            "risk",
            "spec_sha256",
            "stalled",
            "task_id",
            "task_status"
        ]
    );
    assert_eq!(s["pending_edits"], false);
    assert!(s["newest"].is_null());
    assert!(s["brief"].is_null());
    assert_eq!(s["review_count"], 0);
    assert_eq!(s["approved"], false);
    assert_eq!(s["required_before_dispatch"], true);
    assert_eq!(s["spec_sha256"], sha(HIGH));
    let human = show_human(&f, &task);
    assert!(human.contains("no spec review recorded for"), "{human}");
    assert!(human.contains("thoth spec review record"), "{human}");
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start_scenario_task(
        f: &common::Fixture,
        extra: &[&str],
        scenario: &str,
        task: Option<&str>,
    ) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .args(extra)
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("FAKE_SCENARIO", scenario)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(task) = task {
            c.env("FAKE_TASK_ID", task);
        } else {
            c.env_remove("FAKE_TASK_ID");
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_records_shows_and_skips() {
    let f = standard();
    let mut s = Server::start_scenario_task(&f, &[], "crash", None);
    assert_eq!(s.call(1, "thoth_init", json!({}))["result"]["isError"], false);
    let spec: Value = serde_json::from_str(IMPORTANT).unwrap();
    let created = s.call(2, "thoth_task_create", json!({"title":"gate","spec":spec}));
    let task = created["result"]["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(s);
    let mut s = Server::start_scenario_task(&f, &[], "crash", Some(&task));

    let list = s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    // thoth_review_confirm sits after thoth_review_start, one place before
    // these.
    assert_eq!(tools.len(), 36);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names[18], "thoth_spec_review_record");
    assert_eq!(names[19], "thoth_spec_review_show");
    let tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap().clone();
    assert_eq!(tool("thoth_spec_review_show")["annotations"]["readOnlyHint"], true);
    assert_eq!(tool("thoth_spec_review_record")["annotations"]["readOnlyHint"], false);
    assert!(tool("thoth_dispatch")["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .contains_key("skip_spec_review"));

    let shown = s.call(4, "thoth_spec_review_show", json!({"task":task}));
    assert_eq!(shown["result"]["isError"], false, "{shown}");
    assert_eq!(shown["result"]["structuredContent"]["approved"], false);

    let refused = s.call(
        5,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-b","model":"sonnet","effort":"medium"}),
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("has no APPROVE spec review"));

    let revise: Value = serde_json::from_str(REVISE_DOC).unwrap();
    let r = s.call(6, "thoth_spec_review_record", json!({"task":task,"review":revise}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["verdict"], "REVISE");

    let bad = s.call(
        7,
        "thoth_spec_review_record",
        json!({"task":task,"review":{"verdict":"MAYBE","criteria":[]}}),
    );
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(bad["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("invalid at stage schema"));

    let approve: Value = serde_json::from_str(APPROVE_DOC).unwrap();
    let a = s.call(8, "thoth_spec_review_record", json!({"task":task,"review":approve}));
    assert_eq!(a["result"]["isError"], false, "{a}");
    assert_eq!(a["result"]["structuredContent"]["approved"], true);

    let d = s.call(
        9,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-b","model":"sonnet","effort":"medium","skip_spec_review":"via mcp"}),
    );
    assert_eq!(d["result"]["isError"], false, "{d}");
    assert_eq!(d["result"]["structuredContent"]["spec_review"]["skipped"], true);
    assert_eq!(d["result"]["structuredContent"]["spec_review"]["approved"], true);

    let tmp = f.home.path().join("mcp/tmp");
    assert!(tmp
        .read_dir()
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true));
    f.wait_for(&task, "120s");
}

#[test]
fn update_spec_is_allowed_from_queued() {
    let f = standard();
    let (task, _) = f.create_task(common::Fixture::SPEC);
    let two = write_json(&f, "two.json", common::Fixture::SPEC_TWO_AC);
    let u = update_spec_json(&f, &task, &two);
    assert_eq!(u["status"], "queued");
    assert_eq!(u["acceptance_criteria"], 2);
    set_status(&f, &task, "running");
    let out = f
        .cmd()
        .args(["task", "update-spec", &task, "-f"])
        .arg(&two)
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("update-spec needs draft, queued, reject or unverified"), "{err}");
}

fn create_json(f: &Fixture, file: &Path) -> Value {
    let mut c = f.cmd();
    c.args(["task", "create", "-f"]).arg(file).arg("--json");
    let out = f.run_ok(c, "task create");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

#[test]
fn task_create_warns_for_an_important_task_and_not_for_a_normal_one() {
    let f = standard();
    let important = write_json(&f, "important.json", IMPORTANT);
    let v = create_json(&f, &important);
    let task = v["task_id"].as_str().unwrap();
    let warnings = v["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{v}");
    let w = warnings[0].as_str().unwrap();
    assert!(
        w.starts_with("risk important: a spec review APPROVE of this spec is required before the first dispatch"),
        "{w}"
    );
    assert!(w.contains(&format!("thoth spec review record {task}")), "{w}");

    let normal = write_json(&f, "normal.json", common::Fixture::SPEC_TWO_AC);
    let v = create_json(&f, &normal);
    assert_eq!(v["warnings"], json!([]));
}

fn help(args: &[&str]) -> String {
    let f = common::fixture();
    let out = f
        .cmd()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

#[test]
fn help_lists_the_new_surface() {
    let top = help(&["--help"]);
    assert!(top.lines().any(|l| l.trim().starts_with("spec")), "{top}");
    let spec_review = help(&["spec", "review", "--help"]);
    assert!(spec_review.contains("record"), "{spec_review}");
    assert!(spec_review.contains("show"), "{spec_review}");
    let dispatch = help(&["dispatch", "--help"]);
    assert!(dispatch.contains("--skip-spec-review"), "{dispatch}");
    let update_spec = help(&["task", "update-spec", "--help"]);
    assert!(update_spec.contains("queued"), "{update_spec}");
}

#[test]
fn a_spec_changed_after_validation_is_not_approved_by_the_old_review() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    let pause = f.home_path().join("record-paused");
    let mut child = paused_cmd(&f, "THOTH_TEST_SPEC_REVIEW_PAUSE_FILE", &pause)
        .args(["spec", "review", "record", &task, "-f"])
        .arg(&approve)
        .spawn()
        .unwrap();
    wait_for_pause(&mut child, &pause);
    update_spec_json(&f, &task, &rewritten);
    std::fs::remove_file(&pause).unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("spec changed while the spec review was being recorded"), "{err}");
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 0);
    assert_eq!(show_json(&f, &task)["approved"], false);
}

#[test]
fn a_spec_changed_during_dispatch_is_refused() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let approve = write_json(&f, "approve.json", APPROVE_DOC);
    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    record_ok(&f, &task, &approve);
    let pause = f.home_path().join("dispatch-paused");
    let mut child = paused_cmd(&f, "THOTH_TEST_DISPATCH_PAUSE_FILE", &pause)
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .spawn()
        .unwrap();
    wait_for_pause(&mut child, &pause);
    update_spec_json(&f, &task, &rewritten);
    std::fs::remove_file(&pause).unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("spec changed while it was being dispatched"), "{err}");
    assert_eq!(run_count(&f, &task), 0);
    assert_eq!(f.task_status(&task), "queued");
    assert_eq!(count_events(&f, &task, "spec_review_skipped"), 0);
    let wt = worktree_a1(&f, &task);
    assert!(!wt.exists(), "{}", wt.display());
}
