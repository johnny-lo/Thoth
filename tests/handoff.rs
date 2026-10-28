//! S3-9: the handoff file a dispatch writes when it continues a run that
//! stopped early (quota, timeout, crash). Always the fakes (`fake-claude`,
//! `fake-codex`); nothing here reaches a network or a real harness.

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const CODEX_MODEL: &str = "gpt-5.6-luna";

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

/// Dispatches `task_id` to `profile` with the `hooks` scenario stopped by a
/// quota error after two captured commands (the same set-up as
/// tests/evidence.rs's resume test), waits for `interrupted`, and returns the
/// run id and the worktree.
fn interrupt_by_quota(f: &Fixture, task_id: &str, profile: &str) -> (String, PathBuf) {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_HOOKS_THEN_QUOTA", "1")
        .args(["dispatch", task_id, "--profile", profile, "--model", "sonnet", "--effort", "medium", "--json"]);
    let out = f.run_ok(c, "dispatch (hooks, then quota)");
    let d: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(f.wait_for(task_id, "120s").1, "interrupted");
    (
        d["run_id"].as_str().unwrap().to_string(),
        PathBuf::from(d["worktree"].as_str().unwrap()),
    )
}

fn count_runs(f: &Fixture, task_id: &str) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE task_id = ?1", [task_id], |r| r.get(0))
        .unwrap()
}

fn handoff_events(f: &Fixture, task_id: &str) -> Vec<(Option<String>, Value)> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT run_id, detail FROM events WHERE task_id = ?1 AND type = 'handoff_written' ORDER BY seq")
        .unwrap();
    stmt.query_map([task_id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(|row| {
            let (run, detail) = row.unwrap();
            (run, serde_json::from_str(&detail).unwrap())
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn handoff_in(wt: &Path) -> PathBuf {
    wt.join(".agent").join("handoff.md")
}

/// (a)'s set-up: claude-b stops on quota with E001 and E002 captured, then
/// the task goes to codex-a and reaches `in_review`.
struct HandedToCodex {
    f: Fixture,
    task_id: String,
    previous: String,
    wt: PathBuf,
    d: Value,
}

fn handed_to_codex() -> HandedToCodex {
    let f = init();
    f.add_profile("claude-b");
    f.add_codex_profile("codex-a");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let (previous, wt) = interrupt_by_quota(&f, &task_id, "claude-b");
    let rows = f.evidence_rows(&task_id);
    let codes: Vec<&str> = rows
        .iter()
        .filter(|r| r["kind"] == "command")
        .map(|r| r["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["E001", "E002"], "{rows:#?}");

    let d = f.dispatch(&task_id, "codex-a", "ok", &["--model", CODEX_MODEL]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    HandedToCodex { f, task_id, previous, wt, d }
}

// (a) -------------------------------------------------------------------------

#[test]
fn a_a_quota_stop_handed_to_codex_writes_the_handoff_and_records_it() {
    let HandedToCodex { f, task_id, previous, wt, d } = handed_to_codex();
    let new_run = d["run_id"].as_str().unwrap().to_string();
    let run_dir = PathBuf::from(d["run_dir"].as_str().unwrap());

    // both copies exist and are the same
    let in_wt = fs::read(handoff_in(&wt)).expect("worktree .agent/handoff.md");
    let in_run_dir = fs::read(run_dir.join("handoff.md")).expect("run directory handoff.md");
    assert_eq!(in_wt, in_run_dir);
    let text = String::from_utf8(in_wt.clone()).unwrap();
    assert!(text.contains(&previous), "{text}");
    assert!(text.contains("quota"), "{text}");
    assert!(text.contains("E001"), "{text}");
    assert!(text.contains("E002"), "{text}");

    // the prompt names the file and carries none of it
    let prompt = fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains(".agent/handoff.md"), "{prompt}");
    assert!(prompt.contains("## Continuing an interrupted run"), "{prompt}");
    assert!(!prompt.contains("## Evidence so far"), "{prompt}");

    // one handoff_written event for the new run
    let events = handoff_events(&f, &task_id);
    assert_eq!(events.len(), 1, "{events:?}");
    let (event_run, detail) = &events[0];
    assert_eq!(event_run.as_deref(), Some(new_run.as_str()));
    assert_eq!(detail["cross_provider"], true, "{detail}");
    assert_eq!(detail["sha256"], sha256_hex(&in_wt), "{detail}");
    assert_eq!(detail["bytes"].as_u64(), Some(in_wt.len() as u64), "{detail}");
    assert_eq!(detail["previous_run"], previous.as_str(), "{detail}");
    assert_eq!(detail["path"], ".agent/handoff.md", "{detail}");

    // dispatch --json
    assert_eq!(d["attempt"].as_i64(), Some(1), "{d}");
    assert_eq!(d["worktree_reused"], true, "{d}");
    assert_eq!(d["handoff"]["previous_run"], previous.as_str(), "{d}");
    assert_eq!(d["handoff"]["reason"], "quota", "{d}");

    assert_eq!(f.task_status(&task_id), "in_review");

    // review packet: --json and Markdown
    let mut c = f.cmd();
    c.args(["review", "packet", &task_id, "--json"]);
    let packet: Value =
        serde_json::from_slice(&f.run_ok(c, "review packet --json")).unwrap();
    let run = packet["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == new_run.as_str())
        .unwrap_or_else(|| panic!("new run missing from {packet}"));
    assert_eq!(run["continued_from"], previous.as_str(), "{packet}");
    let mut c = f.cmd();
    c.args(["review", "packet", &task_id]);
    let markdown = String::from_utf8(f.run_ok(c, "review packet")).unwrap();
    let line = markdown
        .lines()
        .find(|l| l.starts_with(&format!("- {new_run} ")))
        .unwrap_or_else(|| panic!("no Runs line for {new_run}: {markdown}"));
    assert!(line.contains("continued from"), "{line}");
}

// (b) -------------------------------------------------------------------------

#[test]
fn b_another_claude_profile_continuing_is_not_cross_provider() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let (_previous, wt) = interrupt_by_quota(&f, &task_id, "claude-b");

    let d = f.dispatch(&task_id, "claude-a", "ok", &[]);
    assert!(handoff_in(&wt).exists());
    assert_eq!(d["handoff"]["cross_provider"], false, "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let events = handoff_events(&f, &task_id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].1["cross_provider"], false, "{:?}", events[0]);
}

// (c) -------------------------------------------------------------------------

#[test]
fn c_a_first_dispatch_from_queued_writes_no_handoff() {
    let f = init();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let wt = PathBuf::from(d["worktree"].as_str().unwrap());
    let run_dir = PathBuf::from(d["run_dir"].as_str().unwrap());

    assert!(!handoff_in(&wt).exists());
    assert!(handoff_events(&f, &task_id).is_empty());
    let prompt = fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(!prompt.contains("## Continuing an interrupted run"), "{prompt}");
    assert!(d["handoff"].is_null(), "{d}");
}

// (d) -------------------------------------------------------------------------

#[test]
fn d_resume_writes_no_handoff() {
    let f = init();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let (run_id, wt) = interrupt_by_quota(&f, &task_id, "claude-b");

    f.cmd().args(["profile", "check", "claude-b"]).assert().success();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &run_id, "--json"]);
    f.run_ok(c, "resume");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    assert!(!handoff_in(&wt).exists());
    assert!(handoff_events(&f, &task_id).is_empty());
}

// (f) -------------------------------------------------------------------------

#[test]
fn f_a_timed_out_run_redispatched_gets_a_handoff_with_reason_timeout() {
    let f = init();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let first = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "3s"]);
    assert_eq!(f.wait_for(&task_id, "120s").1, "failed");
    let wt = PathBuf::from(first["worktree"].as_str().unwrap());

    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert!(handoff_in(&wt).exists());
    assert_eq!(d["handoff"]["reason"], "timeout", "{d}");
    assert_eq!(d["handoff"]["previous_run"], first["run_id"], "{d}");
    f.wait_for(&task_id, "120s");
}

// (g) -------------------------------------------------------------------------

#[test]
fn g_a_handoff_that_cannot_be_written_fails_the_dispatch_and_writes_nothing() {
    let f = init();
    f.add_profile("claude-b");
    f.add_codex_profile("codex-a");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let (_previous, wt) = interrupt_by_quota(&f, &task_id, "claude-b");
    fs::create_dir_all(handoff_in(&wt)).unwrap();
    let runs_before = count_runs(&f, &task_id);

    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(count_runs(&f, &task_id), runs_before);
    assert!(handoff_events(&f, &task_id).is_empty());
    assert_eq!(f.task_status(&task_id), "interrupted");
}

// (h) -------------------------------------------------------------------------

#[test]
fn h_a_stale_handoff_is_removed_by_a_dispatch_that_is_not_a_handoff() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let (_previous, wt) = interrupt_by_quota(&f, &task_id, "claude-b");

    // a handoff whose run is refused by rule 1
    f.dispatch(&task_id, "claude-a", "bad_result", &[]);
    assert_eq!(f.wait_for(&task_id, "120s").1, "unverified");
    assert!(handoff_in(&wt).exists());

    // out of unverified: a reroute, not a handoff
    let d = f.dispatch(&task_id, "claude-a", "ok", &[]);
    assert!(!handoff_in(&wt).exists(), "the stale handoff must be gone on return");
    assert!(d["handoff"].is_null(), "{d}");
    f.wait_for(&task_id, "120s");
}

// (i) -------------------------------------------------------------------------

#[test]
fn i_the_handoff_file_is_not_in_the_candidate_commit_or_the_diff_evidence() {
    let HandedToCodex { f, task_id, d, .. } = handed_to_codex();
    let new_run = d["run_id"].as_str().unwrap().to_string();
    let candidate: String = f
        .state_conn()
        .query_row("SELECT candidate_commit FROM runs WHERE run_id = ?1", [&new_run], |r| r.get(0))
        .unwrap();
    let names = common::git(
        &f.repo_path(),
        &["show", "--name-only", "--format=", &candidate],
    );
    assert!(!names.trim().is_empty(), "the codex run committed something");
    for line in names.lines() {
        assert!(!line.starts_with(".agent/"), "{line} is in the candidate commit");
    }
    let rows = f.evidence_rows(&task_id);
    let diff = rows
        .iter()
        .find(|r| r["kind"] == "diff" && r["run_id"] == new_run.as_str())
        .unwrap_or_else(|| panic!("no diff evidence for {new_run}: {rows:#?}"));
    let files = diff["detail"]["files_changed"].as_array().expect("files_changed");
    for path in files {
        assert!(!path.as_str().unwrap().starts_with(".agent/"), "{diff}");
    }
}

// (j) -------------------------------------------------------------------------

#[test]
fn j_a_run_that_was_continued_cannot_be_resumed() {
    let HandedToCodex { f, task_id, previous, d, .. } = handed_to_codex();
    let new_run = d["run_id"].as_str().unwrap().to_string();
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &previous])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains(&new_run), "{stderr}");
    assert!(stderr.contains("continued"), "{stderr}");
}

// S3-10 AC10, moved from tests/handover.rs (RJ-1 removed the handover; the
// handoff of a named re-dispatch it checked stays) ---------------------------

/// One implementer run row of the task.
#[derive(Debug, Clone)]
struct RunRow {
    run_id: String,
    attempt: i64,
    status: String,
    interrupted_reason: Option<String>,
    spec: Value,
    run_dir: String,
}

/// The task's implementer runs, oldest first.
fn runs(f: &Fixture, task_id: &str) -> Vec<RunRow> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare(
            "SELECT run_id, attempt, status, interrupted_reason, spec, run_dir
             FROM runs WHERE task_id = ?1 AND role = 'implementer' ORDER BY run_id",
        )
        .unwrap();
    stmt.query_map([task_id], |r| {
        Ok(RunRow {
            run_id: r.get(0)?,
            attempt: r.get(1)?,
            status: r.get(2)?,
            interrupted_reason: r.get(3)?,
            spec: serde_json::from_str(&r.get::<_, String>(4)?).unwrap(),
            run_dir: r.get(5)?,
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn wait_task(f: &Fixture, task_id: &str) -> String {
    f.wait_for(task_id, "120s").1
}

/// Copy of tests/rework.rs's `rejected_task`, with this file's profile names.
fn rejected_task(f: &Fixture, spec: &str) -> (String, String) {
    f.add_profile("a-claude");
    f.add_profile("b-claude");
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "b-claude", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review", "{}", f.explain_run(&run_id));

    let r = f.review_start(&task_id, "a-claude", "review_reject", &["--model", "haiku"]);
    let mut wait = f.cmd();
    wait.args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"]);
    f.run_ok(wait, "wait --run");
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "reject",
        "{}",
        f.explain_task(&task_id)
    );
    (task_id, run_id)
}

#[test]
fn ac10_a_handoff_dispatch_keeps_the_previous_runs_rework() {
    let f = init();
    let (task_id, _) = rejected_task(&f, Fixture::SPEC);

    let rework_run = f.dispatch(&task_id, "b-claude", "quota", &[]);
    let rework_run = rework_run["run_id"].as_str().unwrap().to_string();
    assert_eq!(wait_task(&f, &task_id), "interrupted");
    let first = runs(&f, &task_id).into_iter().find(|r| r.run_id == rework_run).unwrap();
    assert_eq!(first.attempt, 2);
    assert_eq!(first.status, "interrupted");
    assert_eq!(first.interrupted_reason.as_deref(), Some("quota"));
    assert!(first.spec.get("rework").is_some(), "{}", first.spec);

    let d = f.dispatch(&task_id, "a-claude", "ok", &[]);
    assert_ne!(d["handoff"], Value::Null, "{d}");
    assert_eq!(d["rework"], false, "{d}");
    assert_eq!(d["attempt"], 2, "{d}");
    let new_id = d["run_id"].as_str().unwrap().to_string();
    wait_task(&f, &task_id);

    let new = runs(&f, &task_id).into_iter().find(|r| r.run_id == new_id).unwrap();
    assert_eq!(new.spec["rework"], first.spec["rework"], "{}", new.spec);
    let prompt = fs::read_to_string(Path::new(&new.run_dir).join("prompt.txt")).unwrap();
    assert!(prompt.contains("## Previous attempt was rejected"), "{prompt}");
    assert!(prompt.contains("This is attempt 2"), "{prompt}");
    assert!(prompt.contains("no captured evidence shows this criterion being exercised"), "{prompt}");
}
