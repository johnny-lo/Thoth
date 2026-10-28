//! DT-1: a `run_quota` event also carries the weekly window and whether its
//! numbers can be trusted as the run's own, and a reviewer run writes one too.
//! Driven end to end through `fake-claude` and `fake-codex`; never a real
//! harness, never the network.

mod common;

use serde_json::Value;
use std::path::PathBuf;

const CODEX_MODEL: &str = "gpt-5.6-luna";

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
    write_models_cache(&cfg);
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

fn write_models_cache(cfg: &std::path::Path) {
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

fn assert_reliable(q: &Value) {
    assert_eq!(q["reliable"], true, "{q}");
    assert_eq!(q["unreliable_reasons"], serde_json::json!([]), "{q}");
}

// AC4 (a) ------------------------------------------------------------------

#[test]
fn a_claude_ok_run_records_the_weekly_window_beside_the_five_hour_one() {
    let (f, _task_id, run_id) = claude_run("ok", "in_review");
    let q = the_run_quota(&f, &run_id);
    // the five keys of S3-12, unchanged
    assert_eq!(q["window"], "five_hour", "{q}");
    assert_float(&q["start_percent"], 7.0, "start_percent");
    assert_float(&q["end_percent"], 7.0, "end_percent");
    assert_float(&q["points"], 0.0, "points");
    assert_eq!(q["source"], "claude_stream", "{q}");
    // the weekly window of the same readings (seven_day 0.02)
    assert_float(&q["seven_day"]["start_percent"], 2.0, "seven_day.start_percent");
    assert_float(&q["seven_day"]["end_percent"], 2.0, "seven_day.end_percent");
    assert_float(&q["seven_day"]["points"], 0.0, "seven_day.points");
    assert_reliable(&q);
}

// AC4 (b) ------------------------------------------------------------------

#[test]
fn a_codex_run_with_a_rollout_records_both_windows_and_models_counts_the_weekly_one() {
    let (f, _cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, Some("1"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));

    let q = the_run_quota(&f, &run_id);
    assert_float(&q["start_percent"], 10.0, "start_percent");
    assert_float(&q["end_percent"], 13.0, "end_percent");
    assert_float(&q["points"], 3.0, "points");
    assert_eq!(q["source"], "codex_rollout", "{q}");
    assert_float(&q["seven_day"]["start_percent"], 2.0, "seven_day.start_percent");
    assert_float(&q["seven_day"]["end_percent"], 2.0, "seven_day.end_percent");
    assert_float(&q["seven_day"]["points"], 0.0, "seven_day.points");
    assert_reliable(&q);

    let out = f.cmd().args(["models", "--json"]).assert().success().get_output().stdout.clone();
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
    let g = &breakdown[0];
    assert_float(&g["quota_points"], 3.0, "quota_points");
    assert_eq!(g["quota_runs"], 1, "{g}");
    assert_float(&g["quota_points_weekly"], 0.0, "quota_points_weekly");
    assert_eq!(g["quota_runs_weekly"], 1, "{g}");
    assert_eq!(g["quota_runs_unreliable"], 0, "{g}");
}

// AC4 (c): the two windows are judged on their own -------------------------

#[test]
fn a_five_hour_reset_mid_run_leaves_the_weekly_numbers_alone() {
    let (f, _cfg, task_id) = codex_setup();
    let run_id = dispatch_codex(&f, &task_id, Some("reset"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));

    let q = the_run_quota(&f, &run_id);
    assert!(q["start_percent"].is_null(), "{q}");
    assert!(q["end_percent"].is_null(), "{q}");
    assert!(q["points"].is_null(), "{q}");
    assert_eq!(q["source"], "codex_rollout", "{q}");
    // the fake's two lines share the weekly window's resets_at
    assert_float(&q["seven_day"]["start_percent"], 2.0, "seven_day.start_percent");
    assert_float(&q["seven_day"]["end_percent"], 2.0, "seven_day.end_percent");
    assert_float(&q["seven_day"]["points"], 0.0, "seven_day.points");
}

// AC4 (d): a codex reviewer ------------------------------------------------

#[test]
fn a_codex_reviewer_run_records_run_quota_from_its_rollout() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let cfg = f.add_codex_profile("codex-a");
    write_models_cache(&cfg);
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &["--model", "sonnet"]);
    let implementer_run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review", "{}", f.explain_run(&implementer_run));

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_CODEX_ROLLOUT", "1")
        .args(["review", "start", &task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    let reviewer_run = json(&f.run_ok(c, "review start"))["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    let status: String = f
        .state_conn()
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&reviewer_run], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "completed", "{}", f.explain_run(&reviewer_run));

    let q = the_run_quota(&f, &reviewer_run);
    assert_float(&q["points"], 3.0, "points");
    assert_float(&q["start_percent"], 10.0, "start_percent");
    assert_float(&q["end_percent"], 13.0, "end_percent");
    assert_eq!(q["source"], "codex_rollout", "{q}");
    assert_float(&q["seven_day"]["points"], 0.0, "seven_day.points");
    assert_reliable(&q);

    // before the reviewer run's last status_change
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT type FROM events WHERE run_id = ?1 ORDER BY seq")
        .unwrap();
    let types: Vec<String> = stmt
        .query_map([&reviewer_run], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let quota = types.iter().position(|t| t == "run_quota").expect("a run_quota");
    let last_status_change = types.iter().rposition(|t| t == "status_change").expect("a status_change");
    assert!(quota < last_status_change, "{types:?}");
}
