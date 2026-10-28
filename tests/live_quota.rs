//! AG2: `profile usage --refresh` against fake-agy, the stored `live_quota`
//! readings, quota decided per model group, and `recommend --model`.
//! Each test is named after the acceptance criterion and case it covers
//! (`ac4_c_...` is AC4 case (c)); `..._rule_...` tests cover a rule of that
//! criterion that has no lettered case.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const GEMINI: &str = "Gemini Models";
const GEMINI_DESCRIPTION: &str = "Models within this group: Gemini Flash, Gemini Pro";
const OTHER: &str = "Claude and GPT models";
const OTHER_DESCRIPTION: &str = "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS";

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("valid json")
}

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta).format(&Rfc3339).unwrap()
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

/// Where fake-agy appends one JSON line per launch.
fn args_log(f: &Fixture) -> PathBuf {
    f.home_path().join("agy-args.log")
}

/// A `thoth` command whose agy launches (if any) are recorded in `args_log`.
fn thoth(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_AGY_ARGS_LOG", args_log(f));
    c
}

/// The agy launches fake-agy recorded; none when it never ran.
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

/// `profile usage [extra] --json`, without `--refresh`.
fn usage_json(f: &Fixture, extra: &[&str]) -> Value {
    parse(&thoth(f).args(["profile", "usage"]).args(extra).arg("--json").assert().code(0).get_output().stdout)
}

/// `recommend --role implementer [extra] --json`.
fn recommend_json(f: &Fixture, extra: &[&str]) -> Value {
    parse(&thoth(f).args(["recommend", "--role", "implementer"]).args(extra).arg("--json").assert().code(0).get_output().stdout)
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

/// The (ts, detail) of the newest `live_quota` row.
fn newest_row(f: &Fixture) -> (String, String) {
    f.global_conn()
        .query_row(
            "SELECT ts, detail FROM quota_events WHERE kind='live_quota' ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// One stored agy window, in the `live_quota` detail shape.
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

/// AC5 (a)'s reading, observed a minute ago: the Gemini group at 25% and the
/// Claude and GPT group at 98%, both resetting tomorrow.
fn insert_gemini_25_other_98(f: &Fixture, profile: &str) {
    insert_reading(f, profile, &at(Duration::minutes(-1)), vec![
        stored_window("gemini-weekly", 25.0, Some(at(Duration::days(1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 98.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
}

/// A claude-code profile with one 51% `rate_limit_warning` from a minute ago,
/// written the way tests/usage_recommend.rs writes them.
fn add_claude_at_51(f: &Fixture, id: &str) {
    add_profile(f, id, "claude-code");
    let conn = f.state_conn();
    let run = format!("RUN-{id}");
    conn.execute(
        "INSERT INTO runs (run_id, account, status, role) VALUES (?1, ?2, 'completed', 'implementer')",
        params![run, id],
    )
    .unwrap();
    let resets_at = (OffsetDateTime::now_utc() + Duration::hours(2)).unix_timestamp();
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, 'rate_limit_warning', ?3)",
        params![
            at(Duration::minutes(-1)),
            run,
            json!({"rate_limit_type": "five_hour", "utilization": 0.51, "resets_at": resets_at}).to_string()
        ],
    )
    .unwrap();
}

/// A `/usage` answer shaped like fake-agy's built-in one, with the two
/// remaining fractions chosen by the test.
fn usage_answer(gemini_remaining: f64, other_remaining: f64) -> String {
    json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage", "data": {"groups": [
        {"name": GEMINI, "description": GEMINI_DESCRIPTION, "buckets": [{"id": "gemini-weekly",
            "window": "weekly", "remaining_fraction": gemini_remaining, "reset_time": "2099-01-01T00:00:00Z"}]},
        {"name": OTHER, "description": OTHER_DESCRIPTION, "buckets": [{"id": "3p-weekly",
            "window": "weekly", "remaining_fraction": other_remaining, "reset_time": "2099-01-01T00:00:00Z"}]}
    ]}}})
    .to_string()
}

/// A file for `FAKE_AGY_USAGE_FILE`.
fn answer_file(f: &Fixture, text: &str) -> PathBuf {
    let path = f.home_path().join("answer.txt");
    fs::write(&path, text).unwrap();
    path
}

/// The one agy profile `agy-a` with live-quota on.
fn agy_ready() -> Fixture {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    enable(&f);
    f
}

/// Asserts agy-a's refresh failed with every needle in its error, that the
/// report still shows no reading for it, and that nothing was stored.
fn assert_failed_without_reading(f: &Fixture, v: &Value, needles: &[&str]) {
    let refresh = entry(&v["refresh"], "agy-a");
    assert_eq!(refresh["status"], "failed", "{v}");
    let error = refresh["error"].as_str().unwrap();
    for needle in needles {
        assert!(error.contains(needle), "error {error:?} lacks {needle:?}");
    }
    assert_eq!(entry(&v["profiles"], "agy-a")["windows"], json!([]), "{v}");
    assert_eq!(live_quota_rows(f), 0);
}

/// Runs fake-agy directly with `/usage`'s four arguments.
fn run_fake_usage(env: &[(&str, &std::ffi::OsStr)]) -> std::process::Output {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_fake-agy"));
    c.args(["-p", "/usage", "--output-format", "json"]);
    for (key, value) in env {
        c.env(key, value);
    }
    c.output().unwrap()
}

// ---- AC1: the switch and the command surface ----

#[test]
fn ac1_a_run_id_is_refused_with_exit_1_before_launching() {
    let f = agy_ready();
    thoth(&f)
        .env("THOTH_RUN_ID", "01RUN")
        .args(["profile", "usage", "--refresh"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("THOTH_RUN_ID"));
    assert!(launches(&f).is_empty());
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac1_b_switch_off_without_config_is_refused_with_exit_1_before_launching() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    assert!(!f.home_path().join("config.json").exists());
    thoth(&f)
        .args(["profile", "usage", "--refresh"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("thoth config set live-quota on"))
        .stderr(predicates::str::contains("by the user directly"));
    assert!(launches(&f).is_empty());
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac1_b_rule_invalid_config_json_counts_as_off() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    fs::write(f.home_path().join("config.json"), "{not json").unwrap();
    thoth(&f)
        .args(["profile", "usage", "--refresh", "--json"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("thoth config set live-quota on"));
    assert!(launches(&f).is_empty());
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac1_c_refresh_reports_every_profile_in_id_order() {
    let f = setup();
    add_profile(&f, "codex-a", "codex");
    add_profile(&f, "agy-a", "antigravity");
    add_profile(&f, "claude-a", "claude-code");
    enable(&f);
    let v = refresh_json(&f, |_| {});
    let refresh: Vec<(String, String, Value)> = v["refresh"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["profile_id"].as_str().unwrap().into(), r["status"].as_str().unwrap().into(), r["error"].clone()))
        .collect();
    assert_eq!(refresh, [
        ("agy-a".to_string(), "refreshed".to_string(), Value::Null),
        ("claude-a".to_string(), "refreshed".to_string(), Value::Null),
        ("codex-a".to_string(), "refreshed".to_string(), Value::Null),
    ]);
    assert_eq!(launches(&f).len(), 1);
}

#[test]
fn ac1_d_without_refresh_there_is_no_refresh_key_and_no_launch() {
    let f = agy_ready();
    let v = usage_json(&f, &[]);
    assert!(v.get("refresh").is_none(), "{v}");
    let plain = thoth(&f).args(["profile", "usage"]).assert().code(0).get_output().stdout.clone();
    assert!(!String::from_utf8(plain).unwrap().contains("refresh "));
    assert!(launches(&f).is_empty());
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn ac1_e_enabling_notice_names_agy_and_refresh() {
    let f = setup();
    let out = f.cmd().args(["config", "set", "live-quota", "on", "--json"]).assert().success().get_output().stdout.clone();
    let notice = parse(&out)["notice"].as_str().unwrap().to_owned();
    for needle in ["agy", "--refresh", "official", "thoth config set live-quota off"] {
        assert!(notice.contains(needle), "notice lacks {needle:?}: {notice}");
    }
}

#[test]
fn ac1_rule_human_output_adds_one_refresh_line_per_profile_after_the_report() {
    let f = agy_ready();
    add_claude_at_51(&f, "claude-a");
    let missing = f.home_path().join("no-such-dir").join("agy");
    let out = thoth(&f)
        .env("THOTH_AGY_BIN", &missing)
        .args(["profile", "usage", "--refresh"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let n = lines.len();
    assert!(lines[n - 2].starts_with("refresh agy-a: failed: could not run agy"), "{text}");
    assert_eq!(lines[n - 1], "refresh claude-a: refreshed", "{text}");
    assert!(lines[..n - 2].iter().any(|l| l.starts_with("claude-a ")), "{text}");
}

// ---- AC2: asking agy, and every way it can fail ----

#[test]
fn ac2_success_runs_agy_with_exactly_the_four_arguments() {
    let f = agy_ready();
    let v = refresh_json(&f, |_| {});
    assert_eq!(entry(&v["refresh"], "agy-a")["status"], "refreshed", "{v}");
    let rows = launches(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["args"], json!(["-p", "/usage", "--output-format", "json"]));
    assert!(rows[0]["env"]["CLAUDE_CONFIG_DIR"].is_null(), "{}", rows[0]);
    assert!(rows[0]["env"]["CODEX_HOME"].is_null(), "{}", rows[0]);
    // agy runs in the state directory, never in the user's repo.
    assert_eq!(
        fs::canonicalize(rows[0]["cwd"].as_str().unwrap()).unwrap(),
        fs::canonicalize(f.home_path()).unwrap()
    );
}

#[test]
fn ac2_1_missing_binary_reports_could_not_run_agy() {
    let f = agy_ready();
    let missing = f.home_path().join("no-such-dir").join("agy");
    let v = refresh_json(&f, |c| {
        c.env("THOTH_AGY_BIN", &missing);
    });
    assert_failed_without_reading(&f, &v, &["could not run agy", "THOTH_AGY_BIN"]);
}

#[test]
fn ac2_2_timeout_ends_agy_and_reports_timed_out() {
    let f = agy_ready();
    let marker = f.home_path().join("agy-finished.txt");
    // Measured from just before agy can start (stricter than from the start
    // of the test): fake-agy writes the marker 6 s after it starts.
    let started = Instant::now();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE", "hang")
            .env("FAKE_AGY_HANG_MARKER", &marker)
            .env("THOTH_LIVE_QUOTA_TIMEOUT_MS", "1000");
    });
    assert_failed_without_reading(&f, &v, &["timed out"]);
    std::thread::sleep(std::time::Duration::from_secs(9).saturating_sub(started.elapsed()));
    assert!(!marker.exists(), "agy finished on its own instead of being ended");
}

#[test]
fn ac2_3_signed_out_reports_not_signed_in() {
    let f = agy_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_LOGGED_OUT", "1");
    });
    assert_failed_without_reading(&f, &v, &["not signed in"]);
}

#[test]
fn ac2_4_nonzero_exit_reports_the_code_and_stderr() {
    let f = agy_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE", "fail");
    });
    assert_failed_without_reading(&f, &v, &["exited with code 3", "boom"]);
}

#[test]
fn ac2_4_rule_stderr_is_cut_to_its_first_200_characters() {
    let f = agy_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE", "fail_long");
    });
    let error = entry(&v["refresh"], "agy-a")["error"].as_str().unwrap().to_owned();
    assert!(error.contains("exited with code 3"), "{error}");
    assert!(error.contains(&"a".repeat(200)), "{error}");
    assert!(!error.contains("BEYOND_200_CHARS"), "{error}");
    assert!(!error.contains(&"a".repeat(201)), "{error}");
}

#[test]
fn ac2_5_output_that_is_not_json_reports_not_json() {
    let f = agy_ready();
    let file = answer_file(&f, "not json");
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_failed_without_reading(&f, &v, &["not JSON"]);
}

#[test]
fn ac2_6_error_status_reports_reported_status_error() {
    let f = agy_ready();
    let file = answer_file(&f, &json!({"status": "ERROR"}).to_string());
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_failed_without_reading(&f, &v, &["reported status", "ERROR"]);
}

#[test]
fn ac2_7_answer_without_usage_command_reports_did_not_answer() {
    let f = agy_ready();
    let file = answer_file(&f, &json!({"status": "SUCCESS", "num_turns": 1}).to_string());
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_failed_without_reading(&f, &v, &["did not answer /usage", "1.1.11"]);
}

#[test]
fn ac2_8_empty_groups_report_no_quota_buckets() {
    let f = agy_ready();
    let body = json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": []}}});
    let file = answer_file(&f, &body.to_string());
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_failed_without_reading(&f, &v, &["no quota buckets"]);
}

#[test]
fn ac2_mixed_agy_failure_leaves_the_claude_reading_intact() {
    let f = agy_ready();
    add_claude_at_51(&f, "claude-a");
    let missing = f.home_path().join("no-such-dir").join("agy");
    let v = refresh_json(&f, |c| {
        c.env("THOTH_AGY_BIN", &missing);
        c.env("FAKE_CLAUDE_USAGE", "exit");
    });
    assert_eq!(entry(&v["refresh"], "agy-a")["status"], "failed", "{v}");
    assert_eq!(entry(&v["refresh"], "claude-a")["status"], "failed", "{v}");
    assert_eq!(entry(&v["profiles"], "claude-a")["windows"][0]["used_percent"], 51.0, "{v}");
}

#[test]
fn ac2_failed_refresh_keeps_the_previous_reading() {
    let f = agy_ready();
    let first = refresh_json(&f, |_| {});
    let before = entry(&first["profiles"], "agy-a")["windows"].clone();
    assert_eq!(before.as_array().unwrap().len(), 2);
    assert_eq!(window(&first, "agy-a", "gemini-weekly")["used_percent"], 25.0);
    let file = answer_file(&f, "not json");
    let second = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_eq!(entry(&second["refresh"], "agy-a")["status"], "failed", "{second}");
    let after = &entry(&second["profiles"], "agy-a")["windows"];
    assert_eq!(after, &before);
    assert_eq!(after[0]["observed_at"], before[0]["observed_at"]);
    assert_eq!(live_quota_rows(&f), 1);
}

#[test]
fn ac2_rule_profile_with_a_live_run_is_still_queried() {
    let f = agy_ready();
    f.state_conn()
        .execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE', 'agy-a', 'running', 'implementer')", [])
        .unwrap();
    let v = refresh_json(&f, |_| {});
    assert_eq!(entry(&v["refresh"], "agy-a")["status"], "refreshed", "{v}");
    assert_eq!(entry(&v["profiles"], "agy-a")["live_runs"], 1, "{v}");
    assert_eq!(launches(&f).len(), 1);
}

// ---- AC4: storing a reading and reading it back ----

#[test]
fn ac4_a_refresh_stores_one_row_with_only_source_and_windows() {
    let f = agy_ready();
    refresh_json(&f, |_| {});
    assert_eq!(live_quota_rows(&f), 1);
    let (ts, detail) = newest_row(&f);
    assert!(OffsetDateTime::parse(&ts, &Rfc3339).unwrap().offset().is_utc(), "{ts}");
    let detail: Value = serde_json::from_str(&detail).unwrap();
    let keys: BTreeSet<&str> = detail.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["source", "windows"]));
    assert_eq!(detail["source"], "agy_usage");
    assert_eq!(detail["windows"], json!([
        {"name": "gemini-weekly", "used_percent": 25.0, "resets_at": "2099-01-01T00:00:00Z",
            "group": GEMINI, "group_description": GEMINI_DESCRIPTION},
        {"name": "3p-weekly", "used_percent": 0.0, "resets_at": "2099-01-01T00:00:00Z",
            "group": OTHER, "group_description": OTHER_DESCRIPTION},
    ]));
    let v = usage_json(&f, &["agy-a"]);
    let windows = v["profiles"][0]["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2, "{v}");
    for w in windows {
        assert_eq!(w["source"], "agy_usage");
        assert_eq!(w["observed_at"], ts.as_str());
        assert_eq!(w["reset_since_observed"], false);
    }
    assert_eq!(windows[0]["group"], GEMINI);
}

#[test]
fn ac4_b_plain_usage_and_recommend_only_read_the_stored_reading() {
    let f = agy_ready();
    let first = refresh_json(&f, |_| {});
    let windows = entry(&first["profiles"], "agy-a")["windows"].clone();
    let usage = usage_json(&f, &[]);
    assert_eq!(entry(&usage["profiles"], "agy-a")["windows"], windows);
    let recommend = recommend_json(&f, &[]);
    let candidate = entry(&recommend["candidates"], "agy-a");
    assert_eq!(candidate["newest_observed_at"], windows[0]["observed_at"]);
    assert!(has_reason(candidate, &["gemini-weekly", GEMINI, "25% used"]), "{candidate}");
    assert!(has_reason(candidate, &["3p-weekly", OTHER, "0% used"]), "{candidate}");
    assert_eq!(launches(&f).len(), 1);
    assert_eq!(live_quota_rows(&f), 1);
}

#[test]
fn ac4_c_second_refresh_adds_a_row() {
    let f = agy_ready();
    refresh_json(&f, |_| {});
    let file = answer_file(&f, &usage_answer(0.6, 1.0));
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE_FILE", &file);
    });
    assert_eq!(entry(&v["refresh"], "agy-a")["status"], "refreshed", "{v}");
    assert_eq!(live_quota_rows(&f), 2);
    assert_eq!(window(&v, "agy-a", "gemini-weekly")["used_percent"], 40.0);
}

#[test]
fn ac4_d_newer_row_that_is_not_json_is_skipped() {
    let f = agy_ready();
    let first = refresh_json(&f, |_| {});
    let windows = entry(&first["profiles"], "agy-a")["windows"].clone();
    insert_detail(&f, "agy-a", &at(Duration::minutes(1)), "not json");
    let v = usage_json(&f, &[]);
    assert_eq!(entry(&v["profiles"], "agy-a")["windows"], windows);
}

#[test]
fn ac4_rule_newer_row_with_the_wrong_shape_is_skipped() {
    let f = agy_ready();
    let first = refresh_json(&f, |_| {});
    let windows = entry(&first["profiles"], "agy-a")["windows"].clone();
    insert_detail(&f, "agy-a", &at(Duration::minutes(1)),
        &json!({"source": "agy_usage", "windows": [{"name": 7}]}).to_string());
    let v = usage_json(&f, &[]);
    assert_eq!(entry(&v["profiles"], "agy-a")["windows"], windows);
}

#[test]
fn ac4_e_reading_older_than_24_hours_is_unknown() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_reading(&f, "agy-a", &at(Duration::hours(-25)), vec![
        stored_window("gemini-weekly", 25.0, Some(at(Duration::days(1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 0.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let v = usage_json(&f, &[]);
    let profile = entry(&v["profiles"], "agy-a");
    assert_eq!(profile["quota_state"], "unknown", "{v}");
    assert_eq!(profile["peak_percent"], Value::Null);
    assert_eq!(profile["windows"].as_array().unwrap().len(), 2);
}

#[test]
fn ac4_f_reset_time_in_the_past_sets_reset_since_observed() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_reading(&f, "agy-a", &at(Duration::hours(-2)), vec![
        stored_window("gemini-weekly", 50.0, Some(at(Duration::hours(-1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 10.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let v = usage_json(&f, &[]);
    assert_eq!(window(&v, "agy-a", "gemini-weekly")["reset_since_observed"], true);
    assert_eq!(window(&v, "agy-a", "3p-weekly")["reset_since_observed"], false);
}

#[test]
fn ac4_g_human_output_appends_the_group() {
    let f = agy_ready();
    add_claude_at_51(&f, "claude-a");
    refresh_json(&f, |_| {});
    let out = thoth(&f).args(["profile", "usage"]).assert().code(0).get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    let gemini = text.lines().find(|l| l.trim_start().starts_with("gemini-weekly:")).expect(&text);
    assert!(gemini.ends_with(" group=Gemini Models"), "{gemini}");
    let claude = text.lines().find(|l| l.trim_start().starts_with("five_hour:")).expect(&text);
    assert!(!claude.contains("group="), "{claude}");
}

#[test]
fn ac4_rule_codex_windows_and_extra_buckets_have_no_group() {
    let f = setup();
    let dir = add_profile(&f, "codex-a", "codex");
    fs::create_dir_all(dir.join("sessions")).unwrap();
    let resets_at = (OffsetDateTime::now_utc() + Duration::hours(2)).unix_timestamp();
    let line = |limit: Value| {
        json!({"timestamp": at(Duration::minutes(-1)), "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": limit}}).to_string()
    };
    fs::write(dir.join("sessions").join("rollout.jsonl"), format!("{}\n{}\n",
        line(json!({"limit_id": "codex", "primary": {"used_percent": 10, "window_minutes": 300, "resets_at": resets_at}})),
        line(json!({"limit_id": "base_model_inference", "limit_name": "gpt-reserve",
            "primary": {"used_percent": 20, "window_minutes": 300, "resets_at": resets_at}})),
    )).unwrap();
    let v = usage_json(&f, &[]);
    let profile = entry(&v["profiles"], "codex-a");
    assert_eq!(profile["windows"][0]["group"], Value::Null, "{v}");
    assert_eq!(profile["extra_buckets"][0]["windows"][0]["group"], Value::Null, "{v}");
    assert_eq!(profile["quota_group"], Value::Null, "{v}");
}

// ---- AC5: quota decided per model group ----

#[test]
fn ac5_a_without_a_model_the_best_group_decides() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    let recommend = recommend_json(&f, &[]);
    assert_eq!(recommend["model"], Value::Null);
    let candidate = entry(&recommend["candidates"], "agy-a");
    assert_eq!(candidate["eligible"], true, "{candidate}");
    assert_eq!(candidate["quota_state"], "ok");
    assert_eq!(candidate["peak_percent"], 25.0);
    assert_eq!(candidate["quota_group"], GEMINI);
    assert!(has_reason(candidate, &["best of 2 groups", "--model"]), "{candidate}");
    assert!(has_reason(candidate, &["gemini-weekly", GEMINI]), "{candidate}");
    assert!(has_reason(candidate, &["3p-weekly", OTHER]), "{candidate}");
    let usage = usage_json(&f, &[]);
    let profile = entry(&usage["profiles"], "agy-a");
    for key in ["quota_state", "peak_percent", "quota_group"] {
        assert_eq!(profile[key], candidate[key], "{key}");
    }
}

#[test]
fn ac5_b_every_group_exhausted_makes_the_profile_exhausted() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_reading(&f, "agy-a", &at(Duration::minutes(-1)), vec![
        stored_window("gemini-weekly", 98.0, Some(at(Duration::days(1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 98.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let candidate = entry(&recommend_json(&f, &[])["candidates"], "agy-a").clone();
    assert_eq!(candidate["eligible"], false, "{candidate}");
    assert_eq!(candidate["quota_state"], "exhausted");
    // When every group is exhausted the first one is reported.
    assert_eq!(candidate["quota_group"], GEMINI);
}

#[test]
fn ac5_c_a_reset_group_counts_as_zero() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_reading(&f, "agy-a", &at(Duration::hours(-2)), vec![
        stored_window("gemini-weekly", 98.0, Some(at(Duration::hours(-1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 40.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let candidate = entry(&recommend_json(&f, &[])["candidates"], "agy-a").clone();
    assert_eq!(candidate["quota_group"], GEMINI, "{candidate}");
    assert_eq!(candidate["peak_percent"], 0.0);
    assert_eq!(candidate["eligible"], true);
}

#[test]
fn ac5_d_no_reading_points_to_refresh() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    let candidate = entry(&recommend_json(&f, &[])["candidates"], "agy-a").clone();
    assert_eq!(candidate["quota_state"], "unknown", "{candidate}");
    assert!(has_reason(&candidate, &["no quota reading yet", "thoth profile usage --refresh"]), "{candidate}");
}

#[test]
fn ac5_e_an_ungrouped_claude_profile_is_unchanged() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    add_claude_at_51(&f, "claude-a");
    let candidate = entry(&recommend_json(&f, &[])["candidates"], "claude-a").clone();
    assert_eq!(candidate["quota_group"], Value::Null, "{candidate}");
    assert_eq!(candidate["quota_state"], "ok");
    assert_eq!(candidate["peak_percent"], 51.0);
    assert!(!has_reason(&candidate, &["best of"]), "{candidate}");
    let usage = usage_json(&f, &[]);
    let profile = entry(&usage["profiles"], "claude-a");
    assert_eq!(profile["quota_group"], Value::Null, "{profile}");
    assert_eq!(profile["windows"][0]["group"], Value::Null);
    assert_eq!(profile["quota_state"], "ok");
    assert_eq!(profile["peak_percent"], 51.0);
}

#[test]
fn ac5_rule_a_tie_goes_to_the_group_agy_listed_first() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    // Gemini is listed first although "Claude and GPT models" sorts first.
    insert_reading(&f, "agy-a", &at(Duration::minutes(-1)), vec![
        stored_window("gemini-weekly", 30.0, Some(at(Duration::days(1))), GEMINI, GEMINI_DESCRIPTION),
        stored_window("3p-weekly", 30.0, Some(at(Duration::days(1))), OTHER, OTHER_DESCRIPTION),
    ]);
    let candidate = entry(&recommend_json(&f, &[])["candidates"], "agy-a").clone();
    assert_eq!(candidate["quota_group"], GEMINI, "{candidate}");
}

// ---- AC6: recommend --model ----

#[test]
fn ac6_a_model_in_the_exhausted_group_is_ineligible() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    let recommend = recommend_json(&f, &["--model", "claude-sonnet-4-6"]);
    let candidate = entry(&recommend["candidates"], "agy-a");
    assert_eq!(candidate["eligible"], false, "{candidate}");
    assert_eq!(candidate["quota_state"], "exhausted");
    assert_eq!(candidate["quota_group"], OTHER);
    assert!(has_reason(candidate, &["for model claude-sonnet-4-6"]), "{candidate}");
}

#[test]
fn ac6_b_model_in_the_open_group_uses_that_group() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    let recommend = recommend_json(&f, &["--model", "gemini-3.1-pro-high"]);
    assert_eq!(recommend["model"], "gemini-3.1-pro-high");
    let candidate = entry(&recommend["candidates"], "agy-a");
    assert_eq!(candidate["eligible"], true, "{candidate}");
    assert_eq!(candidate["peak_percent"], 25.0);
    assert_eq!(candidate["quota_group"], GEMINI);
    assert!(has_reason(candidate, &["for model gemini-3.1-pro-high"]), "{candidate}");
}

#[test]
fn ac6_c_unmatched_model_falls_back_to_the_best_group() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    let without = entry(&recommend_json(&f, &[])["candidates"], "agy-a").clone();
    let recommend = recommend_json(&f, &["--model", "no-such-model"]);
    let candidate = entry(&recommend["candidates"], "agy-a");
    for key in ["eligible", "quota_state", "peak_percent", "quota_group"] {
        assert_eq!(candidate[key], without[key], "{key}");
    }
    assert_eq!(candidate["quota_group"], GEMINI);
    assert!(has_reason(candidate, &["matches no group"]), "{candidate}");
}

#[test]
fn ac6_d_model_does_not_change_an_ungrouped_candidate() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    insert_gemini_25_other_98(&f, "agy-a");
    add_claude_at_51(&f, "claude-a");
    let without = entry(&recommend_json(&f, &[])["candidates"], "claude-a").clone();
    let with = entry(&recommend_json(&f, &["--model", "sonnet"])["candidates"], "claude-a").clone();
    assert_eq!(with, without);
}

#[test]
fn ac6_rule_model_matching_two_groups_uses_the_best_group() {
    let f = setup();
    add_profile(&f, "agy-a", "antigravity");
    let description = "Models within this group: Gemini Flash";
    insert_reading(&f, "agy-a", &at(Duration::minutes(-1)), vec![
        stored_window("first-weekly", 60.0, Some(at(Duration::days(1))), "First", description),
        stored_window("second-weekly", 20.0, Some(at(Duration::days(1))), "Second", description),
    ]);
    let recommend = recommend_json(&f, &["--model", "gemini-3.8-flash-low"]);
    let candidate = entry(&recommend["candidates"], "agy-a");
    assert_eq!(candidate["quota_group"], "Second", "{candidate}");
    assert!(has_reason(candidate, &["matches more than one group"]), "{candidate}");
}

// ---- AC7: fake-agy's /usage mode, run directly ----

#[test]
fn ac7_default_usage_answer_has_two_groups() {
    let out = run_fake_usage(&[]);
    assert_eq!(out.status.code(), Some(0));
    let v = parse(&out.stdout);
    assert!(v.is_object());
    assert_eq!(v["status"], "SUCCESS");
    assert_eq!(v["num_turns"], 0);
    assert_eq!(v["command"]["name"], "usage");
    let groups = v["command"]["data"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["buckets"][0]["remaining_fraction"], 0.75);
}

#[test]
fn ac7_fail_mode_exits_3_with_boom() {
    let out = run_fake_usage(&[("FAKE_AGY_USAGE", "fail".as_ref())]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
}

#[test]
fn ac7_usage_file_is_printed_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("answer.txt");
    fs::write(&file, "xyz").unwrap();
    let out = run_fake_usage(&[("FAKE_AGY_USAGE_FILE", file.as_os_str())]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "xyz");
}

#[test]
fn ac7_rule_signed_out_mode_prints_please_sign_in_and_exits_0() {
    let out = run_fake_usage(&[("FAKE_AGY_LOGGED_OUT", "1".as_ref())]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Please sign in"));
}

// ---- AC9 (2): agy's pipes are read while Thoth waits ----

#[test]
fn ac9_2_fake_noisy_mode_prints_200_kb_on_stderr_then_the_json() {
    let out = run_fake_usage(&[("FAKE_AGY_USAGE", "noisy".as_ref())]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.len() >= 200 * 1024, "{} bytes", out.stderr.len());
    assert!(out.stderr.is_ascii());
    assert_eq!(parse(&out.stdout)["command"]["data"]["groups"].as_array().unwrap().len(), 2);
}

#[test]
fn ac9_2_noisy_stderr_does_not_block_the_refresh() {
    let f = agy_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_AGY_USAGE", "noisy");
    });
    assert_eq!(entry(&v["refresh"], "agy-a")["status"], "refreshed", "{v}");
    assert_eq!(entry(&v["profiles"], "agy-a")["windows"].as_array().unwrap().len(), 2, "{v}");
}
