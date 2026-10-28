//! FC-1: failure classes (`auth`, `transient`), the runner's login check,
//! the narrower quota match, and accounts that recover on their own.
//! Each test is named after AC8's case it covers. Everything runs on the
//! fakes and a temporary THOTH_HOME; the agy error texts and Claude's OAuth
//! text are recordings, the `overloaded`, codex disconnect and `auth_expired`
//! texts are synthetic (see the fakes).

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

// ---- copied from tests/antigravity.rs

const MODEL: &str = "gemini-3.8-flash-low";

/// `Fixture::add_agy_profile`: registers an antigravity profile.
trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf;
}

impl AgyFixture for common::Fixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd().args(["profile", "add", id, "--provider", "google", "--harness",
            "antigravity", "--config-dir"]).arg(&cfg).assert().success();
        cfg
    }
}

const CONV: &str = "11111111-2222-4333-8444-555555555555";

fn agy_args_log(f: &common::Fixture) -> std::path::PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

fn agy_env(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", agy_args_log(f))
        .env("THOTH_RETRY_WAITS", "0,0");
    c
}

fn agy_dispatch(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
    model: &str,
) -> Value {
    agy_dispatch_with(f, cfg, task_id, scenario, model, &[])
}

fn agy_dispatch_with(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
    model: &str,
    extra: &[&str],
) -> Value {
    let out = agy_env(f, cfg, task_id, scenario)
        .args(["dispatch", task_id, "--profile", "agy-a", "--model", model, "--json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn run_row(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<String>, Option<String>) {
    f.state_conn()
        .query_row(
            "SELECT status, interrupted_reason, error_text, harness_session_id FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

// ---- copied from tests/outcomes.rs

fn run_status(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<i64>, Option<String>) {
    let conn = f.state_conn();
    conn.query_row(
        "SELECT status, interrupted_reason, exit_code, error_text FROM runs WHERE run_id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

// ---- copied from tests/live_quota.rs

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta).format(&Rfc3339).unwrap()
}

fn stored_window(name: &str, used: f64, resets_at: Option<String>, group: &str, description: &str) -> Value {
    json!({"name": name, "used_percent": used, "resets_at": resets_at, "group": group,
        "group_description": description})
}

fn insert_detail(f: &Fixture, profile: &str, ts: &str, detail: &str) {
    f.global_conn()
        .execute(
            "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1, ?2, 'live_quota', ?3)",
            params![ts, profile, detail],
        )
        .unwrap();
}

/// Writes one well-formed `live_quota` row straight into global.db.
fn insert_reading(f: &Fixture, profile: &str, ts: &str, windows: Vec<Value>) {
    insert_detail(f, profile, ts, &json!({"source": "agy_usage", "windows": windows}).to_string());
}

// ---- copied from tests/usage_recommend.rs

fn epoch(delta: Duration) -> i64 {
    (OffsetDateTime::now_utc() + delta).unix_timestamp()
}

fn add_event(f: &Fixture, run_id: &str, profile: &str, ts: &str, detail: Value) {
    let conn = f.state_conn();
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, 'rate_limit_warning', ?3)",
        params![ts, run_id, detail.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO runs (run_id, account, status, role) VALUES (?1, ?2, 'completed', 'implementer')",
        params![run_id, profile],
    )
    .unwrap();
}

fn candidate<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == id)
        .unwrap()
}

// ---- this file's own helpers

const GEMINI: &str = "Gemini Models";
const GEMINI_DESCRIPTION: &str = "Models within this group: Gemini Flash, Gemini Pro";
const OTHER: &str = "Claude and GPT models";
const OTHER_DESCRIPTION: &str = "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS";

const TRANSIENT_STEP: &str =
    "a temporary problem (network or a busy service), not a quota or login problem; run thoth resume";

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn profile_status(f: &Fixture, id: &str) -> String {
    f.global_conn()
        .query_row("SELECT status FROM profiles WHERE profile_id = ?1", [id], |r| r.get(0))
        .unwrap()
}

/// Every quota event kind of the profile, in seq order.
fn quota_kinds(f: &Fixture, id: &str) -> Vec<String> {
    let conn = f.global_conn();
    let mut stmt = conn
        .prepare("SELECT kind FROM quota_events WHERE profile_id = ?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

/// The detail of the profile's newest quota event of `kind`.
fn newest_quota_detail(f: &Fixture, id: &str, kind: &str) -> Value {
    let detail: String = f
        .global_conn()
        .query_row(
            "SELECT detail FROM quota_events WHERE profile_id = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT 1",
            [id, kind],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&detail).unwrap()
}

/// The detail of the run's newest event of `ty`.
fn event_detail(f: &Fixture, run_id: &str, ty: &str) -> Value {
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            [run_id, ty],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&detail).unwrap()
}

/// Every event type of the run, in seq order.
fn events_of_run(f: &Fixture, run_id: &str) -> Vec<String> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT type FROM events WHERE run_id = ?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([run_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

fn status_json(f: &Fixture, task: &str) -> Value {
    let mut c = f.cmd();
    c.args(["status", task, "--json"]);
    let out = f.run_ok(c, "status --json");
    serde_json::from_slice(&out).unwrap()
}

fn status_human(f: &Fixture, task: &str) -> String {
    let mut c = f.cmd();
    c.args(["status", task]);
    String::from_utf8(f.run_ok(c, "status")).unwrap()
}

/// `thoth recommend EXTRA --json`; the caller gives `--role`.
fn recommend_json(f: &Fixture, extra: &[&str]) -> Value {
    let mut c = f.cmd();
    c.arg("recommend").args(extra).arg("--json");
    let out = f.run_ok(c, "recommend");
    serde_json::from_slice(&out).unwrap()
}

fn run_of(f: &Fixture, task: &str, run_id: &str) -> Value {
    status_json(f, task)["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap()
        .clone()
}

fn report_page(f: &Fixture) -> String {
    let mut c = f.cmd();
    c.args(["report", "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "report")).unwrap();
    std::fs::read_to_string(v["path"].as_str().unwrap()).unwrap()
}

fn has(list: &[String], item: &str) -> bool {
    list.iter().any(|x| x == item)
}

fn park(f: &Fixture, id: &str, since: &str) {
    let g = f.global_conn();
    g.execute("UPDATE profiles SET status='rate_limited' WHERE profile_id=?1", [id]).unwrap();
    g.execute(
        "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1, ?2, 'rate_limited', '{}')",
        params![since, id],
    )
    .unwrap();
}

fn warnings_of(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
}

fn resume(f: &Fixture, task: &str, run: &str) {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .args(["resume", run, "--json"]);
    f.run_ok(c, "resume");
}

// ---- AC8

#[test]
fn an_overloaded_claude_error_is_a_transient_interruption_and_resumes() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "overloaded")
        .env("FAKE_TASK_ID", &task)
        .env("THOTH_RETRY_WAITS", "0,0")
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d: Value =
        serde_json::from_str(String::from_utf8(f.run_ok(c, "dispatch")).unwrap().trim()).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");

    let (status, reason, exit, error) = run_status(&f, &run);
    assert_eq!(status, "interrupted");
    assert_eq!(reason.as_deref(), Some("transient"));
    assert_eq!(exit, Some(1));
    let error = error.unwrap();
    assert!(error.starts_with("transient interruption:"), "{error}");
    assert!(error.contains("Overloaded"), "{error}");

    assert_eq!(profile_status(&f, "claude-b"), "active");
    let kinds = quota_kinds(&f, "claude-b");
    assert!(!has(&kinds, "rate_limited"), "{kinds:?}");
    assert!(!has(&kinds, "logged_out"), "{kinds:?}");

    let events = events_of_run(&f, &run);
    assert!(has(&events, "run_interrupted"), "{events:?}");
    assert!(!has(&events, "quota_exceeded"), "{events:?}");
    assert!(!has(&events, "login_failed"), "{events:?}");
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["interrupted_reason"], "transient", "{change}");
    assert_eq!(change["login_check"], "logged_in", "{change}");
    assert_eq!(change["retries"], 2, "{change}");
    assert_eq!(change["retry_stop"], "exhausted", "{change}");
    assert_eq!(events.iter().filter(|e| *e == "run_retry").count(), 2, "{events:?}");

    let row = run_of(&f, &task, &run);
    assert_eq!(row["interrupted_reason"], "transient");
    assert_eq!(row["next_step"], format!("{TRANSIENT_STEP} {run}"));
    let human = status_human(&f, &task);
    assert!(human.contains("reason=transient"), "{human}");
    assert!(human.lines().any(|l| l.starts_with("  next: a temporary problem")), "{human}");

    resume(&f, &task, &run);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
}

#[test]
fn the_three_recorded_agy_network_failures_are_transient() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let cases = [
        ("network", Some(3), "There was a network issue"),
        ("eligibility", Some(1), "Eligibility check failed"),
        ("stream_interrupted", Some(0), "The stream was interrupted"),
    ];
    let mut runs = Vec::new();
    for (scenario, exit, sentence) in cases {
        let (task, _) = f.create_task(Fixture::SPEC);
        let d = agy_dispatch(&f, &cfg, &task, scenario, MODEL);
        let run = d["run_id"].as_str().unwrap().to_string();
        assert_eq!(f.wait_for(&task, "120s").1, "interrupted", "{scenario}");
        let (status, reason, code, error) = run_status(&f, &run);
        assert_eq!(status, "interrupted", "{scenario}");
        assert_eq!(reason.as_deref(), Some("transient"), "{scenario}");
        assert_eq!(code, exit, "{scenario}");
        let error = error.unwrap();
        assert!(error.contains(sentence), "{scenario}: {error}");
        let change = event_detail(&f, &run, "status_change");
        assert_eq!(change["retries"], 2, "{scenario}: {change}");
        assert_eq!(change["retry_stop"], "exhausted", "{scenario}: {change}");
        runs.push((task, run));
    }
    assert_eq!(run_row(&f, &runs[0].1).3.as_deref(), Some(CONV));
    assert_eq!(run_row(&f, &runs[1].1).3, None);
    assert_eq!(run_row(&f, &runs[2].1).3.as_deref(), Some(CONV));

    let second = run_of(&f, &runs[1].0, &runs[1].1);
    let step = second["next_step"].as_str().unwrap();
    assert!(step.contains("thoth dispatch"), "{step}");
    assert!(!step.contains("thoth resume"), "{step}");

    assert_eq!(profile_status(&f, "agy-a"), "active");
    assert!(!has(&quota_kinds(&f, "agy-a"), "rate_limited"));

    let (first_task, first_run) = &runs[0];
    let mut c = agy_env(&f, &cfg, first_task, "resume");
    c.args(["resume", first_run, "--json"]);
    f.run_ok(c, "resume");
    assert_eq!(f.wait_for(first_task, "120s").1, "in_review");
}

#[test]
fn a_codex_stream_disconnect_is_transient() {
    let f = setup();
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "disconnected")
        .env("FAKE_TASK_ID", &task)
        .env("THOTH_RETRY_WAITS", "0,0")
        .args(["dispatch", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"]);
    let d: Value =
        serde_json::from_str(String::from_utf8(f.run_ok(c, "dispatch")).unwrap().trim()).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    let (status, reason, _, error) = run_status(&f, &run);
    assert_eq!(status, "interrupted");
    assert_eq!(reason.as_deref(), Some("transient"));
    let error = error.unwrap();
    assert!(error.contains("stream disconnected before completion"), "{error}");
    assert_eq!(profile_status(&f, "codex-a"), "active");
    assert_eq!(event_detail(&f, &run, "status_change")["login_check"], "logged_in");
    assert_eq!(event_detail(&f, &run, "status_change")["retries"], 2);
}

#[test]
fn a_quota_stop_still_parks_and_skips_the_login_check() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "quota", &[]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run).1.as_deref(), Some("quota"));
    assert_eq!(profile_status(&f, "claude-b"), "rate_limited");
    let parked = newest_quota_detail(&f, "claude-b", "rate_limited");
    assert_eq!(parked.get("quota_group"), Some(&Value::Null), "{parked}");
    let exceeded = event_detail(&f, &run, "quota_exceeded");
    assert_eq!(exceeded.get("quota_group"), Some(&Value::Null), "{exceeded}");
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change.get("login_check"), Some(&Value::Null), "{change}");
}

#[test]
fn a_claude_run_whose_session_expired_is_an_auth_interruption_and_the_profile_is_logged_out() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let marker = f.home_path().join("logout.marker");
    let dispatch = || {
        let mut c = f.cmd();
        c.env("FAKE_SCENARIO", "auth_expired")
            .env("FAKE_TASK_ID", &task)
            .env("FAKE_LOGOUT_MARKER", &marker)
            .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
        c
    };
    // The marker does not exist yet, so dispatch's own auth check passes.
    let d: Value = serde_json::from_slice(&f.run_ok(dispatch(), "dispatch")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");

    let (status, reason, exit, error) = run_status(&f, &run);
    assert_eq!(status, "interrupted");
    assert_eq!(reason.as_deref(), Some("auth"));
    assert_eq!(exit, Some(1));
    let error = error.unwrap();
    assert!(error.starts_with("login failure: claude-b is not logged in; "), "{error}");

    assert_eq!(profile_status(&f, "claude-b"), "logged_out");
    let kinds = quota_kinds(&f, "claude-b");
    assert_eq!(kinds.iter().filter(|k| *k == "logged_out").count(), 1, "{kinds:?}");
    let logged_out = newest_quota_detail(&f, "claude-b", "logged_out");
    assert_eq!(logged_out["run_id"], run.as_str());
    assert_eq!(logged_out["task_id"], task.as_str());

    let events = events_of_run(&f, &run);
    assert!(has(&events, "login_failed"), "{events:?}");
    assert_eq!(event_detail(&f, &run, "login_failed")["profile"], "claude-b");
    assert!(has(&events, "run_interrupted"), "{events:?}");
    assert_eq!(event_detail(&f, &run, "run_interrupted")["reason"], "auth");
    assert!(!has(&events, "quota_exceeded"), "{events:?}");
    assert_eq!(event_detail(&f, &run, "status_change")["login_check"], "signed_out");

    assert_eq!(
        run_of(&f, &task, &run)["next_step"],
        format!("claude-b is logged out; run thoth profile login claude-b, then thoth resume {run}")
    );
    let human = status_human(&f, &task);
    assert!(human.contains("reason=auth"), "{human}");

    let rec = recommend_json(&f, &["--role", "implementer"]);
    let c = candidate(&rec, "claude-b");
    assert_eq!(c["eligible"], false, "{c}");
    assert!(
        c["reasons"].as_array().unwrap().iter().any(|r| {
            let r = r.as_str().unwrap();
            r.contains("status logged_out") && r.contains("thoth profile login claude-b")
        }),
        "{c}"
    );

    // Still logged out: dispatch refuses and says how to log in again.
    let out = dispatch().output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("failed its auth check"), "{stderr}");
    assert!(stderr.contains("thoth profile login claude-b"), "{stderr}");

    let page = report_page(&f);
    assert!(page.contains(">已登出<"));
    assert!(page.contains("登入失效（claude-b）"));
    assert!(page.contains("thoth profile login claude-b"));

    std::fs::remove_file(&marker).unwrap();
    let mut check = f.cmd();
    check.args(["profile", "check", "claude-b", "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(check, "profile check")).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["status"], "active", "{v}");
    assert_eq!(profile_status(&f, "claude-b"), "active");
    assert_eq!(quota_kinds(&f, "claude-b").last().map(String::as_str), Some("login_restored"));
    assert_eq!(newest_quota_detail(&f, "claude-b", "login_restored")["source"], "profile check");

    resume(&f, &task, &run);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
}

#[test]
fn a_codex_run_whose_session_expired_is_an_auth_interruption() {
    let f = setup();
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let marker = f.home_path().join("logout.marker");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "auth_expired")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_LOGOUT_MARKER", &marker)
        .args(["dispatch", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run).1.as_deref(), Some("auth"));
    assert_eq!(profile_status(&f, "codex-a"), "logged_out");
    assert_eq!(event_detail(&f, &run, "status_change")["login_check"], "signed_out");
    assert!(has(&quota_kinds(&f, "codex-a"), "logged_out"));
}

#[test]
fn an_agy_run_whose_session_expired_is_an_auth_interruption() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let marker = f.home_path().join("logout.marker");
    let mut c = agy_env(&f, &cfg, &task, "auth_expired");
    c.env("FAKE_LOGOUT_MARKER", &marker)
        .args(["dispatch", &task, "--profile", "agy-a", "--model", MODEL, "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run).1.as_deref(), Some("auth"));
    assert_eq!(profile_status(&f, "agy-a"), "logged_out");
    assert_eq!(event_detail(&f, &run, "status_change")["login_check"], "signed_out");
    assert!(has(&quota_kinds(&f, "agy-a"), "logged_out"));
}

#[test]
fn a_login_check_that_cannot_run_or_is_unclear_does_not_reclassify() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "auth_check_breaks")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_AUTH_FAIL_MARKER", f.home_path().join("auth-fail-claude.marker"))
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "failed");
    let (status, reason, _, _) = run_status(&f, &run);
    assert_eq!(status, "failed");
    assert_eq!(reason, None);
    assert_eq!(profile_status(&f, "claude-b"), "active");
    let check = event_detail(&f, &run, "status_change")["login_check"].clone();
    assert!(check.as_str().unwrap().starts_with("error:"), "{check}");

    // agy: its own, not yet written marker, on another task.
    let cfg = f.add_agy_profile("agy-a");
    let (task2, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_env(&f, &cfg, &task2, "auth_check_breaks");
    c.env("FAKE_AUTH_FAIL_MARKER", f.home_path().join("auth-fail-agy.marker"))
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", MODEL, "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run2 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task2, "120s").1, "failed");
    let (status, reason, _, _) = run_status(&f, &run2);
    assert_eq!(status, "failed");
    assert_eq!(reason, None);
    assert_eq!(profile_status(&f, "agy-a"), "active");
    assert_eq!(event_detail(&f, &run2, "status_change")["login_check"], "unknown");

    assert!(!has(&quota_kinds(&f, "claude-b"), "logged_out"));
    assert!(!has(&quota_kinds(&f, "agy-a"), "logged_out"));
}

#[test]
fn a_named_dispatch_marks_a_recovered_profile_active_again() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    park(&f, "claude-b", &at(Duration::hours(-2)));
    add_event(&f, "R", "claude-b", &at(Duration::hours(-1)), json!({
        "rate_limit_type": "five_hour", "utilization": 0.99,
        "resets_at": epoch(Duration::minutes(-30))}));
    let d = f.dispatch(&task, "claude-b", "ok", &["--model", "sonnet", "--effort", "medium"]);
    let warnings = warnings_of(&d);
    assert!(!warnings.iter().any(|w| w.contains("rate_limited")), "{d}");
    assert_eq!(profile_status(&f, "claude-b"), "active");
    assert_eq!(quota_kinds(&f, "claude-b").last().map(String::as_str), Some("window_reset"));
    let reset = newest_quota_detail(&f, "claude-b", "window_reset");
    assert_eq!(reset["source"], "auto", "{reset}");
    // RJ-1: there is no automatic profile pick to tell apart any more
    assert!(reset.get("profile_auto").is_none(), "{reset}");
    assert_eq!(reset["run_id"], d["run_id"], "{reset}");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    // The same, but the window has not reset yet: warned, still parked.
    let f2 = setup();
    f2.add_profile("claude-b");
    let (task2, _) = f2.create_task(Fixture::SPEC);
    park(&f2, "claude-b", &at(Duration::hours(-2)));
    add_event(&f2, "R", "claude-b", &at(Duration::hours(-1)), json!({
        "rate_limit_type": "five_hour", "utilization": 0.99,
        "resets_at": epoch(Duration::hours(1))}));
    let d2 = f2.dispatch(&task2, "claude-b", "ok", &["--model", "sonnet"]);
    let warnings = warnings_of(&d2);
    assert_eq!(
        warnings.iter().filter(|w| w.contains("marked rate_limited since")).count(),
        1,
        "{d2}"
    );
    assert_eq!(profile_status(&f2, "claude-b"), "rate_limited");
    assert!(!has(&quota_kinds(&f2, "claude-b"), "window_reset"));
    f2.wait_for(&task2, "120s");
}

#[test]
fn a_named_review_start_marks_a_recovered_profile_active_again() {
    let f = setup();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task, "claude-a", "hooks", &[]);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    park(&f, "claude-b", &at(Duration::hours(-2)));
    add_event(&f, "R", "claude-b", &at(Duration::hours(-1)), json!({
        "rate_limit_type": "five_hour", "utilization": 0.99,
        "resets_at": epoch(Duration::minutes(-30))}));
    let v = f.review_start(&task, "claude-b", "review_pass", &["--model", "opus"]);
    let warnings = warnings_of(&v);
    assert!(!warnings.iter().any(|w| w.contains("rate_limited")), "{v}");
    assert_eq!(profile_status(&f, "claude-b"), "active");
    let run = v["run_id"].as_str().unwrap().to_string();
    let mut c = f.cmd();
    c.args(["wait", "--run", &run, "--timeout", "120s", "--json"]);
    f.run_ok(c, "wait --run");
}

#[test]
fn an_agy_quota_stop_parks_only_its_model_group() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    insert_reading(&f, "agy-a", &at(Duration::minutes(-1)), vec![
        stored_window("gemini-weekly", 99.0, Some(at(Duration::days(1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 10.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = agy_dispatch(&f, &cfg, &task, "quota", "gemini-3.8-flash-medium");
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run).1.as_deref(), Some("quota"));
    assert_eq!(profile_status(&f, "agy-a"), "rate_limited");
    assert_eq!(newest_quota_detail(&f, "agy-a", "rate_limited")["quota_group"], GEMINI);
    assert_eq!(event_detail(&f, &run, "quota_exceeded")["quota_group"], GEMINI);

    let rec = recommend_json(&f, &["--role", "implementer", "--model", "claude-sonnet-4-6"]);
    let c = candidate(&rec, "agy-a");
    assert_eq!(c["eligible"], true, "{c}");
    assert_eq!(c["recovered"], false, "{c}");
    assert_eq!(c["parked_group"], GEMINI, "{c}");
    assert_eq!(c["quota_group"], OTHER, "{c}");
    assert!(
        c["reasons"].as_array().unwrap().iter()
            .any(|r| r.as_str().unwrap().contains("parked for quota group Gemini Models")),
        "{c}"
    );
    let rec = recommend_json(&f, &["--role", "implementer", "--model", "gemini-3.8-flash-medium"]);
    assert_eq!(candidate(&rec, "agy-a")["eligible"], false, "{rec}");

    let mut usage = f.cmd();
    usage.args(["profile", "usage", "agy-a", "--json"]);
    let u: Value = serde_json::from_slice(&f.run_ok(usage, "profile usage")).unwrap();
    assert_eq!(u["profiles"][0]["parked_group"], GEMINI, "{u}");

    let (task2, _) = f.create_task(Fixture::SPEC);
    // AGY-1: a name that carries no level needs --effort like any other model
    let d2 = agy_dispatch_with(&f, &cfg, &task2, "ok", "claude-sonnet-4-6", &["--effort", "medium"]);
    assert!(!warnings_of(&d2).iter().any(|w| w.contains("rate_limited")), "{d2}");
    assert_eq!(f.wait_for(&task2, "120s").1, "in_review");
    assert_eq!(profile_status(&f, "agy-a"), "rate_limited");

    assert!(report_page(&f).contains("撞到額度（Gemini Models）"));
}

#[test]
fn an_agy_quota_stop_without_a_usage_reading_parks_the_whole_profile() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    agy_dispatch(&f, &cfg, &task, "quota", "gemini-3.8-flash-medium");
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    let parked = newest_quota_detail(&f, "agy-a", "rate_limited");
    assert_eq!(parked.get("quota_group"), Some(&Value::Null), "{parked}");
    let rec = recommend_json(&f, &["--role", "implementer", "--model", "claude-sonnet-4-6"]);
    let c = candidate(&rec, "agy-a");
    assert_eq!(c["eligible"], false, "{c}");
    assert_eq!(c["parked_group"], Value::Null, "{c}");
    assert!(
        c["reasons"].as_array().unwrap().iter()
            .any(|r| r.as_str().unwrap().contains("thoth profile check agy-a")),
        "{c}"
    );
}

#[test]
fn profile_login_restores_a_logged_out_profile() {
    let f = setup();
    // No login marker yet, so `auth status` says logged out until the login.
    f.add_profile("claude-b");
    f.global_conn()
        .execute("UPDATE profiles SET status='logged_out' WHERE profile_id='claude-b'", [])
        .unwrap();
    let user_home = tempfile::tempdir().unwrap();
    let mut c = f.cmd();
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_LOGIN_FAIL")
        .env("HOME", user_home.path())
        .env("USERPROFILE", user_home.path())
        .args(["profile", "login", "claude-b", "--json"]);
    f.run_ok(c, "profile login");
    assert_eq!(profile_status(&f, "claude-b"), "active");
    assert_eq!(quota_kinds(&f, "claude-b").last().map(String::as_str), Some("login_restored"));
    assert_eq!(newest_quota_detail(&f, "claude-b", "login_restored")["source"], "profile login");
}

#[test]
fn status_human_lists_the_reason_and_the_next_step_once() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    f.dispatch(&task, "claude-b", "quota", &[]);
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    let human = status_human(&f, &task);
    assert!(human.contains(" reason=quota"), "{human}");
    let next: Vec<&str> = human.lines().filter(|l| l.starts_with("  next: ")).collect();
    assert_eq!(next.len(), 1, "{human}");
    assert!(next[0].contains("quota used up on claude-b"), "{human}");
}
