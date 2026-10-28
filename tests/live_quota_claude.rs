//! E7: `profile usage --refresh` against fake-claude's `/usage` answer, the
//! stored Claude `live_quota` readings, and how they merge with the run-event
//! readings window by window. Each test is named `claude_live_...` after the
//! case of the spec it covers.

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

fn enable(f: &Fixture) {
    f.cmd().args(["config", "set", "live-quota", "on"]).assert().success();
}

/// Where fake-claude appends one JSON line per launch.
fn args_log(f: &Fixture) -> PathBuf {
    f.home_path().join("claude-args.log")
}

/// A `thoth` command whose claude launches (if any) are recorded in `args_log`.
fn thoth(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_ARGS_LOG", args_log(f));
    c
}

/// The `/usage` launches fake-claude recorded; none when it never ran.
fn usage_launches(f: &Fixture) -> Vec<Value> {
    match fs::read_to_string(args_log(f)) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|row| row["args"].as_array().is_some_and(|a| a.iter().any(|x| x == "/usage")))
            .collect(),
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

/// `claude-b` with live-quota on; its config dir.
fn claude_ready() -> (Fixture, PathBuf) {
    let f = setup();
    let dir = f.add_profile("claude-b");
    enable(&f);
    (f, dir)
}

/// A successful local `/usage` answer whose usage_report has these rows.
fn claude_answer(limits: Value) -> String {
    [
        json!({"type": "system", "subtype": "init"}),
        json!({"type": "assistant", "usage_report": {"rate_limits": {"limits": limits}}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0, "total_cost_usd": 0,
            "local_command": "usage", "result": "Current session: 1% used"}),
    ]
    .iter()
    .map(|v| format!("{v}\n"))
    .collect()
}

/// Where fake-claude records which credential variables it saw.
fn env_log(f: &Fixture) -> PathBuf {
    f.home_path().join("claude-env.log")
}

/// Writes `text` as the answer fake-claude prints; its path.
fn answer_file(f: &Fixture, text: &str) -> PathBuf {
    let path = f.home_path().join("claude-answer.jsonl");
    fs::write(&path, text).unwrap();
    path
}

/// Writes one Claude live reading straight into global.db.
fn insert_claude_reading(f: &Fixture, profile: &str, ts: &str, windows: Value) {
    insert_detail(f, profile, ts, &json!({"source": "claude_usage", "windows": windows}).to_string());
}

/// A completed run of `profile` with one `rate_limit_warning` event at `ts`
/// carrying `windows` (name, utilization 0-1, resets_at unix seconds).
fn insert_warning(f: &Fixture, profile: &str, ts: &str, windows: Value) {
    let conn = f.state_conn();
    let run = format!("RUN-{profile}-{ts}");
    conn.execute(
        "INSERT INTO runs (run_id, account, status, role) VALUES (?1, ?2, 'completed', 'implementer')",
        params![run, profile],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, 'rate_limit_warning', ?3)",
        params![ts, run, json!({"windows": windows}).to_string()],
    )
    .unwrap();
}

/// Every file and directory under `dir`, recursively, relative to it.
fn tree(dir: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            out.insert(path.strip_prefix(root).unwrap().display().to_string());
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

fn same_path(a: &str, b: &Path) -> bool {
    fs::canonicalize(a).unwrap() == fs::canonicalize(b).unwrap()
}

fn refresh_of(v: &Value) -> &Value {
    entry(&v["refresh"], "claude-b")
}

const NINE_ARGS: [&str; 9] = [
    "-p", "/usage", "--output-format", "stream-json", "--verbose", "--no-session-persistence",
    "--model", "haiku", "--strict-mcp-config",
];

#[test]
fn claude_live_success_stores_only_the_listed_fields_and_launches_claude_once() {
    let (f, dir) = claude_ready();
    let other_rows = |f: &Fixture| -> i64 {
        f.global_conn()
            .query_row("SELECT COUNT(*) FROM quota_events WHERE kind <> 'live_quota'", [], |r| r.get(0))
            .unwrap()
    };
    let before = other_rows(&f);
    let files_before = tree(&dir);
    let v = refresh_json(&f, |c| {
        c.env("CLAUDE_CODE_OAUTH_TOKEN", "junk")
            .env("ANTHROPIC_API_KEY", "junk")
            .env("ANTHROPIC_AUTH_TOKEN", "junk")
            .env("FAKE_CLAUDE_ENV_LOG", env_log(&f));
    });
    assert_eq!(refresh_of(&v)["status"], "refreshed", "{v}");
    assert_eq!(refresh_of(&v)["error"], Value::Null, "{v}");
    let env_lines: Vec<String> = fs::read_to_string(env_log(&f)).unwrap().lines().map(str::to_owned).collect();
    assert_eq!(env_lines.len(), 1, "{env_lines:?}");
    assert_eq!(serde_json::from_str::<Value>(&env_lines[0]).unwrap(), json!([]));
    assert_eq!(live_quota_rows(&f), 1);
    let text = newest_detail(&f);
    let detail: Value = serde_json::from_str(&text).unwrap();
    let keys: BTreeSet<&str> = detail.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["source", "windows"]));
    assert_eq!(detail["source"], "claude_usage");
    assert_eq!(detail["windows"], json!([
        {"name": "five_hour", "used_percent": 1.0, "resets_at": "2026-10-12T08:20:00Z"},
        {"name": "seven_day", "used_percent": 23.0, "resets_at": "2026-10-16T08:00:00Z"},
    ]));
    for w in detail["windows"].as_array().unwrap() {
        let keys: BTreeSet<&str> = w.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["name", "used_percent", "resets_at"]));
    }
    for word in ["severity", "extra_usage", "subscription", "session_id"] {
        assert!(!text.contains(word), "{text}");
    }
    let usage = usage_json(&f);
    assert_eq!(window(&usage, "claude-b", "five_hour")["used_percent"], 1.0, "{usage}");
    assert_eq!(window(&usage, "claude-b", "seven_day")["used_percent"], 23.0, "{usage}");
    assert_eq!(window(&usage, "claude-b", "five_hour")["source"], "claude_usage");
    assert_eq!(window(&usage, "claude-b", "seven_day")["source"], "claude_usage");
    let launches = usage_launches(&f);
    assert_eq!(launches.len(), 1, "{launches:?}");
    assert_eq!(launches[0]["args"], json!(NINE_ARGS));
    assert!(same_path(launches[0]["cwd"].as_str().unwrap(), &f.home_path()), "{}", launches[0]);
    assert!(same_path(launches[0]["claude_config_dir"].as_str().unwrap(), &dir), "{}", launches[0]);
    assert_eq!(tree(&dir), files_before);
    let plan: Option<String> = f
        .global_conn()
        .query_row("SELECT plan FROM profiles WHERE profile_id='claude-b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(plan, None);
    assert_eq!(other_rows(&f), before);
}

#[test]
fn claude_live_is_refused_when_off_or_inside_a_run() {
    let f = setup();
    f.add_profile("claude-b");
    let out = thoth(&f).args(["profile", "usage", "--refresh"]).assert().code(1).get_output().stderr.clone();
    let stderr = String::from_utf8(out).unwrap();
    assert!(stderr.contains("thoth config set live-quota on"), "{stderr}");
    enable(&f);
    let out = thoth(&f)
        .env("THOTH_RUN_ID", "01X")
        .args(["profile", "usage", "--refresh"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(out).unwrap();
    assert!(stderr.contains("THOTH_RUN_ID"), "{stderr}");
    assert!(usage_launches(&f).is_empty());
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn claude_live_profile_with_a_live_run_is_skipped_without_launching() {
    let (f, _) = claude_ready();
    f.state_conn()
        .execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE', 'claude-b', 'running', 'implementer')", [])
        .unwrap();
    for status in ["running", "queued"] {
        f.state_conn().execute("UPDATE runs SET status=?1 WHERE run_id='LIVE'", [status]).unwrap();
        let v = refresh_json(&f, |_| {});
        assert_eq!(refresh_of(&v)["status"], "skipped_live_run", "{status}: {v}");
        assert_eq!(refresh_of(&v)["error"], Value::Null, "{v}");
        assert!(usage_launches(&f).is_empty());
        assert_eq!(live_quota_rows(&f), 0);
    }
    f.state_conn().execute("UPDATE runs SET status='completed' WHERE run_id='LIVE'", []).unwrap();
    let v = refresh_json(&f, |_| {});
    assert_eq!(refresh_of(&v)["status"], "refreshed", "{v}");
    assert_eq!(usage_launches(&f).len(), 1);
}

#[test]
fn claude_live_timeout_ends_claude_at_once() {
    let (f, _) = claude_ready();
    let marker = f.home_path().join("claude-finished.txt");
    let started_file = f.home_path().join("claude-hang-started.txt");
    // The stand-in hangs for HANG_SECS, far past the 2 s timeout: the
    // "ended at once" bound below is the timeout plus 10 s, under half of
    // that, so a thoth that waited for the stand-in to finish on its own
    // would still fail, while the few seconds kill_tree's confirmation and
    // the exit take under load pass.
    const HANG_SECS: u64 = 30;
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE", "hang")
            .env("FAKE_CLAUDE_HANG_SECS", HANG_SECS.to_string())
            .env("FAKE_CLAUDE_HANG_MARKER", &marker)
            .env("FAKE_CLAUDE_HANG_STARTED", &started_file)
            .env("THOTH_LIVE_QUOTA_TIMEOUT_MS", "2000");
    });
    let returned = std::time::SystemTime::now();
    // Timed from the moment the stand-in started answering, so thoth's and
    // the stand-in's start-up time is not counted.
    assert!(started_file.exists(), "claude never started answering /usage");
    let started = fs::metadata(&started_file).unwrap().modified().unwrap();
    let since_start = returned.duration_since(started).unwrap_or_default();
    assert!(
        since_start < std::time::Duration::from_secs(12),
        "took {since_start:?} after the start; the bound is the 2s timeout plus 10s, under half the \
         {HANG_SECS}s the stand-in would hang on its own"
    );
    assert_eq!(refresh_of(&v)["status"], "failed", "{v}");
    assert_eq!(refresh_of(&v)["error"], "timed out", "{v}");
    // Past the moment the stand-in would write the marker on its own
    // (HANG_SECS after it started), plus a second.
    if let Ok(wait) = (started + std::time::Duration::from_secs(HANG_SECS + 1)).duration_since(std::time::SystemTime::now()) {
        std::thread::sleep(wait)
    }
    assert!(!marker.exists(), "claude finished on its own instead of being ended");
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn claude_live_exit_code_and_missing_binary_are_failures() {
    let (f, _) = claude_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE", "exit");
    });
    let error = refresh_of(&v)["error"].as_str().unwrap().to_owned();
    assert_eq!(refresh_of(&v)["status"], "failed", "{v}");
    assert!(error.starts_with("exited with code 1: ") && error.contains("fake-claude: usage failed"), "{error}");
    let missing = f.home_path().join("no-such-dir").join("claude");
    let v = refresh_json(&f, |c| {
        c.env("THOTH_CLAUDE_BIN", &missing);
    });
    assert_eq!(refresh_of(&v)["status"], "failed", "{v}");
    assert!(refresh_of(&v)["error"].as_str().unwrap().starts_with("could not run claude from THOTH_CLAUDE_BIN: "), "{v}");
    assert_eq!(live_quota_rows(&f), 0);
}

#[test]
fn claude_live_signed_out_reports_the_result_text() {
    let (f, _) = claude_ready();
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE", "signed_out");
    });
    assert_eq!(refresh_of(&v)["status"], "failed", "{v}");
    assert_eq!(refresh_of(&v)["error"], "claude reported an error: Not logged in · Please run /login", "{v}");
    assert_eq!(live_quota_rows(&f), 0);
    assert_eq!(entry(&usage_json(&f)["profiles"], "claude-b")["windows"], json!([]));
}

#[test]
fn claude_live_wrong_answers_store_nothing_and_keep_the_old_reading() {
    let (f, _) = claude_ready();
    let ok_result = json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0,
        "total_cost_usd": 0, "local_command": "usage", "result": "ok"});
    let lines = |values: &[Value]| values.iter().map(|v| format!("{v}\n")).collect::<String>();
    let report = |rate_limits: Value| json!({"type": "assistant", "usage_report": {"rate_limits": rate_limits}});
    let cases: Vec<(String, &str)> = vec![
        ("not json".into(), "printed no result line"),
        (lines(&[report(json!({"limits": [{"kind": "session", "percent": 1}]})),
            json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "total_cost_usd": 0.01, "result": "hi"})]),
            "did not answer /usage locally"),
        (lines(&[json!({"type": "assistant", "message": {}}), ok_result.clone()]), "no usage_report"),
        (lines(&[report(Value::Null), ok_result.clone()]), "could not read the plan's usage"),
        (lines(&[report(json!({"limits": null})), ok_result.clone()]), "no usage rows"),
        (claude_answer(json!([])), "no quota windows"),
        (claude_answer(json!([
            {"kind": "session", "percent": "35"},
            {"kind": "weekly_all", "percent": 150},
            {"kind": "Weekly All", "percent": 5},
            {"percent": 5}
        ])), "no quota windows"),
    ];
    for (text, needle) in &cases {
        let file = answer_file(&f, text);
        let v = refresh_json(&f, |c| {
            c.env("FAKE_CLAUDE_USAGE_FILE", &file);
        });
        assert_eq!(refresh_of(&v)["status"], "failed", "{needle}: {v}");
        assert!(refresh_of(&v)["error"].as_str().unwrap().contains(needle), "{needle}: {v}");
        assert_eq!(live_quota_rows(&f), 0, "{needle}");
        assert_eq!(entry(&usage_json(&f)["profiles"], "claude-b")["windows"], json!([]), "{needle}");
    }
    let file = answer_file(&f, &claude_answer(json!([
        {"kind": "session", "percent": 40, "resets_at": "2099-01-01T00:00:00Z"},
        {"kind": "weekly_all", "percent": 50, "resets_at": "2099-01-02T00:00:00Z"}
    ])));
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE_FILE", &file);
    });
    assert_eq!(refresh_of(&v)["status"], "refreshed", "{v}");
    assert_eq!(live_quota_rows(&f), 1);
    let kept = entry(&usage_json(&f)["profiles"], "claude-b")["windows"].clone();
    assert_eq!(kept.as_array().unwrap().len(), 2, "{kept}");
    let file = answer_file(&f, "not json");
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE_FILE", &file);
    });
    assert_eq!(refresh_of(&v)["status"], "failed", "{v}");
    assert_eq!(live_quota_rows(&f), 1);
    assert_eq!(entry(&usage_json(&f)["profiles"], "claude-b")["windows"], kept);
}

#[test]
fn claude_live_names_scoped_rows_and_counts_them_for_exhaustion() {
    let (f, _) = claude_ready();
    let file = answer_file(&f, &claude_answer(json!([
        {"kind": "session", "percent": 12.5, "resets_at": "2099-01-01T00:00:00.123456+00:00"},
        {"kind": "weekly_all", "percent": 30, "resets_at": null},
        {"kind": "weekly_scoped", "percent": 96, "scope": {"model": {"display_name": "Sonnet"}}, "resets_at": "2099-01-02T00:00:00Z"},
        {"kind": "session", "percent": 99}
    ])));
    let v = refresh_json(&f, |c| {
        c.env("FAKE_CLAUDE_USAGE_FILE", &file);
    });
    assert_eq!(refresh_of(&v)["status"], "refreshed", "{v}");
    let detail: Value = serde_json::from_str(&newest_detail(&f)).unwrap();
    assert_eq!(detail["windows"], json!([
        {"name": "five_hour", "used_percent": 12.5, "resets_at": "2099-01-01T00:00:00Z"},
        {"name": "seven_day", "used_percent": 30.0, "resets_at": null},
        {"name": "weekly_scoped:Sonnet", "used_percent": 96.0, "resets_at": "2099-01-02T00:00:00Z"},
    ]));
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "claude-b");
    assert_eq!(candidate["quota_state"], "exhausted", "{candidate}");
    assert_eq!(candidate["eligible"], false, "{candidate}");
    assert!(has_reason(candidate, &["weekly_scoped:Sonnet is at least 95% and has not reset"]), "{candidate}");
}

#[test]
fn claude_live_and_run_event_readings_merge_window_by_window() {
    let f = setup();
    f.add_profile("claude-b");
    insert_warning(&f, "claude-b", &at(Duration::minutes(-10)), json!([
        {"name": "five_hour", "utilization": 0.51, "resets_at": epoch(Duration::hours(2))},
        {"name": "seven_day_opus", "utilization": 0.25, "resets_at": epoch(Duration::days(3))}
    ]));
    insert_claude_reading(&f, "claude-b", &at(Duration::hours(-1)), json!([
        {"name": "five_hour", "used_percent": 20, "resets_at": at(Duration::hours(2))},
        {"name": "seven_day", "used_percent": 30, "resets_at": at(Duration::days(3))}
    ]));
    let check = |usage: &Value, expected: &[(&str, f64, &str)]| {
        let names: Vec<&str> = entry(&usage["profiles"], "claude-b")["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected.iter().map(|e| e.0).collect::<Vec<_>>(), "{usage}");
        for (name, used, source) in expected {
            assert_eq!(window(usage, "claude-b", name)["used_percent"], *used, "{usage}");
            assert_eq!(window(usage, "claude-b", name)["source"], *source, "{usage}");
        }
    };
    check(&usage_json(&f), &[
        ("five_hour", 51.0, "thoth_events"),
        ("seven_day", 30.0, "claude_usage"),
        ("seven_day_opus", 25.0, "thoth_events"),
    ]);
    insert_claude_reading(&f, "claude-b", &at(Duration::ZERO), json!([
        {"name": "five_hour", "used_percent": 60, "resets_at": at(Duration::hours(2))},
        {"name": "seven_day", "used_percent": 35, "resets_at": at(Duration::days(3))}
    ]));
    let step5 = [
        ("five_hour", 60.0, "claude_usage"),
        ("seven_day", 35.0, "claude_usage"),
        ("seven_day_opus", 25.0, "thoth_events"),
    ];
    let usage = usage_json(&f);
    check(&usage, &step5);
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "claude-b");
    assert_eq!(candidate["quota_state"], "ok", "{candidate}");
    assert_eq!(candidate["peak_percent"], 60.0, "{candidate}");
    let now = at(Duration::ZERO);
    insert_detail(&f, "claude-b", &now, "not json");
    insert_detail(&f, "claude-b", &now, &json!({"source": "claude_usage", "windows": [
        {"name": "five_hour", "used_percent": "x", "resets_at": null}]}).to_string());
    insert_detail(&f, "claude-b", &now, &json!({"source": "claude_usage", "windows": []}).to_string());
    insert_detail(&f, "claude-b", &now, &json!({"source": "agy_usage", "windows": [
        {"name": "five_hour", "used_percent": 99, "resets_at": null}]}).to_string());
    let again = usage_json(&f);
    check(&again, &step5);
    assert_eq!(entry(&again["profiles"], "claude-b")["windows"], entry(&usage["profiles"], "claude-b")["windows"]);
}

#[test]
fn claude_live_reason_without_a_reading_names_refresh() {
    let f = setup();
    f.add_profile("claude-b");
    let recommend = recommend_json(&f);
    let candidate = entry(&recommend["candidates"], "claude-b");
    assert!(has_reason(candidate, &["no quota reading yet", "thoth profile usage --refresh"]), "{candidate}");
}

#[test]
fn claude_live_enabling_notice_names_claude() {
    let f = setup();
    let v = parse(&f.cmd().args(["config", "set", "live-quota", "on", "--json"]).assert().success().get_output().stdout);
    let notice = v["notice"].as_str().unwrap();
    for needle in ["Claude Code", "/usage", "agy", "codex", "app-server", "official", "--refresh",
        "thoth config set live-quota off", "recommend", "10 minutes"] {
        assert!(notice.contains(needle), "{needle}: {notice}");
    }
    assert!(!notice.contains("only when you explicitly"), "{notice}");
}
