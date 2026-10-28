//! What is left of S3-8 after RJ-1 removed the automatic pick: `auto` stays a
//! reserved profile id, a recovered profile that a dispatch names keeps its
//! status when the dispatch fails before the run row, and the optional
//! `task_type` / `difficulty` labels. Always the fakes (`fake-claude`,
//! `fake-codex`); nothing here reaches a network or a real harness.
//!
//! The quota readings are written straight into the databases the way
//! tests/usage_recommend.rs does it; the small helpers below are copies of its
//! (`at`, `epoch`), because that file is not a module.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const CODEX_MODEL: &str = "gpt-5.6-luna";

// --- copied from tests/usage_recommend.rs ----------------------------------

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta)
        .format(&Rfc3339)
        .unwrap()
}

fn epoch(delta: Duration) -> i64 {
    (OffsetDateTime::now_utc() + delta).unix_timestamp()
}

// --- fixtures ----------------------------------------------------------------

/// The `models_cache.json` shape tests/usage_recommend.rs writes, with one
/// model whose slug is exactly `CODEX_MODEL`.
fn write_codex_cache(cfg: &Path) {
    fs::write(
        cfg.join("models_cache.json"),
        serde_json::json!({"models":[{"slug":CODEX_MODEL,"display_name":CODEX_MODEL,"visibility":"list","priority":1,"supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]}]})
            .to_string(),
    )
    .unwrap();
}

/// Parks `profile` the way tests/usage_recommend.rs's `parked_fixture` does
/// for its recovered case: `rate_limited` two hours ago, then a 99% reading
/// whose window reset half an hour ago.
fn park_and_reset(f: &Fixture, profile: &str) {
    let g = f.global_conn();
    g.execute(
        "UPDATE profiles SET status='rate_limited' WHERE profile_id=?1",
        [profile],
    )
    .unwrap();
    g.execute(
        "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1,?2,'rate_limited','{}')",
        params![at(Duration::hours(-2)), profile],
    )
    .unwrap();
}

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn claude_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-claude-args.jsonl")
}

fn codex_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-codex-args.jsonl")
}

fn envd(f: &Fixture, scenario: &str, task_id: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_CLAUDE_ARGS_LOG", claude_log(f))
        .env("FAKE_CODEX_ARGS_LOG", codex_log(f));
    c
}

fn count(f: &Fixture, table: &str) -> i64 {
    f.state_conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn profile_status(f: &Fixture, profile: &str) -> String {
    f.global_conn()
        .query_row(
            "SELECT status FROM profiles WHERE profile_id = ?1",
            [profile],
            |r| r.get(0),
        )
        .unwrap()
}

fn window_resets(f: &Fixture) -> Vec<Value> {
    let g = f.global_conn();
    let mut stmt = g
        .prepare("SELECT detail FROM quota_events WHERE kind = 'window_reset' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

fn create_task_with(f: &Fixture, spec: &Value) -> String {
    f.create_task(&spec.to_string()).0
}

// (d) -------------------------------------------------------------------------

#[test]
fn a_recovered_profile_keeps_its_status_when_the_dispatch_fails_before_the_run_row() {
    let f = init();
    let cfg = f.add_codex_profile("codex-a");
    write_codex_cache(&cfg);
    park_and_reset(&f, "codex-a");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        serde_json::json!({"timestamp":at(Duration::hours(-1)),"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":99.0,"window_minutes":300,"resets_at":epoch(Duration::minutes(-30))}}}}).to_string() + "\n",
    )
    .unwrap();
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let runs = count(&f, "runs");
    let events = count(&f, "events");

    // the codex login check fails after the model and effort checks, before
    // any write
    let out = envd(&f, "ok", &task_id)
        .env("FAKE_CODEX_LOGGED_OUT", "1")
        .args([
            "dispatch", &task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("auth check"), "{stderr}");
    assert_eq!(profile_status(&f, "codex-a"), "rate_limited");
    assert!(window_resets(&f).is_empty());
    assert_eq!(count(&f, "runs"), runs);
    assert_eq!(count(&f, "events"), events);
}

// (f) -------------------------------------------------------------------------

#[test]
fn f_profile_add_auto_is_refused() {
    let f = init();
    let cfg = f.home_path().join("profiles").join("auto");
    fs::create_dir_all(&cfg).unwrap();
    let out = f
        .cmd()
        .args(["profile", "add", "auto", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reserved"), "{stderr}");
    let profiles: i64 = f
        .global_conn()
        .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(profiles, 0);
}

#[test]
fn profile_add_refuses_auto_in_any_letter_case() {
    let f = init();
    for id in ["AUTO", "Auto"] {
        let cfg = f.home_path().join("profiles").join(id);
        fs::create_dir_all(&cfg).unwrap();
        let out = f
            .cmd()
            .args(["profile", "add", id, "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
            .arg(&cfg)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{id}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("reserved"), "{id}: {stderr}");
    }
    let profiles: i64 = f
        .global_conn()
        .query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get(0))
        .unwrap();
    assert_eq!(profiles, 0);
}

// (g) -------------------------------------------------------------------------

#[test]
fn g_a_task_type_of_coding_is_refused_by_task_create_and_null_is_accepted() {
    let f = init();
    let file = f.home_path().join("bad.json");
    fs::write(
        &file,
        r#"{"goal":"g","acceptance_criteria":["AC1: x"],"task_type":"coding"}"#,
    )
    .unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    for v in ["code", "research", "review", "docs", "mechanical"] {
        assert!(stderr.contains(v), "{stderr} lacks {v}");
    }
    assert_eq!(count(&f, "tasks"), 0);

    let task_id = create_task_with(
        &f,
        &serde_json::json!({"goal":"g","acceptance_criteria":["AC1: x"],"task_type":null}),
    );
    assert_eq!(f.task_status(&task_id), "queued");
}

#[test]
fn update_spec_refuses_a_bad_difficulty_and_events_carry_the_labels() {
    let f = init();
    f.add_profile("claude-a");
    let task_id = create_task_with(
        &f,
        &serde_json::json!({"goal":"g","acceptance_criteria":["AC1: x"],"task_type":"docs","difficulty":"low"}),
    );
    let created: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE task_id = ?1 AND type = 'task_created'",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    let created: Value = serde_json::from_str(&created).unwrap();
    assert_eq!(created["task_type"], "docs", "{created}");
    assert_eq!(created["difficulty"], "low", "{created}");

    // update-spec is allowed from `unverified`; put the task there directly
    f.state_conn()
        .execute("UPDATE tasks SET status = 'unverified' WHERE task_id = ?1", [&task_id])
        .unwrap();
    let bad = f.home_path().join("bad.json");
    fs::write(&bad, r#"{"goal":"g","acceptance_criteria":["AC1: x"],"difficulty":"hard"}"#).unwrap();
    let out = f
        .cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&bad)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("low, normal, high"));
    assert!(!f.events_of(&task_id).iter().any(|t| t == "spec_updated"));

    let good = f.home_path().join("good.json");
    fs::write(&good, r#"{"goal":"g","acceptance_criteria":["AC1: x"]}"#).unwrap();
    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&good)
        .assert()
        .success();
    let updated: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE task_id = ?1 AND type = 'spec_updated'",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    let updated: Value = serde_json::from_str(&updated).unwrap();
    assert_eq!(updated["task_type"], Value::Null, "{updated}");
    assert_eq!(updated["difficulty"], Value::Null, "{updated}");
}
