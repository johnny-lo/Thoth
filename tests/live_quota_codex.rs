//! S3-11: `profile usage --refresh` against fake-codex's app-server mode, the
//! stored codex `live_quota` readings, how they are merged with the rollout
//! reading, and "ordinary usage not allowed".
//! Each test is named after the acceptance criterion and case it covers
//! (`ac8_b_...` is AC8 case (b)); `..._rule_...` tests cover a rule of that
//! criterion that has no lettered case.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

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

/// Where fake-codex appends one JSON line per launch.
fn args_log(f: &Fixture) -> PathBuf {
    f.home_path().join("codex-args.log")
}

/// A `thoth` command whose codex launches (if any) are recorded in `args_log`.
fn thoth(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_CODEX_ARGS_LOG", args_log(f));
    c
}

/// The codex launches fake-codex recorded; none when it never ran.
fn launches(f: &Fixture) -> Vec<Value> {
    match fs::read_to_string(args_log(f)) {
        Ok(text) => text.lines().map(|l| serde_json::from_str(l).unwrap()).collect(),
        Err(_) => Vec::new(),
    }
}

/// `profile usage --refresh --json` after `env` adjusts the command. It must
/// exit 0: a failed refresh is reported in `refresh`, not as an error.
fn refresh_json(f: &Fixture, env: impl FnOnce(&mut assert_cmd::Command)) -> Value {
    let mut c = thoth(f);
    env(&mut c);
    parse(&c.args(["profile", "usage", "--refresh", "--json"]).assert().code(0).get_output().stdout)
}

/// `profile usage --json`, without `--refresh`.
fn usage_json(f: &Fixture) -> Value {
    parse(&thoth(f).args(["profile", "usage", "--json"]).assert().code(0).get_output().stdout)
}

/// `recommend --role implementer --json`.
fn recommend_json(f: &Fixture) -> Value {
    parse(&thoth(f).args(["recommend", "--role", "implementer", "--json"]).assert().code(0).get_output().stdout)
}

/// The element of `list` whose `profile_id` is `id`.
fn entry<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|v| v["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {list}"))
}

/// The window named `name` of profile `id` in a `profile usage --json` output.
fn window<'a>(usage: &'a Value, id: &str, name: &str) -> &'a Value {
    entry(&usage["profiles"], id)["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == name)
        .unwrap_or_else(|| panic!("no window {name} in {usage}"))
}

/// Whether one of the candidate's reasons contains every needle.
fn has_reason(candidate: &Value, needles: &[&str]) -> bool {
    candidate["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .any(|r| needles.iter().all(|n| r.contains(n)))
}

fn live_quota_rows(f: &Fixture) -> i64 {
    f.global_conn()
        .query_row("SELECT COUNT(*) FROM quota_events WHERE kind='live_quota'", [], |r| r.get(0))
        .unwrap()
}

/// The detail of the newest `live_quota` row, as text.
fn newest_detail(f: &Fixture) -> String {
    f.global_conn()
        .query_row(
            "SELECT detail FROM quota_events WHERE kind='live_quota' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
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

/// One rollout `token_count` line, written the way tests/live_quota.rs does.
fn rollout_line(timestamp: &str, limit: Value) -> String {
    json!({"timestamp": timestamp, "type": "event_msg",
        "payload": {"type": "token_count", "rate_limits": limit}}).to_string()
}

/// The general bucket with one 300-minute window, resetting tomorrow.
fn general_limit(used: i64) -> Value {
    json!({"limit_id": "codex", "primary": {"used_percent": used, "window_minutes": 300, "resets_at": epoch(Duration::days(1))}})
}

fn write_rollout(dir: &Path, lines: &[String]) {
    fs::create_dir_all(dir.join("sessions")).unwrap();
    fs::write(dir.join("sessions").join("rollout.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// The one codex profile `codex-a` with live-quota on; its config dir.
fn codex_ready() -> (Fixture, PathBuf) {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    enable(&f);
    (f, dir)
}

// ---- AC6: the enabling notice ----

#[test]
fn ac6_a_enabling_notice_names_codex_app_server_agy_and_refresh() {
    let f = setup();
    let out = f.cmd().args(["config", "set", "live-quota", "on", "--json"]).assert().success().get_output().stdout.clone();
    let notice = parse(&out)["notice"].as_str().unwrap().to_owned();
    for needle in ["codex", "app-server", "agy", "--refresh"] {
        assert!(notice.contains(needle), "notice lacks {needle:?}: {notice}");
    }
}

// ---- AC8: the integration cases ----

#[test]
fn ac8_a_switch_off_is_refused_with_exit_1_before_launching() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    thoth(&f).args(["profile", "usage", "--refresh"]).assert().code(1);
    assert!(launches(&f).is_empty(), "{:?}", launches(&f));
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac8_b_success_stores_the_reading_and_launches_app_server_once() {
    let (f, dir) = codex_ready();
    let v = refresh_json(&f, |_| {});
    let refresh = entry(&v["refresh"], "codex-a");
    assert_eq!(refresh["status"], "refreshed", "{v}");
    assert_eq!(refresh["error"], Value::Null, "{v}");

    assert_eq!(live_quota_rows(&f), 1);
    let detail: Value = serde_json::from_str(&newest_detail(&f)).unwrap();
    let keys: BTreeSet<&str> = detail.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["source", "windows", "ordinary_usage_allowed"]));
    assert_eq!(detail["source"], "codex_app_server");
    assert_eq!(detail["ordinary_usage_allowed"], true);

    let usage = usage_json(&f);
    assert_eq!(window(&usage, "codex-a", "five_hour")["used_percent"].as_f64(), Some(0.0), "{usage}");
    assert_eq!(window(&usage, "codex-a", "seven_day")["used_percent"].as_f64(), Some(86.0), "{usage}");
    assert_eq!(window(&usage, "codex-a", "five_hour")["source"], "codex_app_server");
    assert_eq!(window(&usage, "codex-a", "seven_day")["source"], "codex_app_server");

    let rows = launches(&f);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["args"], json!(["app-server", "--listen", "stdio://"]));
    // codex runs in the state directory, never in the user's repo.
    assert_eq!(
        fs::canonicalize(rows[0]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(f.home_path()).unwrap()
    );
    assert_eq!(
        rows[0]["codex_home"].as_str().map(|p| fs::canonicalize(p).unwrap()),
        Some(fs::canonicalize(&dir).unwrap()),
        "CODEX_HOME must be the profile's config dir"
    );
}

#[test]
fn ac8_c_error_reply_fails_without_a_reading() {
    let (f, _) = codex_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "error");
    });
    let refresh = entry(&v["refresh"], "codex-a");
    assert_eq!(refresh["status"], "failed", "{v}");
    assert!(refresh["error"].as_str().unwrap().contains("authentication required"), "{v}");
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac8_d_timeout_ends_codex_at_once_and_reports_timed_out() {
    let (f, _) = codex_ready();
    let marker = f.home_path().join("codex-finished.txt");
    let started_file = f.home_path().join("codex-hang-started.txt");
    // The stand-in hangs for HANG_SECS, far past the 2 s timeout: the
    // "ended at once" bound below is the timeout plus 10 s, under half of
    // that, so a thoth that waited for the stand-in to finish on its own
    // would still fail, while the few seconds kill_tree's confirmation and
    // the exit take under load pass.
    const HANG_SECS: u64 = 30;
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "hang")
            .env("FAKE_CODEX_HANG_SECS", HANG_SECS.to_string())
            .env("FAKE_CODEX_HANG_MARKER", &marker)
            .env("FAKE_CODEX_HANG_STARTED", &started_file)
            .env("THOTH_LIVE_QUOTA_TIMEOUT_MS", "2000");
    });
    let returned = std::time::SystemTime::now();
    // Timed from the moment the stand-in received the request, so thoth's
    // and the stand-in's start-up time is not counted.
    assert!(started_file.exists(), "codex never received the rate-limits request");
    let requested = fs::metadata(&started_file).unwrap().modified().unwrap();
    let since_request = returned.duration_since(requested).unwrap_or_default();
    assert!(
        since_request < std::time::Duration::from_secs(12),
        "took {since_request:?} after the request; the bound is the 2s timeout plus 10s, under half \
         the {HANG_SECS}s the stand-in would hang on its own"
    );
    let refresh = entry(&v["refresh"], "codex-a");
    assert_eq!(refresh["status"], "failed", "{v}");
    assert!(refresh["error"].as_str().unwrap().contains("timed out"), "{v}");
    // Past the moment the stand-in would write the marker on its own
    // (HANG_SECS after the request), plus a second.
    if let Ok(wait) = (requested + std::time::Duration::from_secs(HANG_SECS + 1)).duration_since(std::time::SystemTime::now()) {
        std::thread::sleep(wait)
    }
    assert!(!marker.exists(), "codex finished on its own instead of being ended");
}

#[test]
fn ac8_e_profile_with_a_live_run_is_skipped_without_launching() {
    let (f, _) = codex_ready();
    f.state_conn()
        .execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE', 'codex-a', 'running', 'implementer')", [])
        .unwrap();
    let v = refresh_json(&f, |_| {});
    let refresh = entry(&v["refresh"], "codex-a");
    assert_eq!(refresh["status"], "skipped_live_run", "{v}");
    assert_eq!(refresh["error"], Value::Null, "{v}");
    assert!(!launches(&f).iter().any(|row| row.to_string().contains("app-server")), "{:?}", launches(&f));
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac8_e_rule_a_queued_run_also_skips() {
    let (f, _) = codex_ready();
    f.state_conn()
        .execute("INSERT INTO runs (run_id, account, status, role) VALUES ('QUEUED', 'codex-a', 'queued', 'implementer')", [])
        .unwrap();
    let v = refresh_json(&f, |_| {});
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "skipped_live_run", "{v}");
    assert!(launches(&f).is_empty());
}

#[test]
fn ac8_f_the_newer_of_live_and_rollout_wins() {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    // 1. a live reading from two hours ago
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 40.0, json!(true));
    // 2. a rollout from an hour ago, with an extra bucket
    let an_hour_ago = at(Duration::hours(-1));
    write_rollout(&dir, &[
        rollout_line(&an_hour_ago, general_limit(10)),
        rollout_line(&an_hour_ago, json!({"limit_id": "base_model_inference", "limit_name": "gpt-reserve",
            "primary": {"used_percent": 20, "window_minutes": 300, "resets_at": epoch(Duration::days(1))}})),
    ]);
    // 3. the rollout is newer
    let usage = usage_json(&f);
    let windows = entry(&usage["profiles"], "codex-a")["windows"].as_array().unwrap().clone();
    assert!(!windows.is_empty(), "{usage}");
    assert!(windows.iter().all(|w| w["source"] == "codex_rollout"), "{usage}");
    assert_eq!(window(&usage, "codex-a", "five_hour")["used_percent"].as_f64(), Some(10.0));
    // 4. a live reading from now
    insert_codex_reading(&f, "codex-a", &at(Duration::ZERO), 70.0, json!(true));
    // 5. the live reading is newer; the extra bucket still comes from the rollout
    let usage = usage_json(&f);
    let profile = entry(&usage["profiles"], "codex-a");
    let windows = profile["windows"].as_array().unwrap();
    assert!(!windows.is_empty(), "{usage}");
    assert!(windows.iter().all(|w| w["source"] == "codex_app_server"), "{usage}");
    assert_eq!(window(&usage, "codex-a", "five_hour")["used_percent"].as_f64(), Some(70.0));
    let extras = profile["extra_buckets"].as_array().unwrap();
    assert_eq!(extras.len(), 1, "{usage}");
    assert_eq!(extras[0]["limit_id"], "base_model_inference");
}

#[test]
fn ac8_g_bad_rows_are_skipped() {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    // 1.
    write_rollout(&dir, &[rollout_line(&at(Duration::hours(-3)), general_limit(10))]);
    // 2.
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 40.0, json!(true));
    // 3.
    insert_detail(&f, "codex-a", &at(Duration::hours(-1)), "not json");
    // 4.
    insert_detail(&f, "codex-a", &at(Duration::ZERO),
        &json!({"source": "codex_app_server", "windows": [{"name": 7}]}).to_string());
    // 5.
    let usage = parse(&thoth(&f).args(["profile", "usage", "--json"]).assert().code(0).get_output().stdout);
    let windows = entry(&usage["profiles"], "codex-a")["windows"].as_array().unwrap().clone();
    assert!(windows.iter().all(|w| w["source"] == "codex_app_server"), "{usage}");
    assert_eq!(window(&usage, "codex-a", "five_hour")["used_percent"].as_f64(), Some(40.0));
}

#[test]
fn ac8_g_rule_ordinary_usage_allowed_of_another_type_makes_the_row_invalid() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 40.0, json!(true));
    insert_codex_reading(&f, "codex-a", &at(Duration::ZERO), 60.0, json!("no"));
    // A row with no windows is valid only when usage is not allowed.
    insert_detail(&f, "codex-a", &at(Duration::ZERO),
        &json!({"source": "codex_app_server", "windows": [], "ordinary_usage_allowed": true}).to_string());
    let usage = usage_json(&f);
    assert_eq!(window(&usage, "codex-a", "five_hour")["used_percent"].as_f64(), Some(40.0), "{usage}");
}

#[test]
fn ac8_h_not_allowed_is_exhausted_until_a_newer_reading_says_otherwise() {
    let (f, _) = codex_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "denied");
    });
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    assert_eq!(entry(&usage_json(&f)["profiles"], "codex-a")["quota_state"], "exhausted");
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert_eq!(candidate["quota_state"], "exhausted", "{candidate}");
    assert_eq!(candidate["eligible"], false, "{candidate}");
    assert!(has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");

    // 1. the default answer: ordinaryUsageAllowed true
    let v = refresh_json(&f, |_| {});
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    // 2.
    assert_eq!(entry(&usage_json(&f)["profiles"], "codex-a")["quota_state"], "ok");
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert_eq!(candidate["quota_state"], "ok", "{candidate}");
    assert!(!has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");
    // 3.
    insert_codex_reading(&f, "codex-a", &at(Duration::ZERO), 10.0, json!(false));
    let recommend = recommend_json(&f);
    assert_eq!(entry(&recommend["candidates"], "codex-a")["quota_state"], "exhausted", "{recommend}");
    // 4.
    insert_codex_reading(&f, "codex-a", &at(Duration::ZERO), 10.0, Value::Null);
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert_eq!(candidate["quota_state"], "ok", "{candidate}");
    assert!(!has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");
}

#[test]
fn ac8_i_login_file_never_leaks() {
    let (f, dir) = codex_ready();
    const SENTINEL: &str = "SENTINEL-auth-7f3a9c";
    fs::write(dir.join("auth.json"), format!("{{\"token\":\"{SENTINEL}\"}}")).unwrap();
    let out = thoth(&f).args(["profile", "usage", "--refresh", "--json"]).assert().code(0).get_output().clone();
    let v = parse(&out.stdout);
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains(SENTINEL));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(SENTINEL));
    assert!(!newest_detail(&f).contains(SENTINEL));
}

#[test]
fn ac8_j_agy_and_codex_are_both_refreshed_in_id_order() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    enable(&f);
    let v = refresh_json(&f, |_| {});
    let refresh: Vec<(String, String)> = v["refresh"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["profile_id"].as_str().unwrap().into(), r["status"].as_str().unwrap().into()))
        .collect();
    assert_eq!(refresh, [
        ("agy-a".to_string(), "refreshed".to_string()),
        ("codex-a".to_string(), "refreshed".to_string()),
    ]);
}

#[test]
fn ac8_k_missing_binary_names_thoth_codex_bin() {
    let (f, _) = codex_ready();
    let missing = f.home_path().join("no-such-dir").join("codex");
    let v = refresh_json(&f, |c| {
        c.env("THOTH_CODEX_BIN", &missing);
    });
    let refresh = entry(&v["refresh"], "codex-a");
    assert_eq!(refresh["status"], "failed", "{v}");
    assert!(refresh["error"].as_str().unwrap().contains("THOTH_CODEX_BIN"), "{v}");
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac8_l_exit_before_answering_fails_without_a_reading() {
    let (f, _) = codex_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "exit");
    });
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "failed", "{v}");
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac8_m_not_allowed_with_no_windows_is_stored_and_exhausted() {
    let (f, _) = codex_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CODEX_APP_SERVER", "denied_empty");
    });
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    let detail: Value = serde_json::from_str(&newest_detail(&f)).unwrap();
    assert_eq!(detail["windows"], json!([]));
    assert_eq!(detail["ordinary_usage_allowed"], false);
    let usage = usage_json(&f);
    let profile = entry(&usage["profiles"], "codex-a");
    assert_eq!(profile["windows"], json!([]), "{usage}");
    assert_eq!(profile["quota_state"], "exhausted", "{usage}");
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert!(has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");
}

#[test]
fn ac8_n_not_allowed_older_than_24_hours_does_not_count() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-25)), 10.0, json!(false));
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert_eq!(candidate["quota_state"], "unknown", "{candidate}");
    assert!(!has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");
}

#[test]
fn ac8_o_no_reading_points_to_refresh() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert!(has_reason(candidate, &["no quota reading yet", "thoth profile usage --refresh"]), "{candidate}");
}

#[test]
fn ac8_p_a_newer_rollout_ends_not_allowed() {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    // 1.
    insert_codex_reading(&f, "codex-a", &at(Duration::hours(-2)), 10.0, json!(false));
    // 2.
    let recommend = recommend_json(&f);
    assert_eq!(entry(&recommend["candidates"], "codex-a")["quota_state"], "exhausted", "{recommend}");
    // 3.
    write_rollout(&dir, &[rollout_line(&at(Duration::hours(-1)), general_limit(20))]);
    // 4.
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "codex-a");
    assert_eq!(candidate["quota_state"], "ok", "{candidate}");
    assert!(!has_reason(candidate, &["ordinary usage not allowed"]), "{candidate}");
    // 5.
    assert_eq!(entry(&usage_json(&f)["profiles"], "codex-a")["quota_state"], "ok");
}

#[test]
fn ac5_rule_profile_usage_and_recommend_agree_on_not_allowed() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    insert_codex_reading(&f, "codex-a", &at(Duration::minutes(-1)), 10.0, json!(false));
    let usage = usage_json(&f);
    let recommend = recommend_json(&f);
    assert_eq!(
        entry(&usage["profiles"], "codex-a")["quota_state"],
        entry(&recommend["candidates"], "codex-a")["quota_state"]
    );
    assert_eq!(entry(&usage["profiles"], "codex-a")["quota_state"], "exhausted");
}

#[test]
fn ac1_rule_human_output_has_one_refresh_line_for_codex() {
    let (f, _) = codex_ready();
    let out = thoth(&f).args(["profile", "usage", "--refresh"]).assert().code(0).get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.trim_end().lines().last(), Some("refresh codex-a: refreshed"), "{text}");
}
