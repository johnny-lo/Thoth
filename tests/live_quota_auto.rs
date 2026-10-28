//! QR-1: with the `live-quota` switch on, `recommend` refreshes the agy and
//! codex readings that are older than 10 minutes (or missing) before ranking,
//! against fake-codex's app-server mode and fake-agy's `/usage`; since RJ-1
//! the launch commands never do.
//! Each test is named after the acceptance criterion and case it covers
//! (`ac10_b_...` is AC10 case (b)).

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

// --- copied from tests/live_quota_codex.rs ------------------------------------

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("valid json")
}

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta).format(&Rfc3339).unwrap()
}

fn epoch(delta: Duration) -> i64 {
    (OffsetDateTime::now_utc() + delta).unix_timestamp()
}

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn add_profile(f: &Fixture, id: &str, harness: &str) -> PathBuf {
    let dir = f.home_path().join("profiles").join(id);
    fs::create_dir_all(&dir).unwrap();
    let provider = match harness {
        "claude-code" => "anthropic",
        "codex" => "openai",
        _ => "google",
    };
    f.cmd()
        .args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"])
        .arg(&dir)
        .assert()
        .success();
    dir
}

fn enable(f: &Fixture) {
    f.cmd().args(["config", "set", "live-quota", "on"]).assert().success();
}

/// The element of `list` whose `profile_id` is `id`.
fn entry<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|v| v["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {list}"))
}

/// Whether one of the candidate's reasons contains `needle`.
fn has_reason(candidate: &Value, needle: &str) -> bool {
    candidate["reasons"].as_array().unwrap().iter().filter_map(Value::as_str).any(|r| r.contains(needle))
}

fn live_quota_rows(f: &Fixture) -> i64 {
    f.global_conn()
        .query_row("SELECT COUNT(*) FROM quota_events WHERE kind='live_quota'", [], |r| r.get(0))
        .unwrap()
}

fn insert_detail(f: &Fixture, profile: &str, ts: &str, detail: &str) {
    f.global_conn()
        .execute(
            "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1, ?2, 'live_quota', ?3)",
            params![ts, profile, detail],
        )
        .unwrap();
}

/// Writes one codex live reading straight into global.db: one five_hour
/// window resetting tomorrow, and `allowed` as `ordinary_usage_allowed`.
fn insert_codex_reading(f: &Fixture, profile: &str, ts: &str, used: f64, allowed: Value) {
    let detail = json!({"source": "codex_app_server", "windows": [
        {"name": "five_hour", "used_percent": used, "resets_at": at(Duration::days(1))}
    ], "ordinary_usage_allowed": allowed});
    insert_detail(f, profile, ts, &detail.to_string());
}

fn rollout_line(timestamp: &str, limit: Value) -> String {
    json!({"timestamp": timestamp, "type": "event_msg",
        "payload": {"type": "token_count", "rate_limits": limit}}).to_string()
}

fn general_limit(used: i64) -> Value {
    json!({"limit_id": "codex", "primary": {"used_percent": used, "window_minutes": 300, "resets_at": epoch(Duration::days(1))}})
}

fn write_rollout(dir: &Path, lines: &[String]) {
    fs::create_dir_all(dir.join("sessions")).unwrap();
    fs::write(dir.join("sessions").join("rollout.jsonl"), lines.join("\n") + "\n").unwrap();
}

// --- this file's own helpers --------------------------------------------------

fn codex_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-codex-args.jsonl")
}

fn agy_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

/// A `thoth` command whose codex and agy launches are recorded.
fn thoth(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_CODEX_ARGS_LOG", codex_log(f)).env("FAKE_AGY_ARGS_LOG", agy_log(f));
    c
}

/// The `args` of every launch logged in `log`, in order.
fn logged_args(log: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
        })
        .collect()
}

/// Lines in the codex log whose first argument is `app-server`.
fn app_server_count(f: &Fixture) -> usize {
    logged_args(&codex_log(f)).iter().filter(|a| a.first().map(String::as_str) == Some("app-server")).count()
}

/// Lines in the agy log whose arguments contain `/usage`.
fn usage_count(f: &Fixture) -> usize {
    logged_args(&agy_log(f)).iter().filter(|a| a.iter().any(|x| x == "/usage")).count()
}

/// `recommend --role implementer --json` with `env`; exit 0. (stdout, stderr)
fn recommend(f: &Fixture, env: impl FnOnce(&mut assert_cmd::Command)) -> (Value, String) {
    let mut c = thoth(f);
    env(&mut c);
    let out = c.args(["recommend", "--role", "implementer", "--json"]).assert().code(0).get_output().clone();
    (parse(&out.stdout), String::from_utf8_lossy(&out.stderr).to_string())
}

fn refresh_of(v: &Value) -> &Value {
    &v["live_quota_refresh"]
}

fn stderr_lines_with(stderr: &str, needle: &str) -> usize {
    stderr.lines().filter(|l| l.contains(needle)).count()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

/// `TASK ARGS... --json` launch with the fakes' logs and a scenario.
fn launch(f: &Fixture, task: &str, scenario: &str, args: &[&str]) -> assert_cmd::Command {
    let mut c = thoth(f);
    c.env("FAKE_SCENARIO", scenario).env("FAKE_TASK_ID", task).args(args);
    c
}

// ---- AC10: recommend, and the commands that do not refresh ----

#[test]
fn ac10_a_switch_off_refreshes_nothing() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(*refresh_of(&v), json!({"enabled": false, "skipped": "off", "results": []}), "{v}");
    assert_eq!(app_server_count(&f), 0);
    assert_eq!(usage_count(&f), 0);
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac10_b_switch_on_refreshes_every_due_agy_and_codex_profile() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    add_profile(&f, "claude-a", "claude-code");
    enable(&f);
    let (v, stderr) = recommend(&f, |_| {});
    let refresh = refresh_of(&v);
    assert_eq!(refresh["enabled"], true, "{v}");
    assert_eq!(refresh["skipped"], Value::Null, "{v}");
    let results = refresh["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{v}");
    assert_eq!(results[0], json!({"profile_id": "agy-a", "status": "refreshed", "error": null}), "{v}");
    assert_eq!(results[1], json!({"profile_id": "codex-a", "status": "refreshed", "error": null}), "{v}");
    assert_eq!(app_server_count(&f), 1);
    assert_eq!(usage_count(&f), 1);
    assert_eq!(live_quota_rows(&f), 2);
    assert!(!has_reason(entry(&v["candidates"], "codex-a"), "no quota reading yet"), "{v}");
    assert_eq!(stderr_lines_with(&stderr, "live quota refresh for"), 0, "{stderr}");
}

#[test]
fn ac10_c_a_fresh_reading_is_not_refreshed() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    insert_codex_reading(&f, "codex-a", &at(Duration::minutes(-1)), 10.0, json!(true));
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(refresh_of(&v)["results"], json!([]), "{v}");
    assert_eq!(app_server_count(&f), 0);
    assert_eq!(live_quota_rows(&f), 1);
}

#[test]
fn ac10_d_a_newer_rollout_reading_counts() {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    enable(&f);
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 10.0, json!(true));
    write_rollout(&dir, &[rollout_line(&at(Duration::minutes(-1)), general_limit(20))]);
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(refresh_of(&v)["results"], json!([]), "{v}");
    assert_eq!(app_server_count(&f), 0);
}

#[test]
fn ac10_e_usage_not_allowed_without_windows_counts_as_a_reading() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    insert_detail(
        &f,
        "codex-a",
        &at(Duration::minutes(-1)),
        r#"{"source":"codex_app_server","windows":[],"ordinary_usage_allowed":false}"#,
    );
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(refresh_of(&v)["results"], json!([]), "{v}");
    assert_eq!(app_server_count(&f), 0);
    assert_eq!(entry(&v["candidates"], "codex-a")["quota_state"], "exhausted", "{v}");
}

#[test]
fn ac10_f_an_old_reading_is_replaced() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 99.0, json!(true));
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(
        refresh_of(&v)["results"],
        json!([{"profile_id": "codex-a", "status": "refreshed", "error": null}]),
        "{v}"
    );
    assert_eq!(live_quota_rows(&f), 2);
    let codex = entry(&v["candidates"], "codex-a");
    assert_eq!(codex["quota_state"], "ok", "{v}");
    assert_eq!(codex["eligible"], true, "{v}");
}

#[test]
fn ac10_g_a_failed_query_does_not_block() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 40.0, json!(true));
    let (v, stderr) = recommend(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "error");
    });
    let results = refresh_of(&v)["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{v}");
    assert_eq!(results[0]["profile_id"], "codex-a", "{v}");
    assert_eq!(results[0]["status"], "failed", "{v}");
    assert!(results[0]["error"].as_str().unwrap().contains("authentication required"), "{v}");
    let warned = stderr
        .lines()
        .filter(|l| l.contains("warning: live quota refresh for codex-a failed:") && l.contains("ranking with the previous reading"))
        .count();
    assert_eq!(warned, 1, "{stderr}");
    assert_eq!(live_quota_rows(&f), 1);
    let codex = entry(&v["candidates"], "codex-a");
    assert_eq!(codex["quota_state"], "ok", "{v}");
    assert_eq!(codex["peak_percent"].as_f64(), Some(40.0), "{v}");
}

#[test]
fn ac10_h_a_timeout_does_not_block() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    let (v, stderr) = recommend(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "hang").env("THOTH_LIVE_QUOTA_TIMEOUT_MS", "1000");
    });
    let codex = entry(&refresh_of(&v)["results"], "codex-a");
    assert_eq!(codex["status"], "failed", "{v}");
    assert!(codex["error"].as_str().unwrap().contains("timed out"), "{v}");
    assert!(
        stderr.lines().any(|l| l
            == "warning: live quota refresh for codex-a failed: timed out; ranking without a quota reading"),
        "{stderr}"
    );
    assert_eq!(stderr_lines_with(&stderr, "previous reading"), 0, "{stderr}");
    assert_eq!(live_quota_rows(&f), 0);
    assert!(has_reason(entry(&v["candidates"], "codex-a"), "no quota reading yet"), "{v}");
}

#[test]
fn ac10_i_a_codex_profile_with_a_live_run_is_skipped() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    f.state_conn()
        .execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE', 'codex-a', 'running', 'implementer')", [])
        .unwrap();
    let (v, stderr) = recommend(&f, |_| {});
    assert_eq!(
        refresh_of(&v)["results"],
        json!([{"profile_id": "codex-a", "status": "skipped_live_run", "error": null}]),
        "{v}"
    );
    assert_eq!(app_server_count(&f), 0);
    assert_eq!(stderr_lines_with(&stderr, "live quota refresh for"), 0, "{stderr}");
}

#[test]
fn ac10_j_a_logged_out_profile_is_not_queried() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    f.global_conn().execute("UPDATE profiles SET status='logged_out' WHERE profile_id='codex-a'", []).unwrap();
    let (v, _) = recommend(&f, |_| {});
    assert_eq!(refresh_of(&v)["results"], json!([]), "{v}");
    assert_eq!(app_server_count(&f), 0);
}

#[test]
fn ac10_k_inside_a_worker_nothing_is_refreshed() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    let (v, _) = recommend(&f, |c| {
        c.env("THOTH_RUN_ID", "01QR1TESTRUN0000000000000");
    });
    assert_eq!(*refresh_of(&v), json!({"enabled": true, "skipped": "worker", "results": []}), "{v}");
    assert_eq!(app_server_count(&f), 0);
}

#[test]
fn ac10_l_an_invalid_role_refreshes_nothing() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    enable(&f);
    thoth(&f).args(["recommend", "--role", "bogus", "--json"]).assert().code(1);
    assert_eq!(app_server_count(&f), 0);
}

#[test]
fn ac10_m_other_commands_do_not_refresh() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    enable(&f);
    thoth(&f).args(["profile", "usage", "--json"]).assert().code(0);
    thoth(&f).args(["status", "--json"]).assert().code(0);
    thoth(&f).args(["report", "--json"]).assert().code(0);
    assert_eq!(app_server_count(&f), 0);
    assert_eq!(usage_count(&f), 0);
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac10_n_human_output_is_unchanged() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 40.0, json!(true));
    let off = thoth(&f).args(["recommend", "--role", "implementer"]).assert().code(0).get_output().clone();
    enable(&f);
    let on = thoth(&f)
        .env("FAKE_CODEX_APP_SERVER", "error")
        .args(["recommend", "--role", "implementer"])
        .assert()
        .code(0)
        .get_output()
        .clone();
    assert_eq!(String::from_utf8_lossy(&off.stdout), String::from_utf8_lossy(&on.stdout));
    let stderr = String::from_utf8_lossy(&on.stderr);
    assert_eq!(stderr_lines_with(&stderr, "warning: live quota refresh for codex-a failed:"), 1, "{stderr}");
}

#[test]
fn ac10_o_enabling_notice_names_the_automatic_refresh() {
    let f = setup();
    let out = thoth(&f).args(["config", "set", "live-quota", "on", "--json"]).assert().success().get_output().stdout.clone();
    let notice = s(&parse(&out)["notice"]);
    for needle in ["recommend", "10 minutes"] {
        assert!(notice.contains(needle), "notice lacks {needle:?}: {notice}");
    }
    assert!(!notice.contains("only when you explicitly"), "{notice}");
}

#[test]
fn ac10_p_registry_description_names_refresh() {
    let f = setup();
    let out = thoth(&f).args(["config", "list", "--json"]).assert().success().get_output().stdout.clone();
    let v = parse(&out);
    let setting = v["settings"].as_array().unwrap().iter().find(|s| s["key"] == "live-quota").unwrap();
    let description = s(&setting["description"]);
    assert!(!description.contains("only when you ask"), "{description}");
    assert!(description.contains("--refresh"), "{description}");
}

#[test]
fn ac3_a_failed_repo_listing_refreshes_nothing() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    enable(&f);
    // Renaming the table is the cheapest way to make the repo listing fail
    // while the profile listing still works. An empty repo list would count
    // codex's live runs as zero, so nothing may be refreshed.
    f.global_conn().execute("ALTER TABLE repos RENAME TO repos_hidden", []).unwrap();
    let out = thoth(&f).args(["recommend", "--role", "implementer", "--json"]).output().unwrap();
    assert!(logged_args(&codex_log(&f)).is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(logged_args(&agy_log(&f)).is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(live_quota_rows(&f), 0);
}

// ---- AC11: the three launch commands ----

/// RJ-1: only `recommend` refreshes; a dispatch neither refreshes nor
/// reports a `live_quota_refresh`.
#[test]
fn ac11_b_a_named_profile_does_not_refresh() {
    let f = setup();
    enable(&f);
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = parse(&f.run_ok(
        launch(&f, &task, "ok", &["dispatch", &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--json"]),
        "dispatch",
    ));
    assert!(d.get("live_quota_refresh").is_none(), "{d}");
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    assert_eq!(app_server_count(&f), 0);
}

