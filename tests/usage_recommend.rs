mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::Value;
use std::fs;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("valid json")
}

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta)
        .format(&Rfc3339)
        .unwrap()
}

fn epoch(delta: Duration) -> i64 {
    (OffsetDateTime::now_utc() + delta).unix_timestamp()
}

fn add_claude(f: &Fixture, id: &str) -> std::path::PathBuf {
    let cfg = f.home_path().join("profiles").join(id);
    fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args([
            "profile",
            "add",
            id,
            "--provider",
            "anthropic",
            "--harness",
            "claude-code",
            "--config-dir",
        ])
        .arg(&cfg)
        .assert()
        .success();
    cfg
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

fn make_registered_repo(f: &Fixture) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    common::git(repo.path(), &["init", "--initial-branch=main"]);
    common::git(repo.path(), &["config", "user.email", "test@example.com"]);
    common::git(repo.path(), &["config", "user.name", "thoth test"]);
    fs::write(repo.path().join("other.txt"), "other\n").unwrap();
    common::git(repo.path(), &["add", "other.txt"]);
    common::git(repo.path(), &["commit", "-m", "first"]);
    f.cmd_in(repo.path()).arg("init").assert().success();
    repo
}

fn codex_window(f: &Fixture, id: &str, used: f64, window_minutes: i64, resets_at: i64) {
    let cfg = f.add_codex_profile(id);
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        serde_json::json!({"timestamp":at(Duration::hours(-1)),"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":used,"window_minutes":window_minutes,"resets_at":resets_at}}}}).to_string() + "\n",
    ).unwrap();
}

fn snapshot(root: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, u64, std::time::SystemTime)>,
    ) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::metadata(&path).unwrap();
            if metadata.is_dir() {
                walk(root, &path, out);
            } else if !path.to_string_lossy().ends_with("-wal")
                && !path.to_string_lossy().ends_with("-shm")
            {
                out.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    metadata.len(),
                    metadata.modified().unwrap(),
                ));
            }
        }
    }
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn profile_usage_reports_claude_windows_and_live_runs() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    add_event(
        &f,
        "RUN-A",
        "claude-a",
        "2026-09-28T00:00:00Z",
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.42,"resets_at":4102444800u64}),
    );
    add_event(
        &f,
        "RUN-A",
        "claude-a",
        "2026-09-28T01:00:00Z",
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.51,"resets_at":4102444800u64}),
    );
    let conn = f.state_conn();
    conn.execute("UPDATE runs SET status='running' WHERE run_id='RUN-A'", [])
        .unwrap();
    let out = f
        .cmd()
        .args(["profile", "usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["profiles"][0]["profile_id"], "claude-a");
    assert_eq!(v["profiles"][0]["live_runs"], 1);
    assert_eq!(v["profiles"][0]["windows"][0]["used_percent"], 51.0);
    assert_eq!(v["profiles"][0]["windows"][0]["source"], "thoth_events");
    assert_eq!(
        v["profiles"][0]["windows"][0]["reset_since_observed"],
        false
    );
}

#[test]
fn profile_usage_json_carries_quota_state_peak_percent_and_newest_observed_at() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    add_claude(&f, "claude-b");
    add_event(
        &f,
        "RUN-A",
        "claude-a",
        &at(Duration::minutes(-5)),
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.42,"resets_at":4102444800u64}),
    );
    let out = f
        .cmd()
        .args(["profile", "usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    let profiles = v["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 2);
    for p in profiles {
        let obj = p.as_object().unwrap();
        for key in ["quota_state", "peak_percent", "newest_observed_at"] {
            assert!(obj.contains_key(key), "{p} lacks {key}");
        }
        let state = p["quota_state"].as_str().unwrap();
        assert!(
            ["ok", "unknown", "exhausted"].contains(&state),
            "unexpected quota_state {state}"
        );
    }
}

#[test]
fn profile_usage_reports_reset_since_observed_true_once_the_reset_time_has_passed() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "codex-reset", 52.0, 300, 0);
    codex_window(&f, "codex-live", 6.0, 300, 4102444800);

    let out = f
        .cmd()
        .args(["profile", "usage", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(v["profiles"][0]["profile_id"], "codex-live");
    assert_eq!(
        v["profiles"][0]["windows"][0]["reset_since_observed"],
        false
    );
    assert_eq!(v["profiles"][1]["profile_id"], "codex-reset");
    assert_eq!(v["profiles"][1]["windows"][0]["reset_since_observed"], true);
}

#[test]
fn profile_usage_merges_claude_events_across_registered_repos_and_skips_missing_state() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    add_claude(&f, "claude-other");
    let second_repo = make_registered_repo(&f);
    let repo_ids = {
        let g = f.global_conn();
        let mut s = g
            .prepare("SELECT repo_id FROM repos ORDER BY repo_id")
            .unwrap();
        s.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let second_id = repo_ids
        .into_iter()
        .find(|id| f.home_path().join("repos").join(id).join("state.db") != f.state_db())
        .unwrap();
    let second_state = f
        .home_path()
        .join("repos")
        .join(&second_id)
        .join("state.db");
    let c = rusqlite::Connection::open(second_state).unwrap();
    c.execute("INSERT INTO runs (run_id, account, status, role) VALUES ('RUN-SECOND', 'claude-a', 'completed', 'implementer')", []).unwrap();
    c.execute("INSERT INTO events (ts, run_id, type, detail) VALUES ('2026-09-28T02:00:00Z', 'RUN-SECOND', 'rate_limit_warning', ?1)", [serde_json::json!({"rate_limit_type":"five_hour","utilization":0.70,"resets_at":4102444800u64}).to_string()]).unwrap();
    add_event(
        &f,
        "RUN-FIRST",
        "claude-a",
        "2026-09-28T01:00:00Z",
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.20,"resets_at":4102444800u64}),
    );
    add_event(
        &f,
        "RUN-OTHER",
        "claude-other",
        "2026-09-28T03:00:00Z",
        serde_json::json!({"rate_limit_type":"weekly","utilization":0.99,"resets_at":4102444800u64}),
    );
    c.execute("INSERT INTO runs (run_id, account, status, role) VALUES ('RUN-SECOND-WEEK', 'claude-a', 'completed', 'implementer')", []).unwrap();
    c.execute("INSERT INTO events (ts, run_id, type, detail) VALUES ('2026-09-28T02:30:00Z', 'RUN-SECOND-WEEK', 'rate_limit_warning', ?1)", [serde_json::json!({"rate_limit_type":"weekly","utilization":0.30,"resets_at":4102444800u64}).to_string()]).unwrap();
    let missing = f
        .home_path()
        .join("repos")
        .join("missing-repo")
        .join("state.db");
    f.global_conn()
        .execute(
            "INSERT INTO repos (repo_id, root_path, created_at) VALUES ('missing-repo', '', '')",
            [],
        )
        .unwrap();
    assert!(!missing.exists());
    let out = f
        .cmd()
        .args(["profile", "usage", "claude-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let windows = json(&out)["profiles"][0]["windows"].clone();
    assert_eq!(windows[0]["used_percent"], 70.0);
    assert_eq!(windows[1]["used_percent"], 30.0);
    assert!(!missing.exists());
    drop(second_repo);
}

#[test]
fn recommend_prefers_a_quota_reading_over_no_reading_when_other_keys_tie() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-with-reading");
    f.add_codex_profile("codex-without-reading");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        token_count_line(
            &at(Duration::hours(-1)),
            serde_json::json!({"primary":{"used_percent":20,"window_minutes":300}}),
        ),
    )
    .unwrap();

    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(first_eligible(&json(&out)), "codex-with-reading");
}

#[test]
fn recommend_orders_live_runs_readings_peak_reset_and_profile_id() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "reset-high", 99.0, 300, 0);
    codex_window(&f, "low-a", 10.0, 300, 4102444800);
    codex_window(&f, "low-b", 10.0, 300, 4102444800);
    codex_window(&f, "high", 20.0, 300, 4102444800);
    f.add_codex_profile("no-reading");
    let live_cfg = f.add_codex_profile("live");
    let sessions = live_cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        token_count_line(
            &at(Duration::hours(-1)),
            serde_json::json!({"primary":{"used_percent":1,"window_minutes":300}}),
        ),
    )
    .unwrap();
    f.state_conn().execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE-RUN', 'live', 'queued', 'implementer')", []).unwrap();
    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = json(&out);
    let ids = value["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["profile_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["reset-high", "low-a", "low-b", "high", "no-reading", "live"]
    );
}

#[test]
fn reviewer_recommendation_prefers_a_different_provider_for_a_task() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-impl");
    f.add_codex_profile("codex-review");
    let (task_id, _, _) = f.seed_run(
        "claude-impl",
        r#"{"goal":"test recommendation","acceptance_criteria":["AC1: works"]}"#,
    );
    let out = f
        .cmd()
        .args([
            "recommend",
            "--role",
            "reviewer",
            "--task",
            &task_id,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = json(&out);
    assert_eq!(first_eligible(&value), "codex-review");
    assert!(value["candidates"][0]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "different provider from the implementer (openai vs anthropic)"));
}

#[test]
fn read_only_commands_do_not_touch_credentials_or_databases() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = add_claude(&f, "claude-a");
    fs::write(cfg.join(".credentials.json"), "secret").unwrap();
    fs::write(cfg.join("auth.json"), "secret").unwrap();
    fs::write(
        f.home_path().join("config.json"),
        r#"{"same_provider_rotation":false}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in [".credentials.json", "auth.json"] {
            let path = cfg.join(name);
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        }
    }
    let before_home = snapshot(&f.home_path());
    let before_cfg = snapshot(&cfg);
    f.cmd()
        .args(["profile", "usage", "--json"])
        .assert()
        .success();
    f.cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success();
    assert_eq!(before_home, snapshot(&f.home_path()));
    assert_eq!(before_cfg, snapshot(&cfg));
}

#[test]
fn profile_usage_reads_newest_codex_rollout_file_and_names_windows_by_minutes() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let sessions = cfg.join("sessions").join("2026");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("z-old.jsonl"),
        "{bad\n{\"timestamp\":\"2026-09-28T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"primary\":{\"used_percent\":12,\"window_minutes\":300},\"secondary\":null}}}\n",
    ).unwrap();
    let newest = sessions.join("a-new.jsonl");
    fs::write(
        &newest,
        "not json\n{\"timestamp\":\"2026-09-28T02:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"primary\":{\"used_percent\":88,\"window_minutes\":300,\"resets_at\":4102444800},\"secondary\":{\"used_percent\":4,\"window_minutes\":10080}}}}\n",
    ).unwrap();
    let out = f
        .cmd()
        .args(["profile", "usage", "codex-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let windows = json(&out)["profiles"][0]["windows"].clone();
    assert_eq!(windows[0]["name"], "five_hour");
    assert_eq!(windows[1]["name"], "seven_day");
    assert_eq!(windows[1]["source"], "codex_rollout");
    assert_eq!(windows[0]["used_percent"], 88.0);
}

fn token_count_line(ts: &str, rate_limits: serde_json::Value) -> String {
    serde_json::json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": rate_limits}})
        .to_string()
        + "\n"
}

/// RJ-1: `recommend` has no `recommended` key; the first eligible candidate
/// is what it used to name.
fn first_eligible(value: &Value) -> &str {
    value["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["eligible"] == true)
        .and_then(|c| c["profile_id"].as_str())
        .unwrap_or_else(|| panic!("no eligible candidate: {value}"))
}

fn candidate<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == id)
        .unwrap()
}

fn recommend_json(f: &Fixture) -> Value {
    json(
        &f.cmd()
            .args(["recommend", "--role", "implementer", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
}

#[test]
fn quota_state_exhausted_even_when_observation_is_three_days_old() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("a");
    fs::create_dir_all(cfg.join("sessions")).unwrap();
    fs::write(cfg.join("sessions/r.jsonl"), token_count_line(&at(Duration::days(-3)), serde_json::json!({"primary":{"used_percent":99,"window_minutes":300,"resets_at":epoch(Duration::hours(2))}}))).unwrap();
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "a")["quota_state"], "exhausted");
    assert_eq!(candidate(&v, "a")["peak_percent"], Value::Null);
}

#[test]
fn quota_state_ok_with_fresh_reading_and_reports_peak() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "a", 50.0, 300, epoch(Duration::hours(2)));
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "a")["quota_state"], "ok");
    assert_eq!(candidate(&v, "a")["peak_percent"], 50.0);
}

#[test]
fn quota_state_unknown_when_newest_reading_is_over_24_hours_old() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("a");
    fs::create_dir_all(cfg.join("sessions")).unwrap();
    fs::write(cfg.join("sessions/r.jsonl"), token_count_line(&at(Duration::hours(-30)), serde_json::json!({"primary":{"used_percent":50,"window_minutes":300,"resets_at":epoch(Duration::hours(2))}}))).unwrap();
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "a")["quota_state"], "unknown");
    assert_eq!(candidate(&v, "a")["peak_percent"], Value::Null);
}

#[test]
fn quota_state_unknown_without_readings() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_codex_profile("a");
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "a")["quota_state"], "unknown");
    assert_eq!(candidate(&v, "a")["newest_observed_at"], Value::Null);
}

#[test]
fn quota_state_ok_with_zero_peak_after_reset() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "a", 80.0, 300, epoch(Duration::hours(-1)));
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "a")["quota_state"], "ok");
    assert_eq!(candidate(&v, "a")["peak_percent"], 0.0);
}

fn parked_fixture(reset: Option<i64>, with_window: bool) -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    let parked = at(Duration::hours(-2));
    {
        let g = f.global_conn();
        g.execute(
            "UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-a'",
            [],
        )
        .unwrap();
        g.execute("INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1,'claude-a','rate_limited','{}')", [&parked]).unwrap();
    }
    if with_window {
        add_event(
            &f,
            "R",
            "claude-a",
            &at(Duration::hours(-1)),
            serde_json::json!({"rate_limit_type":"five_hour","utilization":0.99,"resets_at":reset}),
        );
    }
    f
}

#[test]
fn rate_limited_profile_recovers_after_a_post_event_window_reset() {
    let f = parked_fixture(Some(epoch(Duration::minutes(-30))), true);
    let v = recommend_json(&f);
    let c = candidate(&v, "claude-a");
    assert_eq!(c["eligible"], true);
    assert_eq!(c["recovered"], true);
    assert!(c["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("window reset at")));
}

#[test]
fn rate_limited_profile_does_not_recover_before_reset() {
    let f = parked_fixture(Some(epoch(Duration::hours(1))), true);
    let v = recommend_json(&f);
    let c = candidate(&v, "claude-a");
    assert_eq!(c["eligible"], false);
    assert_eq!(c["recovered"], false);
}

#[test]
fn rate_limited_profile_without_windows_stays_parked_with_check_reason() {
    let f = parked_fixture(None, false);
    let v = recommend_json(&f);
    let c = candidate(&v, "claude-a");
    assert_eq!(c["eligible"], false);
    assert!(c["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("thoth profile check")));
}

fn rotation_fixture(config: Option<&str>) -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-b");
    add_claude(&f, "claude-c");
    f.add_codex_profile("codex-a");
    add_event(
        &f,
        "B",
        "claude-b",
        &at(Duration::hours(-1)),
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.99,"resets_at":epoch(Duration::hours(2))}),
    );
    add_event(
        &f,
        "C",
        "claude-c",
        &at(Duration::hours(-1)),
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.10,"resets_at":epoch(Duration::hours(2))}),
    );
    if let Some(text) = config {
        fs::write(f.home_path().join("config.json"), text).unwrap();
    }
    f
}

/// RJ-1: a used-up claude-b no longer holds claude-c back; each profile is
/// judged on its own facts (what rotation on gave before).
#[test]
fn a_blocked_sibling_does_not_hold_the_others() {
    let f = rotation_fixture(None);
    let v = recommend_json(&f);
    let c = candidate(&v, "claude-c");
    assert_eq!(c["eligible"], true, "{v}");
    assert_eq!(first_eligible(&v), "claude-c", "{v}");
    assert!(c.get("held").is_none(), "{c}");
    assert!(!c["reasons"].as_array().unwrap().iter().any(|r| r
        .as_str()
        .unwrap()
        .contains("same-provider rotation")), "{c}");
}

#[test]
fn ranking_uses_quota_state_peak_then_live_runs() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "a", 40.0, 300, epoch(Duration::hours(2)));
    f.add_codex_profile("b");
    codex_window(&f, "c", 10.0, 300, epoch(Duration::hours(2)));
    let ids = |v: &Value| {
        v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["profile_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&recommend_json(&f)), ["c", "a", "b"]);
    f.state_conn().execute("INSERT INTO runs (run_id, account, status, role) VALUES ('LIVE','c','running','implementer')", []).unwrap();
    assert_eq!(ids(&recommend_json(&f)), ["a", "b", "c"]);
}

#[test]
fn claude_windows_array_is_reported_and_exhausts_recommendation() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    add_event(
        &f,
        "R",
        "claude-a",
        &at(Duration::hours(-1)),
        serde_json::json!({"windows":[{"name":"five_hour","utilization":0.2,"resets_at":epoch(Duration::hours(2))},{"name":"seven_day","utilization":0.97,"resets_at":epoch(Duration::hours(3))}]}),
    );
    let usage = json(
        &f.cmd()
            .args(["profile", "usage", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert_eq!(usage["profiles"][0]["windows"].as_array().unwrap().len(), 2);
    let v = recommend_json(&f);
    assert_eq!(candidate(&v, "claude-a")["quota_state"], "exhausted");
    assert_eq!(candidate(&v, "claude-a")["eligible"], false);
}

#[test]
fn profile_usage_reads_the_general_codex_bucket_not_a_newer_model_reserve() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    // The shape codex-cli 0.154 wrote on 2026-09-28: the general `codex`
    // bucket, then a per-model reserve bucket on later lines of the same run.
    let text = token_count_line(
        "2026-09-28T04:54:11Z",
        serde_json::json!({
            "limit_id": "codex", "limit_name": null,
            "primary": {"used_percent": 16.0, "window_minutes": 300, "resets_at": 4102444800u64},
            "secondary": {"used_percent": 8.0, "window_minutes": 10080, "resets_at": 4102444800u64}
        }),
    ) + &token_count_line(
        "2026-09-28T05:26:44Z",
        serde_json::json!({
            "limit_id": "base_model_inference", "limit_name": "gpt-reserve",
            "primary": {"used_percent": 0.0, "window_minutes": 10080, "resets_at": 4102444800u64},
            "secondary": null
        }),
    );
    fs::write(sessions.join("rollout.jsonl"), text).unwrap();

    let out = f
        .cmd()
        .args(["profile", "usage", "codex-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let windows = json(&out)["profiles"][0]["windows"].clone();
    assert_eq!(windows.as_array().unwrap().len(), 2, "{windows}");
    assert_eq!(windows[0]["name"], "five_hour");
    assert_eq!(windows[0]["used_percent"], 16.0);
    assert_eq!(windows[1]["name"], "seven_day");
    assert_eq!(windows[1]["used_percent"], 8.0);
}

#[test]
fn profile_usage_looks_past_a_newer_file_that_holds_only_a_reserve_bucket() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let older = sessions.join("older.jsonl");
    // No limit_id at all: what codex wrote before it named its buckets.
    fs::write(
        &older,
        token_count_line(
            "2026-09-27T00:00:00Z",
            serde_json::json!({
                "primary": {"used_percent": 30.0, "window_minutes": 300, "resets_at": 4102444800u64}
            }),
        ),
    )
    .unwrap();
    fs::File::options()
        .write(true)
        .open(&older)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    fs::write(sessions.join("newer.jsonl"), token_count_line("2026-09-28T00:00:00Z", serde_json::json!({
        "limit_id": "base_model_inference", "limit_name": "gpt-reserve",
        "primary": {"used_percent": 0.0, "window_minutes": 10080, "resets_at": 4102444800u64}
    }))).unwrap();

    let out = f
        .cmd()
        .args(["profile", "usage", "codex-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let windows = json(&out)["profiles"][0]["windows"].clone();
    assert_eq!(windows.as_array().unwrap().len(), 1, "{windows}");
    assert_eq!(windows[0]["name"], "five_hour");
    assert_eq!(windows[0]["used_percent"], 30.0);
}

#[test]
fn profile_usage_omits_a_null_window_from_the_selected_codex_reading() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        "{\"timestamp\":\"2026-09-28T02:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"primary\":{\"used_percent\":17,\"window_minutes\":777},\"secondary\":null}}}\n",
    )
    .unwrap();

    let out = f
        .cmd()
        .args(["profile", "usage", "codex-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let windows = json(&out)["profiles"][0]["windows"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["name"], "777m");
}

#[test]
fn profile_usage_human_output_uses_plain_strings() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_claude(&f, "claude-a");
    add_event(
        &f,
        "RUN-A",
        "claude-a",
        "2026-09-28T00:00:00Z",
        serde_json::json!({
            "rate_limit_type":"five_hour", "utilization":0.42, "resets_at":4102444800u64
        }),
    );

    let out = f
        .cmd()
        .args(["profile", "usage", "claude-a"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("claude-a  anthropic / claude-code  status=active"),
        "{text}"
    );
    assert!(
        text.contains("five_hour: 42.0% resets_at=2100-01-01T00:00:00Z"),
        "{text}"
    );
    assert!(!text.contains('"'), "{text}");
}

#[test]
fn recommend_excludes_hot_or_inactive_profiles_and_ranks_eligible_profiles() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    fs::write(
        f.home_path().join("config.json"),
        r#"{"same_provider_rotation":true}"#,
    )
    .unwrap();
    add_claude(&f, "claude-hot");
    add_claude(&f, "claude-good");
    {
        let g = f.global_conn();
        g.execute(
            "UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-hot'",
            [],
        )
        .unwrap();
    }
    add_claude(&f, "claude-full");
    add_event(
        &f,
        "RUN-G",
        "claude-good",
        &at(Duration::hours(-1)),
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.20,"resets_at":epoch(Duration::hours(2))}),
    );
    add_event(
        &f,
        "RUN-F",
        "claude-full",
        &at(Duration::hours(-1)),
        serde_json::json!({"rate_limit_type":"five_hour","utilization":0.96,"resets_at":epoch(Duration::hours(2))}),
    );
    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json(&out);
    assert_eq!(first_eligible(&v), "claude-good");
    assert_eq!(v["candidates"].as_array().unwrap().len(), 3);
    assert_eq!(v["candidates"][2]["eligible"], false);
    assert_eq!(v["candidates"][1]["eligible"], false);
}

#[test]
fn recommend_reasons_explain_reset_and_active_window_ranking() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    codex_window(&f, "reset-high", 52.0, 300, 0);
    codex_window(&f, "active-low", 6.0, 10080, 4102444800);

    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value = json(&out);
    assert_eq!(first_eligible(&value), "reset-high");
    let candidates = value["candidates"].as_array().unwrap();
    let reset_reasons = candidates[0]["reasons"].as_array().unwrap();
    assert!(
        reset_reasons.iter().any(
            |r| r.as_str().is_some_and(|s| s.contains("five_hour: was 52%")
                && s.contains("reset at 1970-01-01T00:00:00Z (counts as 0%)"))
        ),
        "{reset_reasons:?}"
    );
    let active_reasons = candidates[1]["reasons"].as_array().unwrap();
    assert!(
        active_reasons
            .iter()
            .any(|r| r == "seven_day: 6% used, resets 2100-01-01T00:00:00Z"),
        "{active_reasons:?}"
    );
}
