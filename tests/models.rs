mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const SOURCE_URL: &str = "https://aistupidlevel.info";
const KEY_URL: &str = "https://aistupidlevel.info/account/data-keys";

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}
fn output(f: &Fixture) -> Value {
    serde_json::from_slice(
        &f.cmd()
            .args(["models", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap()
}
fn fixture_url() -> String {
    let p = fs::canonicalize("tests/fixtures/aistupidlevel/v1-models.json").unwrap();
    let shown = p.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{shown}")
}
fn aa_fixture_url() -> String {
    let p = fs::canonicalize("tests/fixtures/artificialanalysis").unwrap();
    let shown = p.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{shown}/page-")
}
fn cache(at: &str, rows: Value) -> Value {
    json!({"schema":"aistupidlevel-v1","fetched_at":at,"url":"fixture","source":{"name":"AI Stupid Level","url":SOURCE_URL,"license":"test"},"models":{"success":true,"data":rows}})
}
fn row(name: &str, current: i64, status: &str, trend: &str) -> Value {
    json!({"id":"1","name":name,"provider":"test","currentScore":current,"trend":trend,"status":status,"confidenceLower":70.5,"confidenceUpper":90.5,"lastUpdated":"2026-09-29T07:00:00Z"})
}
fn aa_cache(rows: Value) -> Value {
    json!({"schema":"artificialanalysis-v2-free","fetched_at":"2026-09-29T00:00:00Z","source":{"name":"Artificial Analysis","url":"https://artificialanalysis.ai"},"pages":1,"data":rows})
}
fn aa_row(name: &str, intelligence: i64, coding: i64) -> Value {
    json!({"id":"fake","name":name,"evaluations":{"artificial_analysis_intelligence_index":intelligence,"artificial_analysis_coding_index":coding,"artificial_analysis_agentic_index":null},"pricing":{"price_1m_input_tokens":null,"price_1m_output_tokens":null}})
}

fn snapshot(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            if meta.is_dir() {
                walk(root, &path, out);
            } else if !path.to_string_lossy().ends_with("-wal")
                && !path.to_string_lossy().ends_with("-shm")
            {
                out.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    meta.len(),
                    meta.modified().unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// RJ-2: without a key `--refresh` still runs (exit 0): it says the two keyed
/// sources were skipped and fetches Agent Arena, which needs none; here curl
/// cannot start, so that only warns and nothing is written.
#[test]
fn t1_refresh_without_key_skips_keyed_sources_and_touches_nothing() {
    let f = setup();
    let empty = tempfile::tempdir().unwrap();
    // SQLite's -wal / -shm sidecars of the read-only open that now follows
    // the refresh are not files Thoth writes (as `snapshot` treats them).
    let names = |f: &Fixture| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(f.home_path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
            .collect();
        v.sort();
        v
    };
    let before = names(&f);
    f.cmd()
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env("PATH", empty.path())
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stderr(predicates::str::contains("AISTUPIDLEVEL_API_KEY"))
        .stderr(predicates::str::contains("ARTIFICIALANALYSIS_API_KEY"))
        .stderr(predicates::str::contains(KEY_URL))
        .stderr(predicates::str::contains("Free keys"))
        .stderr(predicates::str::contains("Agent Arena refresh failed"));
    assert_eq!(before, names(&f));
}

#[test]
fn aa_refresh_fetches_two_pages_maps_rows_and_never_leaks_key() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    fs::write(cfg.join("models_cache.json"),json!({"models":[{"slug":"gpt-5.6-luna","display_name":"Luna","visibility":"list","priority":1},{"slug":"unmatched-model","display_name":"Nope","visibility":"list","priority":2}]}).to_string()).unwrap();
    let key = "aa_secret_test_key";
    let assert = f
        .cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", key)
        .env("THOTH_AA_URL", aa_fixture_url())
        .args(["models", "--refresh", "--json"])
        .assert()
        .success();
    assert!(!String::from_utf8_lossy(&assert.get_output().stdout).contains(key));
    assert!(!String::from_utf8_lossy(&assert.get_output().stderr).contains(key));
    let v: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        v["external"]["artificial_analysis"]["cache"]["refreshed"],
        true
    );
    assert_eq!(
        v["profiles"][0]["models"][0]["aa"]["rows"][0]["effort"],
        "low"
    );
    assert_eq!(
        v["profiles"][0]["models"][0]["aa"]["rows"][1]["effort"],
        "xhigh"
    );
    assert!(v["profiles"][0]["models"][0]["aa"]["rows"][1]["coding_index"].is_null());
    assert_eq!(
        v["external"]["artificial_analysis"]["unmatched_profile_models"],
        json!(["codex-a/unmatched-model"])
    );
    let raw = fs::read_to_string(f.home_path().join("model-scores-aa.json")).unwrap();
    assert!(!raw.contains(key));
    let cache: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(cache["pages"], 2);
    assert_eq!(cache["data"].as_array().unwrap().len(), 4);
    f.cmd().arg("report").assert().success();
    let page = fs::read_to_string(f.home_path().join("report.html")).unwrap();
    assert!(!page.contains(key));
    assert!(page.contains("<a href=\"https://artificialanalysis.ai\">Artificial Analysis</a>"));
}

#[test]
fn aa_missing_second_page_preserves_old_cache_and_cleans_temporaries() {
    let f = setup();
    let old = b"old aa cache bytes";
    fs::write(f.home_path().join("model-scores-aa.json"), old).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("page-1"),
        r#"{"pagination":{"has_more":true},"data":[]}"#,
    )
    .unwrap();
    let base = format!(
        "file:///{}/page-",
        dir.path()
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    f.cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", "fake")
        .env("THOTH_AA_URL", base)
        .args(["models", "--refresh"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Artificial Analysis"));
    assert_eq!(
        fs::read(f.home_path().join("model-scores-aa.json")).unwrap(),
        old
    );
    assert!(fs::read_dir(f.home_path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn aa_fresh_cache_skips_curl() {
    let f = setup();
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    fs::write(f.home_path().join("model-scores-aa.json"),json!({"schema":"artificialanalysis-v2-free","fetched_at":now,"source":{"name":"Artificial Analysis","url":"https://artificialanalysis.ai"},"pages":1,"data":[]}).to_string()).unwrap();
    let empty = tempfile::tempdir().unwrap();
    f.cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", "fake")
        .env("PATH", empty.path())
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(predicates::str::contains("still fresh for 360 minutes"));
}

#[test]
fn refresh_freshness_messages_only_name_attempted_sources() {
    let f = setup();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        aa_cache(json!([])).to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        cache("2026-09-29T00:00:00Z", json!([])).to_string(),
    )
    .unwrap();
    let text = String::from_utf8(
        f.cmd()
            .env("AISTUPIDLEVEL_API_KEY", "fake")
            .env("THOTH_SCORES_URL", fixture_url())
            .args(["models", "--refresh"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(!text.contains("Artificial Analysis cache fetched"));

    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        aa_cache(json!([]))
            .to_string()
            .replace("2026-09-29T00:00:00Z", &now),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        cache("2026-09-29T00:00:00Z", json!([])).to_string(),
    )
    .unwrap();
    let text = String::from_utf8(
        f.cmd()
            .env("ARTIFICIALANALYSIS_API_KEY", "fake")
            .args(["models", "--refresh"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(text.contains("still fresh for 360 minutes"));
    assert!(!text.contains("AI Stupid Level updates roughly hourly"));
}

/// RJ-2: an alias that never ran has no name to look up: `score_name` and
/// `aa` are null; there is no hard-coded alias table to fall back to.
#[test]
fn claude_aa_name_uses_newest_harness_init_across_repos_and_nothing_without_a_run() {
    let f = setup();
    f.add_profile("claude-a");
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        aa_cache(json!([
            aa_row("Claude Sonnet Old", 10, 11),
            aa_row("Claude Sonnet New", 20, 21),
            aa_row("Claude Opus 5.5", 30, 31),
            aa_row("Claude Fable 5.1 (max)", 40, 41)
        ]))
        .to_string(),
    )
    .unwrap();
    let template = f.state_db();
    for (repo, ts, name) in [
        (
            "zzzz-old",
            "2026-01-01T00:00:00Z",
            "claude-sonnet-old-20260101",
        ),
        (
            "aaaa-new",
            "2026-02-01T00:00:00Z",
            "claude-sonnet-new-20260201",
        ),
    ] {
        let dir = f.home_path().join("repos").join(repo);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        fs::copy(&template, &db).unwrap();
        let c = rusqlite::Connection::open(db).unwrap();
        c.execute("INSERT INTO runs (run_id,account,model,status) VALUES (?1,'claude-a','sonnet','completed')",[repo]).unwrap();
        c.execute(
            "INSERT INTO events (ts,run_id,type,detail) VALUES (?1,?2,'harness_init',?3)",
            params![ts, repo, json!({"model":name}).to_string()],
        )
        .unwrap();
        f.global_conn()
            .execute(
                "INSERT INTO repos (repo_id,root_path,created_at) VALUES (?1,?1,?2)",
                params![repo, ts],
            )
            .unwrap();
    }
    let v = output(&f);
    let models = v["profiles"][0]["models"].as_array().unwrap();
    assert_eq!(models[1]["score_name"], "claude-sonnet-new", "{v}");
    assert_eq!(models[1]["aa"]["rows"][0]["name"], "Claude Sonnet New");
    for (i, alias) in [(0, "haiku"), (2, "opus"), (3, "fable")] {
        assert_eq!(models[i]["model"], alias, "{v}");
        assert!(models[i]["score_name"].is_null(), "{alias} never ran: {v}");
        assert!(models[i]["aa"].is_null(), "{alias} never ran: {v}");
    }
    assert_eq!(
        v["external"]["artificial_analysis"]["unmatched_profile_models"],
        json!(["claude-a/haiku", "claude-a/opus", "claude-a/fable"]),
        "{v}"
    );
}

#[test]
fn aa_plain_rows_follow_their_model_item() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    fs::write(cfg.join("models_cache.json"),json!({"models":[{"slug":"gpt-5.6-luna","display_name":"Luna","visibility":"list","priority":1}]}).to_string()).unwrap();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        aa_cache(json!([aa_row("GPT-5.6 Luna (low)", 71, 61)])).to_string(),
    )
    .unwrap();
    let text = String::from_utf8(
        f.cmd()
            .args(["models"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        text.find("gpt-5.6-luna (Luna)").unwrap()
            < text
                .find("Artificial Analysis low: coding 61 intelligence 71")
                .unwrap()
    );
}

#[test]
fn aa_non_array_data_names_failing_page_url() {
    let f = setup();
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("page-1"),
        r#"{"pagination":{"has_more":false},"data":{}}"#,
    )
    .unwrap();
    let base = format!(
        "file:///{}/page-",
        dir.path()
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let expected = format!("{base}1");
    f.cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", "fake")
        .env("THOTH_AA_URL", base)
        .args(["models", "--refresh"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(expected));
}

#[test]
fn refresh_sources_are_independent_and_successful_cache_survives_peer_failure() {
    let f = setup();
    f.cmd()
        .env("AISTUPIDLEVEL_API_KEY", "fake-asl")
        .env("THOTH_SCORES_URL", "file:///definitely/missing-asl")
        .env("ARTIFICIALANALYSIS_API_KEY", "fake-aa")
        .env("THOTH_AA_URL", aa_fixture_url())
        .args(["models", "--refresh"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("AI Stupid Level"));
    let cache: Value = serde_json::from_str(
        &fs::read_to_string(f.home_path().join("model-scores-aa.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["schema"], "artificialanalysis-v2-free");
    assert_eq!(cache["pages"], 2);
}

#[test]
fn aa_wrong_schema_is_not_fresh_or_present() {
    let f = setup();
    fs::write(f.home_path().join("model-scores-aa.json"),json!({"schema":"wrong","fetched_at":OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),"data":[]}).to_string()).unwrap();
    let v = output(&f);
    assert_eq!(
        v["external"]["artificial_analysis"]["cache"]["present"],
        false
    );
    f.cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", "fake")
        .env("THOTH_AA_URL", aa_fixture_url())
        .args(["models", "--refresh", "--json"])
        .assert()
        .success()
        .stdout(predicates::str::contains(r#""refreshed":true"#));
}

#[test]
fn t2_models_without_refresh_needs_no_key_or_curl() {
    let f = setup();
    f.add_profile("claude-a");
    let empty = tempfile::tempdir().unwrap();
    let out = f
        .cmd()
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env("PATH", empty.path())
        .args(["models", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["cache"]["present"], false);
    assert_eq!(v["source"]["name"], "AI Stupid Level");
}

/// RJ-2: a Claude alias is looked up under the model its newest
/// `harness_init` reported (date suffix removed), so the two aliases that are
/// to match a score get one run each; `haiku` never ran and has none.
#[test]
fn t3_refresh_maps_v1_scores_and_never_leaks_key() {
    let f = setup();
    f.add_profile("claude-a");
    let cfg = f.add_codex_profile("codex-a");
    fs::write(cfg.join("models_cache.json"), json!({"models":[{"slug":"gpt-5.6-luna","display_name":"Luna","visibility":"list","priority":1},{"slug":"gpt-5.6-sol","display_name":"Sol","visibility":"list","priority":2}]}).to_string()).unwrap();
    let c = f.state_conn();
    for (alias, reported) in [("sonnet", "claude-sonnet-5-20260101"), ("opus", "claude-opus-5-5-20260101")] {
        c.execute(
            "INSERT INTO runs (run_id,account,model,status) VALUES (?1,'claude-a',?2,'completed')",
            params![format!("T3-{alias}"), alias],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events (ts,run_id,type,detail) VALUES ('2026-01-01T00:00:00Z',?1,'harness_init',?2)",
            params![format!("T3-{alias}"), json!({"model": reported}).to_string()],
        )
        .unwrap();
    }
    drop(c);
    let key = "asl_live_test_key";
    let assert = f
        .cmd()
        .env("AISTUPIDLEVEL_API_KEY", key)
        .env("THOTH_SCORES_URL", fixture_url())
        .args(["models", "--refresh", "--json"])
        .assert()
        .success();
    assert!(!String::from_utf8_lossy(&assert.get_output().stdout).contains(key));
    assert!(!String::from_utf8_lossy(&assert.get_output().stderr).contains(key));
    let v: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(v["cache"]["refreshed"], true);
    assert!(v["profiles"][0]["models"][0]["score_name"].is_null(), "{v}");
    assert!(v["profiles"][0]["models"][0]["score"].is_null(), "{v}");
    assert_eq!(v["profiles"][0]["models"][1]["score_name"], "claude-sonnet-5");
    assert_eq!(v["profiles"][0]["models"][1]["score"]["current"], 88);
    assert_eq!(v["profiles"][0]["models"][2]["may_have_degraded"], true);
    assert_eq!(v["profiles"][1]["models"][0]["score"]["trend"], "down");
    assert!(v["profiles"][1]["models"][0]["score"]
        .get("baseline")
        .is_none());
    let cached = fs::read_to_string(f.home_path().join("model-scores.json")).unwrap();
    assert!(!cached.contains(key));
    let c: Value = serde_json::from_str(&cached).unwrap();
    assert_eq!(c["schema"], "aistupidlevel-v1");
    f.cmd().args(["report"]).assert().success();
    assert!(!fs::read_to_string(f.home_path().join("report.html"))
        .unwrap()
        .contains(key));
}

#[test]
fn t4_invalid_response_is_rejected() {
    let f = setup();
    let bad = tempfile::NamedTempFile::new().unwrap();
    fs::write(bad.path(), r#"{"success":false,"data":[]}"#).unwrap();
    let url = format!(
        "file:///{}",
        bad.path()
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    f.cmd()
        .env("AISTUPIDLEVEL_API_KEY", "test")
        .env("THOTH_SCORES_URL", url)
        .args(["models", "--refresh"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("success=true and data array"));
}

#[test]
fn t5_failed_refresh_preserves_cache_and_cleans_temporaries() {
    let f = setup();
    let old = b"old cache bytes";
    fs::write(f.home_path().join("model-scores.json"), old).unwrap();
    f.cmd()
        .env("AISTUPIDLEVEL_API_KEY", "test")
        .env("THOTH_SCORES_URL", "file:///definitely/missing")
        .args(["models", "--refresh"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("file:///definitely/missing"));
    assert_eq!(
        fs::read(f.home_path().join("model-scores.json")).unwrap(),
        old
    );
    assert!(fs::read_dir(f.home_path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[test]
fn t6_legacy_cache_is_unavailable_and_named_in_plain_output() {
    let f = setup();
    f.add_profile("claude-a");
    fs::write(
        f.home_path().join("model-scores.json"),
        r#"{"fetched_at":"2026-09-29T00:00:00Z"}"#,
    )
    .unwrap();
    let v = output(&f);
    assert_eq!(v["cache"]["present"], false);
    assert!(v["profiles"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["score"].is_null()));
    f.cmd()
        .args(["models"])
        .assert()
        .success()
        .stdout(predicates::str::contains("model-scores.json"))
        .stdout(predicates::str::contains("--refresh"));
}

#[test]
fn t7_fresh_cache_skips_curl_but_stale_cache_refreshes() {
    let f = setup();
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        cache(&now, json!([])).to_string(),
    )
    .unwrap();
    let empty = tempfile::tempdir().unwrap();
    f.cmd()
        .env("AISTUPIDLEVEL_API_KEY", "test")
        .env("PATH", empty.path())
        .args(["models", "--refresh"])
        .assert()
        .success()
        .stdout(predicates::str::contains("updates roughly hourly"))
        .stdout(predicates::str::contains("Delete the cache file"));
    let stale = (OffsetDateTime::now_utc() - Duration::minutes(61))
        .format(&Rfc3339)
        .unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        cache(&stale, json!([])).to_string(),
    )
    .unwrap();
    f.cmd()
        .env("AISTUPIDLEVEL_API_KEY", "test")
        .env("THOTH_SCORES_URL", fixture_url())
        .args(["models", "--refresh", "--json"])
        .assert()
        .success()
        .stdout(predicates::str::contains(r#""refreshed":true"#));
}

#[test]
fn t8_degraded_rule_uses_status_or_down_trend() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    fs::write(cfg.join("models_cache.json"), json!({"models":[{"slug":"warning-up","display_name":"A","visibility":"list","priority":1},{"slug":"good-down","display_name":"B","visibility":"list","priority":2},{"slug":"good-up","display_name":"C","visibility":"list","priority":3}]}).to_string()).unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        cache(
            "2026-09-29T00:00:00Z",
            json!([
                row("warning-up", 1, "warning", "up"),
                row("good-down", 2, "good", "down"),
                row("good-up", 3, "good", "up")
            ]),
        )
        .to_string(),
    )
    .unwrap();
    let models = output(&f)["profiles"][0]["models"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        models
            .iter()
            .map(|m| m["may_have_degraded"].as_bool().unwrap())
            .collect::<Vec<_>>(),
        [true, true, false]
    );
}

#[test]
fn t9_source_is_in_json_plain_text_and_html() {
    let f = setup();
    f.add_profile("claude-a");
    fs::write(
        f.home_path().join("model-scores.json"),
        cache(
            "2026-09-29T00:00:00Z",
            json!([row("claude-sonnet-5", 88, "good", "up")]),
        )
        .to_string(),
    )
    .unwrap();
    assert_eq!(output(&f)["source"]["url"], SOURCE_URL);
    f.cmd()
        .args(["models"])
        .assert()
        .success()
        .stdout(predicates::str::contains("AI Stupid Level"))
        .stdout(predicates::str::contains(SOURCE_URL));
    f.cmd().args(["report"]).assert().success();
    let page = fs::read_to_string(f.home_path().join("report.html")).unwrap();
    assert!(page.contains("<a href=\"https://aistupidlevel.info\">AI Stupid Level</a>"));
}

#[test]
fn codex_models_are_visible_and_priority_ordered() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    fs::write(cfg.join("models_cache.json"),json!({"models":[{"slug":"late","display_name":"Late","visibility":"list","priority":20},{"slug":"hidden","display_name":"Hidden","visibility":"hide","priority":0},{"slug":"gpt-5.6-luna","display_name":"Luna","visibility":"list","priority":10}]}).to_string()).unwrap();
    let v = output(&f);
    assert_eq!(v["profiles"][0]["models"][0]["model"], "gpt-5.6-luna");
    assert_eq!(v["profiles"][0]["models"].as_array().unwrap().len(), 2);
}

#[test]
fn claude_uses_fixed_aliases() {
    let f = setup();
    f.add_profile("claude-a");
    let v = output(&f);
    let names: Vec<_> = v["profiles"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["haiku", "sonnet", "opus", "fable"]);
}

#[test]
fn thoth_counts_runs_reviewed_pass_and_reject() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute("INSERT INTO runs (run_id, role, account, model, status) VALUES ('r1','implementer','claude-a','sonnet','completed'),('r2','implementer','claude-a','sonnet','completed'),('r3','reviewer','claude-a','sonnet','completed')", []).unwrap();
    c.execute("INSERT INTO reviews (review_id,target_run_id,verdict,findings,created_at) VALUES (?1,'r1','PASS','{}','2026-01-01T00:00:00Z'),(?2,'r1','REJECT','{}','2026-01-02T00:00:00Z')", params!["v1", "v2"]).unwrap();
    let v = output(&f);
    // RJ-3 (AC10): the JSON has no pass or reject counts; the human output
    // still prints them
    assert_eq!(
        v["profiles"][0]["models"][1]["thoth"],
        json!({"implementer_runs":2,"reviewed":1})
    );
    assert!(plain(&f).contains("Thoth runs 2 reviewed 1 PASS 1 REJECT 1"));
}

fn plain(f: &Fixture) -> String {
    String::from_utf8(f.cmd().args(["models"]).assert().success().get_output().stdout.clone()).unwrap()
}

/// Every key anywhere in `v`, objects and arrays walked all the way down.
fn all_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                out.push(k.clone());
                all_keys(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|child| all_keys(child, out)),
        _ => {}
    }
}

/// RJ-3 (AC10): with reviewed runs on this computer and on another one, no
/// pass, reject, pass rate or verdict reaches the commander through
/// `thoth models --json` (the same document as MCP `thoth_models`).
#[test]
fn models_json_carries_no_pass_reject_or_verdict_anywhere() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute("INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at) VALUES ('t1','t1','{\"task_type\":\"code\",\"difficulty\":\"high\"}','normal','completed','abc','2026-01-01','2026-01-01')", []).unwrap();
    c.execute("INSERT INTO runs (run_id,task_id,role,account,model,status,spec,started_at) VALUES ('r1','t1','implementer','claude-a','sonnet','completed','{\"launch\":{\"effort_requested\":\"high\",\"effort\":\"high\"}}','2026-01-01T00:00:00Z'),('rv','t1','reviewer','claude-a','opus','completed','{}','2026-01-01T01:00:00Z')", []).unwrap();
    c.execute("INSERT INTO reviews (review_id,target_run_id,reviewer_run_id,verdict,findings,created_at) VALUES ('v1','r1','rv','REJECT','{}','2026-01-01T02:00:00Z')", []).unwrap();
    c.execute("INSERT INTO events (run_id,type,detail) VALUES ('r1','launch_choice','{\"reason\":\"tests are many\",\"previous_run\":null}')", []).unwrap();
    drop(c);
    let dir = f.home_path().join("records");
    fs::create_dir_all(&dir).unwrap();
    let remote = json!({
        "v": 2, "machine": "beta", "repo_id": "deadbeef0000", "repo": "Thoth",
        "run_id": "R-REMOTE", "task_id": "T-REMOTE", "task_title": "remote", "attempt": 1,
        "role": "implementer", "profile": "claude-a", "provider": "anthropic",
        "harness": "claude-code", "model": "sonnet", "effort_requested": "high", "effort": "high",
        "task_type": "code", "difficulty": "high", "risk": "normal",
        "status": "completed", "interrupted_reason": null, "exit_code": 0,
        "started_at": "2026-01-02T00:00:00Z", "ended_at": "2026-01-02T00:10:00Z",
        "verdict": "PASS",
        "tokens": {"input": 5, "cache_read": null, "cache_write": null, "output": 6, "reasoning": null, "total": 11},
        "quota": null, "reason": null,
    });
    fs::write(dir.join("beta.jsonl"), format!("{remote}\n")).unwrap();
    f.cmd().args(["config", "set", "records-dir"]).arg(&dir).assert().success();
    f.cmd().args(["config", "set", "machine-name", "alpha"]).assert().success();

    let v = output(&f);
    // the fixture really reached the document: both computers, both runs
    assert_eq!(v["records"]["files"][0]["lines"], 1, "{v}");
    assert_eq!(v["decision_table"]["summary"]["machines"], 2, "{v}");
    assert_eq!(v["decision_table"]["choices"].as_array().unwrap().len(), 3, "{v}");
    assert_eq!(v["profiles"][0]["models"][1]["thoth"]["reviewed"], 2, "{v}");
    let mut keys = Vec::new();
    all_keys(&v, &mut keys);
    assert!(keys.iter().any(|k| k == "implementer_runs"), "{v}");
    for banned in ["pass", "reject", "pass_rate", "verdict"] {
        assert!(!keys.iter().any(|k| k == banned), "{banned} in {v}");
    }
    // the human output still prints them
    assert!(plain(&f).contains("PASS 1 REJECT 1"));
}

#[test]
fn thoth_breakdown_groups_specs_usage_and_latest_quota() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    for (task, spec) in [
        ("t1", r#"{"task_type":"code","difficulty":"high"}"#),
        ("t2", r#"{}"#),
        ("t3", r#"{"task_type":"docs","difficulty":"low"}"#),
    ] {
        c.execute("INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at) VALUES (?1,?1,?2,'normal','completed','abc','2026-01-01','2026-01-01')", params![task, spec]).unwrap();
    }
    let runs = [
        ("r1", "t1", r#"{"launch":{"effort":"high"}}"#, "10", "20"),
        ("r2", "t1", r#"{"launch":{"effort":"high"}}"#, "11", "21"),
        ("r3", "t2", r#"{}"#, "1", "2"),
        ("r4", "t3", r#"{"launch":{"effort":"high"}}"#, "3", "4"),
        ("rv", "t1", r#"{}"#, "99", "99"),
    ];
    for (id, task, spec, input, output) in runs {
        let role = if id == "rv" {
            "reviewer"
        } else {
            "implementer"
        };
        c.execute("INSERT INTO runs (run_id,task_id,role,account,model,status,spec,input_tokens,output_tokens) VALUES (?1,?2,?3,'claude-a','sonnet','completed',?4,?5,?6)", params![id, task, role, spec, input.parse::<i64>().unwrap(), output.parse::<i64>().unwrap()]).unwrap();
    }
    c.execute("INSERT INTO reviews (review_id,target_run_id,verdict,findings,created_at) VALUES ('v1','r1','PASS','{}','2026-01-01T00:00:00Z'),('v2','r2','REJECT','{}','2026-01-01T00:00:00Z'),('v3','r2','PASS','{}','2026-01-02T00:00:00Z'),('v4','r4','REJECT','{}','2026-01-01T00:00:00Z')", []).unwrap();
    c.execute("INSERT INTO events (run_id,type,detail) VALUES ('r1','run_quota','{\"points\":1.5}'),('r2','run_quota','{\"points\":2}')", []).unwrap();
    let model = &output(&f)["profiles"][0]["models"][1];
    let groups = model["thoth_breakdown"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(
        groups
            .iter()
            .map(|g| json!([
                g["effort"].clone(),
                g["task_type"].clone(),
                g["difficulty"].clone()
            ]))
            .collect::<Vec<_>>(),
        vec![
            json!([null, null, null]),
            json!(["high", "code", "high"]),
            json!(["high", "docs", "low"]),
        ]
    );
    let null_group = &groups[0];
    assert_eq!(null_group["implementer_runs"], 1);
    assert_eq!(null_group["completed"], 1);
    assert_eq!(null_group["reviewed"], 0);
    // RJ-3 (AC10): no pass or reject counts in the JSON; the labelled
    // groups' counts are checked in the human output below
    assert!(null_group.get("pass").is_none());
    assert!(null_group.get("reject").is_none());
    assert_eq!(null_group["input_tokens"], 1);
    assert_eq!(null_group["output_tokens"], 2);
    assert!(null_group["quota_points"].is_null());
    assert_eq!(null_group["quota_runs"], 0);
    let code = &groups[1];
    assert_eq!(code["effort"], "high");
    assert_eq!(code["difficulty"], "high");
    assert_eq!(code["implementer_runs"], 2);
    assert_eq!(code["reviewed"], 2);
    assert_eq!(code["input_tokens"], 21);
    assert_eq!(code["output_tokens"], 41);
    assert_eq!(code["quota_points"], 3.5);
    assert_eq!(code["quota_runs"], 2);
    let docs = &groups[2];
    assert_eq!(docs["implementer_runs"], 1);
    assert_eq!(docs["completed"], 1);
    assert_eq!(docs["reviewed"], 1);
    assert_eq!(docs["input_tokens"], 3);
    assert_eq!(docs["output_tokens"], 4);
    assert!(docs["quota_points"].is_null());
    assert_eq!(docs["quota_runs"], 0);
    assert_eq!(model["thoth"], json!({"implementer_runs":4,"reviewed":3}));
    let text = plain(&f);
    assert!(text.contains("Thoth runs 4 reviewed 3 PASS 2 REJECT 2"), "{text}");
    assert!(text.contains("effort high code/high runs 2 reviewed 2 PASS 2 REJECT 0"), "{text}");
    assert!(text.contains("effort high docs/low runs 1 reviewed 1 PASS 0 REJECT 1"), "{text}");
}

#[test]
fn thoth_breakdown_invalid_specs_are_an_all_null_group() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute("INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at) VALUES ('banana','banana','{\"task_type\":\"banana\",\"difficulty\":\"extreme\"}','normal','completed','abc','2026-01-01','2026-01-01')", []).unwrap();
    c.execute("INSERT INTO runs (run_id,task_id,role,account,model,status,spec) VALUES ('bad','banana','implementer','claude-a','sonnet','completed','not json')", []).unwrap();
    let model = &output(&f)["profiles"][0]["models"][1];
    let groups = model["thoth_breakdown"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert!(
        groups[0]["effort"].is_null()
            && groups[0]["task_type"].is_null()
            && groups[0]["difficulty"].is_null()
    );
    let text = String::from_utf8(
        f.cmd()
            .args(["models"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(!text.lines().any(|line| line.contains("effort null")));
}

#[test]
fn thoth_breakdown_quota_counts_latest_valid_event_per_run() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute("INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at) VALUES ('quota','quota','{}','normal','completed','abc','2026-01-01','2026-01-01')", []).unwrap();
    for id in ["q1", "q2", "q3"] {
        c.execute("INSERT INTO runs (run_id,task_id,role,account,model,status,spec) VALUES (?1,'quota','implementer','claude-a','sonnet','completed','{}')", [id]).unwrap();
    }
    c.execute("INSERT INTO events (run_id,type,detail) VALUES ('q1','run_quota','{\"points\":1.5}'),('q2','run_quota','{\"points\":99}'),('q2','run_quota','{\"points\":2}')", []).unwrap();
    let groups = output(&f)["profiles"][0]["models"][1]["thoth_breakdown"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["quota_points"], 3.5);
    assert_eq!(groups[0]["quota_runs"], 2);
}

#[test]
fn models_plain_output_shows_labeled_breakdown_only_for_labeled_groups() {
    let f = setup();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute("INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at) VALUES ('plain','plain','{\"task_type\":\"code\",\"difficulty\":\"high\"}','normal','completed','abc','2026-01-01','2026-01-01')", []).unwrap();
    c.execute("INSERT INTO runs (run_id,task_id,role,account,model,status,spec) VALUES ('plain-run','plain','implementer','claude-a','sonnet','completed','{\"launch\":{\"effort\":\"high\"}}')", []).unwrap();
    let text = String::from_utf8(
        f.cmd()
            .args(["models"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(text
        .lines()
        .any(|line| line.contains("effort high") && line.contains("code/high")));
    assert!(!text.lines().any(|line| line.contains("effort null")));
}

#[test]
fn models_is_read_only_even_with_unreadable_credentials() {
    let f = setup();
    let cfg = f.add_profile("claude-a");
    fs::write(cfg.join(".credentials.json"), "secret").unwrap();
    fs::write(cfg.join("auth.json"), "secret").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in [".credentials.json", "auth.json"] {
            fs::set_permissions(cfg.join(name), fs::Permissions::from_mode(0o000)).unwrap();
        }
    }
    let before_home = snapshot(&f.home_path());
    let before_cfg = snapshot(&cfg);
    output(&f);
    assert_eq!(before_home, snapshot(&f.home_path()));
    assert_eq!(before_cfg, snapshot(&cfg));
}
