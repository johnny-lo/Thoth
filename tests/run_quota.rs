//! S3-12: Claude's `rate_limit_warning` records every window, and an
//! implementer run records a `run_quota` event before its final
//! `status_change`. Driven end to end through `fake-claude`, `fake-codex` and
//! `fake-agy`; never a real harness, never the network.

mod common;

use serde_json::Value;
use std::path::{Path, PathBuf};

const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";

fn json(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim()).unwrap()
}

/// Every event of `ty` for `run_id`, oldest first, with `detail` parsed.
fn run_events(f: &common::Fixture, run_id: &str, ty: &str) -> Vec<Value> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")
        .unwrap();
    stmt.query_map([run_id, ty], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

/// The one `run_quota` event of `run_id`; panics with the run's story when
/// there is not exactly one.
fn the_run_quota(f: &common::Fixture, run_id: &str) -> Value {
    let all = run_events(f, run_id, "run_quota");
    assert_eq!(all.len(), 1, "exactly one run_quota event: {all:?}\n{}", f.explain_run(run_id));
    all[0].clone()
}

fn assert_float(v: &Value, want: f64, what: &str) {
    let got = v
        .as_f64()
        .unwrap_or_else(|| panic!("{what} must be a JSON number, got {v}"));
    assert!((got - want).abs() < 1e-9, "{what}: got {got}, want {want}");
}

/// A codex profile whose `models_cache.json` lists `CODEX_MODEL` with the
/// exact slug `--model` is given, so `thoth models` reports it.
fn codex_setup() -> (common::Fixture, PathBuf, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    std::fs::write(
        cfg.join("models_cache.json"),
        serde_json::json!({"models": [{
            "slug": CODEX_MODEL,
            "display_name": "Luna",
            "visibility": "list",
            "priority": 1
        }]})
        .to_string(),
    )
    .unwrap();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

fn dispatch_codex(f: &common::Fixture, task_id: &str, rollout: Option<&str>) -> String {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task_id)
        .args(["dispatch", task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    if let Some(v) = rollout {
        c.env("FAKE_CODEX_ROLLOUT", v);
    }
    let v = json(&f.run_ok(c, "dispatch"));
    v["run_id"].as_str().unwrap().to_string()
}

/// One Claude implementer run of `scenario` on `claude-b`, waited for.
fn claude_run(scenario: &str, wanted: &str) -> (common::Fixture, String, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", scenario, &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, wanted, "{}", f.explain_run(&run_id));
    (f, task_id, run_id)
}

// AC1 ---------------------------------------------------------------------

#[test]
fn profile_usage_shows_five_hour_and_seven_day_after_a_claude_ok_run() {
    let (f, _task_id, run_id) = claude_run("ok", "in_review");

    // The runner's event keeps its three keys and gains `windows`.
    let warnings = run_events(&f, &run_id, "rate_limit_warning");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let w = &warnings[0];
    assert_eq!(w["rate_limit_type"], "five_hour", "{w}");
    assert_eq!(w["utilization"], 0.07, "{w}");
    assert_eq!(w["resets_at"], 1789381800i64, "{w}");
    let windows = w["windows"].as_array().expect("windows array");
    assert_eq!(windows.len(), 2, "{w}");
    let five = windows.iter().find(|x| x["name"] == "five_hour").expect("five_hour");
    assert_eq!(five["utilization"], 0.07, "{w}");
    assert_eq!(five["resets_at"], 1789381800i64, "{w}");
    let seven = windows.iter().find(|x| x["name"] == "seven_day").expect("seven_day");
    assert_eq!(seven["utilization"], 0.02, "{w}");
    assert_eq!(seven["resets_at"], 1789963200i64, "{w}");

    let out = f
        .cmd()
        .args(["profile", "usage", "claude-b", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    let names: Vec<&str> = v["profiles"][0]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .filter_map(|w| w["name"].as_str())
        .collect();
    assert!(names.contains(&"five_hour"), "{v}");
    assert!(names.contains(&"seven_day"), "{v}");
}

// AC4 (a) -----------------------------------------------------------------

#[test]
fn a_codex_run_with_a_rollout_records_three_points_and_models_counts_them() {
    let (f, _cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, Some("1"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));

    let q = the_run_quota(&f, &run_id);
    assert_float(&q["points"], 3.0, "points");
    assert_float(&q["start_percent"], 10.0, "start_percent");
    assert_float(&q["end_percent"], 13.0, "end_percent");
    assert_eq!(q["source"], "codex_rollout", "{q}");
    assert_eq!(q["window"], "five_hour", "{q}");

    let out = f
        .cmd()
        .args(["models", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    let profile = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "codex-a")
        .unwrap_or_else(|| panic!("no codex-a in {v}"));
    let model = profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == CODEX_MODEL)
        .unwrap_or_else(|| panic!("no {CODEX_MODEL} in {profile}"));
    let breakdown = model["thoth_breakdown"].as_array().expect("thoth_breakdown");
    assert_eq!(breakdown.len(), 1, "{model}");
    assert_float(&breakdown[0]["quota_points"], 3.0, "quota_points");
    assert_eq!(breakdown[0]["quota_runs"], 1, "{model}");
}

// AC4 (b): the deliberately wrong example -------------------------------

#[test]
fn a_codex_rollout_that_crosses_a_reset_records_null_points() {
    let (f, cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, Some("reset"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    // the file is there, so the nulls below come from the reset, not from a miss
    assert_eq!(rollout_names(&f, &cfg, &run_id).len(), 1);

    let q = the_run_quota(&f, &run_id);
    assert!(q["start_percent"].is_null(), "{q}");
    assert!(q["end_percent"].is_null(), "{q}");
    assert!(q["points"].is_null(), "{q}");
    assert_eq!(q["source"], "codex_rollout", "{q}");
    assert_eq!(q["window"], "five_hour", "{q}");
}

// AC4 (c) -----------------------------------------------------------------

#[test]
fn a_claude_ok_run_with_one_reading_records_zero_points() {
    let (f, _task_id, run_id) = claude_run("ok", "in_review");
    let q = the_run_quota(&f, &run_id);
    assert_float(&q["points"], 0.0, "points");
    assert_eq!(q["source"], "claude_stream", "{q}");
    // the one reading is both ends, rounded: 0.07 * 100 is 7.0, not 7.000000000000001
    assert_float(&q["start_percent"], 7.0, "start_percent");
    assert_float(&q["end_percent"], 7.0, "end_percent");
    assert_eq!(q["window"], "five_hour", "{q}");
}

// AC4 (d) -----------------------------------------------------------------

#[test]
fn a_reviewer_run_records_run_quota_too() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let implementer_run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    // the implementer wrote exactly one
    assert_eq!(run_events(&f, &implementer_run, "run_quota").len(), 1);

    let r = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    let status: String = f
        .state_conn()
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&reviewer_run], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "completed", "{}", f.explain_run(&reviewer_run));
    // DT-1: a reviewer run writes one too; fake-claude's review_pass emits no
    // rate_limit_event, so it has no readings.
    let q = the_run_quota(&f, &reviewer_run);
    assert_eq!(q["source"], "claude_stream", "{q}");
    assert_eq!(q["window"], "five_hour", "{q}");
    assert!(q["start_percent"].is_null(), "{q}");
    assert!(q["end_percent"].is_null(), "{q}");
    assert!(q["points"].is_null(), "{q}");
    assert!(q["seven_day"]["start_percent"].is_null(), "{q}");
    assert!(q["seven_day"]["end_percent"].is_null(), "{q}");
    assert!(q["seven_day"]["points"].is_null(), "{q}");
    assert_eq!(q["reliable"], true, "{q}");
    assert_eq!(q["unreliable_reasons"], serde_json::json!([]), "{q}");
}

// AC4 (e) -----------------------------------------------------------------

#[test]
fn run_quota_lands_before_the_runs_last_status_change() {
    let (f, task_id, run_id) = claude_run("ok", "in_review");
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT type, run_id FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([&task_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let of_run = |ty: &str| -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, (t, r))| t == ty && r.as_deref() == Some(run_id.as_str()))
            .map(|(i, _)| i)
            .collect()
    };
    let quota = of_run("run_quota");
    assert_eq!(quota.len(), 1, "{rows:?}");
    let last_status_change = *of_run("status_change").last().expect("a status_change");
    assert!(
        quota[0] < last_status_change,
        "run_quota at {} must come before the last status_change at {last_status_change}: {rows:?}",
        quota[0]
    );
}

// AC2: every outcome, and the harnesses that write none -----------------

#[test]
fn an_interrupted_claude_run_still_records_run_quota() {
    let (f, _task_id, run_id) = claude_run("quota", "interrupted");
    let q = the_run_quota(&f, &run_id);
    assert_eq!(q["source"], "claude_stream", "{q}");
    assert_float(&q["start_percent"], 99.0, "start_percent");
    assert_float(&q["end_percent"], 99.0, "end_percent");
    assert_float(&q["points"], 0.0, "points");
}

#[test]
fn an_antigravity_run_records_no_run_quota() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.home_path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_AGY_DATA_DIR", &cfg)
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", AGY_MODEL, "--json"]);
    let run_id = json(&f.run_ok(c, "dispatch"))["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert!(run_events(&f, &run_id, "run_quota").is_empty());
}

/// The rollout fake-codex wrote is where the runner looks for it.
#[test]
fn fake_codex_writes_the_rollout_only_when_asked() {
    let (f, cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, None);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert!(!cfg.join("sessions").exists(), "no rollout without FAKE_CODEX_ROLLOUT");
    // no reading at either end: the event is still written, with nulls
    let q = the_run_quota(&f, &run_id);
    assert!(q["points"].is_null(), "{q}");
    assert_eq!(q["source"], "codex_rollout", "{q}");

    let (f, cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, Some("1"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert_eq!(rollout_names(&f, &cfg, &run_id).len(), 1);
}

/// The files fake-codex wrote under `sessions/2026/01/01/`; each must be a
/// file whose name contains the run's thread id.
fn rollout_names(f: &common::Fixture, cfg: &Path, run_id: &str) -> Vec<String> {
    let thread: String = f
        .state_conn()
        .query_row("SELECT harness_session_id FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap();
    let dir = cfg.join("sessions").join("2026").join("01").join("01");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for name in &names {
        assert!(name.contains(&thread), "{names:?} must name thread {thread}");
        assert!(dir.join(name).is_file(), "{name}");
    }
    names
}
