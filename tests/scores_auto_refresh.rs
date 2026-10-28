//! DT-6: the `scores-auto-refresh` switch, the automatic refresh of the
//! external score caches, and the staleness keys of `thoth models`.
//! Responses are `file://` URLs to files the tests write themselves; a case
//! that must not touch the network points PATH at an empty folder so curl
//! cannot even start. Always the fakes; no real harness is ever launched.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::SystemTime;
use tempfile::TempDir;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const CODEX_MODEL: &str = "gpt-5.6-luna";
const ASL_KEY: &str = "asl-fake-key-1";
const AA_KEY: &str = "aa-fake-key-1";
const LOCK: &str = ".model-scores.refresh.lock";

// --- helpers -----------------------------------------------------------------

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn switch(f: &Fixture, value: &str) {
    f.cmd()
        .args(["config", "set", "scores-auto-refresh", value])
        .assert()
        .success();
}

fn ago(hours: i64) -> String {
    (OffsetDateTime::now_utc() - Duration::hours(hours))
        .format(&Rfc3339)
        .unwrap()
}

fn file_url(path: &Path) -> String {
    let canon = fs::canonicalize(path).unwrap();
    let shown = canon.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{}", shown.trim_start_matches('/'))
}

/// Response files the fake sources serve.
struct Responses {
    _dir: TempDir,
    asl_url: String,
    aa_url: String,
}

fn responses() -> Responses {
    let dir = tempfile::tempdir().unwrap();
    let asl = dir.path().join("asl.json");
    fs::write(&asl, r#"{"success":true,"data":[]}"#).unwrap();
    let aa = dir.path().join("aa");
    fs::create_dir(&aa).unwrap();
    fs::write(
        aa.join("page-1"),
        r#"{"pagination":{"has_more":false},"data":[]}"#,
    )
    .unwrap();
    Responses {
        asl_url: file_url(&asl),
        aa_url: format!("{}/page-", file_url(&aa)),
        _dir: dir,
    }
}

fn cache(at: &str) -> Value {
    json!({"schema":"aistupidlevel-v1","fetched_at":at,"url":"fixture","source":{"name":"AI Stupid Level","url":"https://aistupidlevel.info","license":"test"},"models":{"success":true,"data":[]}})
}

fn aa_cache(at: &str) -> Value {
    json!({"schema":"artificialanalysis-v2-free","fetched_at":at,"source":{"name":"Artificial Analysis","url":"https://artificialanalysis.ai"},"pages":1,"data":[]})
}

fn write_asl(f: &Fixture, at: &str) {
    fs::write(f.home_path().join("model-scores.json"), cache(at).to_string()).unwrap();
}

fn write_aa(f: &Fixture, at: &str) {
    fs::write(f.home_path().join("model-scores-aa.json"), aa_cache(at).to_string()).unwrap();
}

/// RJ-2: Agent Arena needs no key, so with the switch on it is due whenever
/// its cache is missing or a day old. The cases about the two keyed sources
/// write a fresh Agent Arena cache first, so it is not due and their
/// assertions stay about those two.
fn write_fresh_arena(f: &Fixture) {
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    fs::write(
        f.home_path().join("model-scores-arena.json"),
        json!({"schema":"agent-arena-v1","fetched_at":now,"sha":"fixture","source":{"name":"Agent Arena"},"boards":{"agent":{"publish_date":null,"rows":[]}}}).to_string(),
    )
    .unwrap();
}

fn asl_path(f: &Fixture) -> PathBuf {
    f.home_path().join("model-scores.json")
}

fn aa_path(f: &Fixture) -> PathBuf {
    f.home_path().join("model-scores-aa.json")
}

fn lock_path(f: &Fixture) -> PathBuf {
    f.home_path().join(LOCK)
}

fn home_names(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(f.home_path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_no_tmp_or_lock(f: &Fixture) {
    for name in home_names(f) {
        assert!(!name.ends_with(".tmp"), "leftover {name}");
        assert_ne!(name, LOCK, "leftover lock");
    }
}

/// Runs `models --json`; returns (exit ok, stdout JSON, stderr).
fn models_json(mut c: assert_cmd::Command) -> (Value, String, String) {
    c.args(["models", "--json"]);
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "models failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    (v, stdout, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn models_plain(f: &Fixture) -> String {
    let empty = empty_dir();
    let out = f
        .cmd()
        .env("PATH", empty.path())
        .arg("models")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

fn empty_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

fn lines_starting(text: &str, prefix: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

// --- AC1: the switch -----------------------------------------------------------

#[test]
fn s1_the_switch_is_the_second_setting_and_defaults_off() {
    let f = common::fixture();
    let before = home_names(&f);
    let out = f.cmd().args(["config", "list", "--json"]).assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 16, "{v}");
    assert_eq!(settings[1]["key"], "scores-auto-refresh");
    assert_eq!(settings[1]["value"], "off");
    assert_eq!(settings[1]["default"], "off");
    assert!(settings[1]["description"].as_str().unwrap().contains("once a day"));
    let out = f
        .cmd()
        .args(["config", "get", "scores-auto-refresh", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v, json!({"key": "scores-auto-refresh", "value": "off"}));
    assert_eq!(home_names(&f), before);
}

#[test]
fn s2_set_on_prints_the_notice_and_off_prints_none() {
    let f = common::fixture();
    let out = f
        .cmd()
        .args(["config", "set", "scores-auto-refresh", "on", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["previous"], "off");
    let notice = v["notice"].as_str().unwrap();
    for needle in [
        "aistupidlevel.info",
        "artificialanalysis.ai",
        "AISTUPIDLEVEL_API_KEY",
        "ARTIFICIALANALYSIS_API_KEY",
        "24 hours",
        "scores-auto-refresh off",
        // RJ-2: Agent Arena needs no key and is fetched from huggingface.co
        "huggingface.co",
        "recommend",
    ] {
        assert!(notice.contains(needle), "{needle} missing from {notice}");
    }
    let config: Value =
        serde_json::from_str(&fs::read_to_string(f.home_path().join("config.json")).unwrap()).unwrap();
    assert_eq!(config["scores_auto_refresh"], true);

    let out = f
        .cmd()
        .args(["config", "set", "scores-auto-refresh", "off", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["previous"], "on");
    assert!(v["notice"].is_null());

    let plain = f
        .cmd()
        .args(["config", "set", "scores-auto-refresh", "on"])
        .assert()
        .success();
    assert_eq!(
        String::from_utf8_lossy(&plain.get_output().stdout).trim(),
        "set scores-auto-refresh on"
    );
    assert!(String::from_utf8_lossy(&plain.get_output().stderr).contains("aistupidlevel.info"));
}

#[test]
fn s3_set_is_refused_inside_a_worker() {
    let f = common::fixture();
    f.cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["config", "set", "scores-auto-refresh", "on"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("THOTH_RUN_ID"));
    assert!(!f.home_path().join("config.json").exists());
}

// --- AC4: staleness ----------------------------------------------------------------

fn age_case(
    f: &Fixture,
    write: fn(&Fixture, &str),
    cache_of: fn(&Value) -> &Value,
    other_of: fn(&Value) -> &Value,
    name: &str,
    other_name: &str,
    hours: i64,
) {
    let key_name = if name == "AI Stupid Level" {
        "AISTUPIDLEVEL_API_KEY"
    } else {
        "ARTIFICIALANALYSIS_API_KEY"
    };
    write(f, &ago(hours));
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path());
    let (v, _, _) = models_json(c);
    assert_eq!(cache_of(&v)["age_hours"], hours, "{v}");
    assert_eq!(cache_of(&v)["stale"], hours >= 48, "{v}");
    assert!(other_of(&v)["age_hours"].is_null(), "{v}");
    assert_eq!(other_of(&v)["stale"], true, "{v}");
    let other_key_name = if other_name == "AI Stupid Level" {
        "AISTUPIDLEVEL_API_KEY"
    } else {
        "ARTIFICIALANALYSIS_API_KEY"
    };
    let plain = models_plain(f);
    assert!(
        lines_starting(&plain, &format!("stale: no {other_name} scores cached"))
            .iter()
            .any(|l| l.contains("thoth models --refresh")
                && l.contains(other_key_name)
                && l.contains("scores-auto-refresh on")),
        "{plain}"
    );
    if hours >= 48 {
        let lines = lines_starting(&plain, &format!("stale: {name} scores fetched {hours} hours ago"));
        assert_eq!(lines.len(), 1, "{plain}");
        assert!(lines[0].contains(key_name), "{plain}");
    } else {
        assert!(lines_starting(&plain, &format!("stale: {name}")).is_empty(), "{plain}");
        assert!(lines_starting(&plain, &format!("stale: no {name}")).is_empty(), "{plain}");
    }
}

fn asl_cache_of(v: &Value) -> &Value {
    &v["cache"]
}
fn aa_cache_of(v: &Value) -> &Value {
    &v["external"]["artificial_analysis"]["cache"]
}

#[test]
fn m_ai_stupid_level_staleness_in_json_and_text() {
    let f = setup();
    let empty = empty_dir();
    // The Artificial Analysis cache is missing throughout.
    age_case(&f, write_asl, asl_cache_of, aa_cache_of, "AI Stupid Level", "Artificial Analysis", 47);
    age_case(&f, write_asl, asl_cache_of, aa_cache_of, "AI Stupid Level", "Artificial Analysis", 49);
    fs::remove_file(asl_path(&f)).unwrap();
    let mut c = f.cmd();
    c.env("PATH", empty.path());
    let (v, _, _) = models_json(c);
    assert!(v["cache"]["age_hours"].is_null());
    assert_eq!(v["cache"]["stale"], true);
    let plain = models_plain(&f);
    let lines = lines_starting(&plain, "stale: no AI Stupid Level scores cached");
    assert_eq!(lines.len(), 1, "{plain}");
    assert!(lines[0].contains("AISTUPIDLEVEL_API_KEY"), "{plain}");
}

#[test]
fn m_artificial_analysis_staleness_in_json_and_text() {
    let f = setup();
    let empty = empty_dir();
    age_case(&f, write_aa, aa_cache_of, asl_cache_of, "Artificial Analysis", "AI Stupid Level", 47);
    age_case(&f, write_aa, aa_cache_of, asl_cache_of, "Artificial Analysis", "AI Stupid Level", 49);
    fs::remove_file(aa_path(&f)).unwrap();
    let mut c = f.cmd();
    c.env("PATH", empty.path());
    let (v, _, _) = models_json(c);
    assert!(aa_cache_of(&v)["age_hours"].is_null());
    assert_eq!(aa_cache_of(&v)["stale"], true);
    let plain = models_plain(&f);
    let lines = lines_starting(&plain, "stale: no Artificial Analysis scores cached");
    assert_eq!(lines.len(), 1, "{plain}");
    assert!(lines[0].contains("ARTIFICIALANALYSIS_API_KEY"), "{plain}");
}

// --- AC5: `thoth models` ---------------------------------------------------------

#[test]
fn a_refreshes_both_missing_caches_once() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    write_fresh_arena(&f);
    let run = |json: bool| {
        let mut c = f.cmd();
        c.env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
            .env("ARTIFICIALANALYSIS_API_KEY", AA_KEY)
            .env("THOTH_SCORES_URL", &r.asl_url)
            .env("THOTH_AA_URL", &r.aa_url);
        if json {
            c.args(["models", "--json"]);
        } else {
            c.arg("models");
        }
        c.output().unwrap()
    };
    let out = run(true);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let both = json!(["AI Stupid Level", "Artificial Analysis"]);
    assert_eq!(v["auto_refresh"]["enabled"], true);
    assert!(v["auto_refresh"]["skipped"].is_null());
    assert_eq!(v["auto_refresh"]["attempted"], both);
    assert_eq!(v["auto_refresh"]["refreshed"], both);
    assert_eq!(v["auto_refresh"]["failed"], json!([]));
    for c in [&v["cache"], &v["external"]["artificial_analysis"]["cache"]] {
        assert_eq!(c["present"], true, "{v}");
        assert_eq!(c["refreshed"], true);
        assert_eq!(c["stale"], false);
        assert_eq!(c["age_hours"], 0);
    }
    let asl: Value = serde_json::from_str(&fs::read_to_string(asl_path(&f)).unwrap()).unwrap();
    assert_eq!(asl["schema"], "aistupidlevel-v1");
    let aa: Value = serde_json::from_str(&fs::read_to_string(aa_path(&f)).unwrap()).unwrap();
    assert_eq!(aa["schema"], "artificialanalysis-v2-free");
    let mut seen = String::from_utf8_lossy(&out.stdout).into_owned();
    seen.push_str(&String::from_utf8_lossy(&out.stderr));
    seen.push_str(&fs::read_to_string(asl_path(&f)).unwrap());
    seen.push_str(&fs::read_to_string(aa_path(&f)).unwrap());
    assert!(!seen.contains(ASL_KEY) && !seen.contains(AA_KEY));
    assert_no_tmp_or_lock(&f);

    fs::remove_file(asl_path(&f)).unwrap();
    fs::remove_file(aa_path(&f)).unwrap();
    let out = run(false);
    assert!(out.status.success());
    let plain = String::from_utf8(out.stdout).unwrap();
    assert!(plain.lines().any(|l| l == "auto-refresh: AI Stupid Level, Artificial Analysis"), "{plain}");
    assert!(lines_starting(&plain, "stale:").is_empty(), "{plain}");

    let out = run(true);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert_eq!(v["cache"]["refreshed"], false);
}

#[test]
fn b_switch_off_never_fetches() {
    let f = setup();
    let r = responses();
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path())
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("ARTIFICIALANALYSIS_API_KEY", AA_KEY)
        .env("THOTH_SCORES_URL", &r.asl_url)
        .env("THOTH_AA_URL", &r.aa_url);
    let (v, _, stderr) = models_json(c);
    assert_eq!(v["auto_refresh"]["enabled"], false);
    assert_eq!(v["auto_refresh"]["skipped"], "off");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert!(!asl_path(&f).exists() && !aa_path(&f).exists());
    assert_eq!(stderr, "");
}

/// RJ-2: without keys only Agent Arena (no key needed) is attempted; the two
/// keyed caches never appear. curl cannot start here, so it fails and warns.
#[test]
fn c_no_keys_means_only_agent_arena_is_attempted() {
    let f = setup();
    switch(&f, "on");
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path());
    let (v, _, stderr) = models_json(c);
    assert_eq!(v["auto_refresh"]["enabled"], true);
    assert!(v["auto_refresh"]["skipped"].is_null());
    assert_eq!(v["auto_refresh"]["attempted"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["refreshed"], json!([]), "{v}");
    let failed = v["auto_refresh"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{v}");
    assert_eq!(failed[0]["source"], "Agent Arena");
    assert!(stderr.contains("automatic score refresh failed") && stderr.contains("Agent Arena"), "{stderr}");
    assert!(!asl_path(&f).exists() && !aa_path(&f).exists() && !lock_path(&f).exists());
    assert!(!f.home_path().join("model-scores-arena.json").exists());
}

#[test]
fn d_asl_refreshes_only_after_24_hours() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    write_fresh_arena(&f);
    write_asl(&f, &ago(23));
    let before = fs::read(asl_path(&f)).unwrap();
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path())
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", &r.asl_url);
    let (v, _, _) = models_json(c);
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert_eq!(v["cache"]["refreshed"], false);
    assert_eq!(fs::read(asl_path(&f)).unwrap(), before);

    let old_at = ago(25);
    write_asl(&f, &old_at);
    let mut c = f.cmd();
    c.env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", &r.asl_url);
    let (v, _, _) = models_json(c);
    assert_eq!(v["auto_refresh"]["attempted"], json!(["AI Stupid Level"]));
    assert_eq!(v["cache"]["refreshed"], true);
    assert_eq!(v["cache"]["age_hours"], 0);
    let new: Value = serde_json::from_str(&fs::read_to_string(asl_path(&f)).unwrap()).unwrap();
    assert!(new["fetched_at"].as_str().unwrap() > old_at.as_str());
}

#[test]
fn d2_aa_refreshes_only_after_24_hours() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    write_fresh_arena(&f);
    write_aa(&f, &ago(23));
    let before = fs::read(aa_path(&f)).unwrap();
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path())
        .env("ARTIFICIALANALYSIS_API_KEY", AA_KEY)
        .env("THOTH_AA_URL", &r.aa_url);
    let (v, _, _) = models_json(c);
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert_eq!(aa_cache_of(&v)["refreshed"], false);
    assert_eq!(fs::read(aa_path(&f)).unwrap(), before);

    write_aa(&f, &ago(25));
    let mut c = f.cmd();
    c.env("ARTIFICIALANALYSIS_API_KEY", AA_KEY)
        .env("THOTH_AA_URL", &r.aa_url);
    let (v, _, _) = models_json(c);
    assert_eq!(v["auto_refresh"]["attempted"], json!(["Artificial Analysis"]));
    assert_eq!(aa_cache_of(&v)["refreshed"], true);
}

#[test]
fn e_a_failed_refresh_keeps_the_old_cache_and_warns_once() {
    let f = setup();
    switch(&f, "on");
    write_fresh_arena(&f);
    write_asl(&f, &ago(25));
    let before = fs::read(asl_path(&f)).unwrap();
    let mut c = f.cmd();
    c.env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", "file:///definitely/missing");
    let (v, stdout, stderr) = models_json(c);
    assert_eq!(v["cache"]["present"], true);
    assert_eq!(v["cache"]["refreshed"], false);
    assert_eq!(v["cache"]["stale"], false);
    assert_eq!(v["auto_refresh"]["attempted"], json!(["AI Stupid Level"]));
    assert_eq!(v["auto_refresh"]["refreshed"], json!([]));
    let failed = v["auto_refresh"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{v}");
    assert_eq!(failed[0]["source"], "AI Stupid Level");
    assert!(failed[0]["error"].as_str().unwrap().contains("file:///definitely/missing"));
    let warnings = lines_starting(&stderr, "warning: automatic score refresh failed");
    assert_eq!(
        stderr.lines().filter(|l| l.contains("automatic score refresh failed")).count(),
        1,
        "{stderr}"
    );
    assert!(warnings[0].contains("AI Stupid Level"));
    assert!(warnings[0].contains("keeping the cached scores"));
    assert!(warnings[0].contains("thoth models --refresh"));
    assert!(!stderr.contains(ASL_KEY));
    assert!(!stdout.contains("warning"));
    assert_eq!(fs::read(asl_path(&f)).unwrap(), before);
    assert_no_tmp_or_lock(&f);
}

#[test]
fn f_inside_a_worker_nothing_is_fetched() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    let empty = empty_dir();
    let mut c = f.cmd();
    c.env("PATH", empty.path())
        .env("THOTH_RUN_ID", "01RUN")
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", &r.asl_url);
    let (v, _, _) = models_json(c);
    assert_eq!(v["auto_refresh"]["skipped"], "worker");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert!(!asl_path(&f).exists());
}

#[test]
fn j_a_fresh_lock_blocks_and_a_stale_lock_is_replaced() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    write_fresh_arena(&f);
    let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
    fs::write(lock_path(&f), &now).unwrap();
    let fetching = |f: &Fixture| {
        let mut c = f.cmd();
        c.env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
            .env("THOTH_SCORES_URL", &r.asl_url);
        models_json(c).0
    };
    let v = fetching(&f);
    assert_eq!(v["auto_refresh"]["skipped"], "busy");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert!(!asl_path(&f).exists());
    assert_eq!(fs::read_to_string(lock_path(&f)).unwrap(), now);

    // An empty lock (the instant between create and write) is not stale.
    fs::write(lock_path(&f), "").unwrap();
    let v = fetching(&f);
    assert_eq!(v["auto_refresh"]["skipped"], "busy", "{v}");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
    assert!(!asl_path(&f).exists());
    assert!(lock_path(&f).exists());

    // Staleness is the file's modification time, not its content.
    let file = fs::File::options().write(true).open(lock_path(&f)).unwrap();
    file.set_modified(SystemTime::now() - std::time::Duration::from_secs(600))
        .unwrap();
    drop(file);
    let v = fetching(&f);
    assert!(v["auto_refresh"]["skipped"].is_null(), "{v}");
    assert_eq!(v["auto_refresh"]["attempted"], json!(["AI Stupid Level"]));
    assert!(asl_path(&f).exists());
    assert!(!lock_path(&f).exists());
}

#[test]
fn k_manual_refresh_is_unchanged() {
    let f = setup();
    switch(&f, "on");
    write_asl(&f, &OffsetDateTime::now_utc().format(&Rfc3339).unwrap());
    let empty = empty_dir();
    let out = f
        .cmd()
        .env("PATH", empty.path())
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .args(["models", "--refresh"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("updates roughly hourly"));
    let mut c = f.cmd();
    c.env("PATH", empty.path()).env("AISTUPIDLEVEL_API_KEY", ASL_KEY);
    c.args(["models", "--refresh", "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["cache"]["refreshed"], false);
    assert_eq!(v["auto_refresh"]["skipped"], "manual");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]));
}

// --- AC6: the other commands -------------------------------------------------------

fn codex_with_history(f: &Fixture, id: &str) -> PathBuf {
    let cfg = f.add_codex_profile(id);
    fs::write(
        cfg.join("models_cache.json"),
        json!({"models":[{"slug":CODEX_MODEL,"display_name":CODEX_MODEL,"visibility":"list","priority":1,"supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]}]})
            .to_string(),
    )
    .unwrap();
    f.state_conn()
        .execute(
            "INSERT INTO runs(run_id,role,account,model,status,ended_at) VALUES(?1,'implementer',?2,?3,'completed','2026-09-30T01:00:00Z')",
            params![format!("PAST-{id}"), id, CODEX_MODEL],
        )
        .unwrap();
    cfg
}

fn write_aa_scores(f: &Fixture) {
    fs::write(
        aa_path(f),
        json!({"schema":"artificialanalysis-v2-free","fetched_at":"2026-09-30T00:00:00Z","data":[
            {"name":format!("{CODEX_MODEL} (high)"),"evaluations":{"artificial_analysis_intelligence_index":60.0,"artificial_analysis_coding_index":60.0},"pricing":{"price_1m_output_tokens":4.0}}]})
            .to_string(),
    )
    .unwrap();
}

/// A command with the fakes' scenario and the fake ASL key and URL.
fn scored(f: &Fixture, r: &Responses, scenario: &str, task: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", &r.asl_url);
    c
}

#[test]
fn g2_report_rewrites_and_the_runner_never_fetch() {
    let f = setup();
    let r = responses();
    codex_with_history(&f, "codex-a");
    write_aa_scores(&f);
    let (task, _) = f.create_task(Fixture::SPEC);
    f.cmd().arg("report").assert().success();
    let report = f.home_path().join("report.html");
    let before = fs::read(&report).unwrap();

    switch(&f, "on");
    assert!(!asl_path(&f).exists());
    let other = f.home_path().join("g2-spec.json");
    fs::write(&other, Fixture::SPEC).unwrap();
    let mut c = scored(&f, &r, "ok", &task);
    c.args(["task", "create", "-f"])
        .arg(&other)
        .args(["--title", "g2", "--json"])
        .assert()
        .success();
    assert_ne!(fs::read(&report).unwrap(), before, "the report was not rewritten");
    assert!(!asl_path(&f).exists());

    let mut c = scored(&f, &r, "ok", &task);
    c.args(["dispatch", &task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    f.run_ok(c, "dispatch");
    // RJ-1: a dispatch reads no scores, so it never fetches
    assert!(!asl_path(&f).exists());
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    assert!(!asl_path(&f).exists());
}

/// RJ-2: `recommend` sends the external tables, so the day's first call
/// fetches a due source with or without `--task` (the switch on); a failure
/// warns once and keeps the exit code; the switch off fetches nothing.
#[test]
fn h_recommend_fetches_a_due_source_with_or_without_a_task() {
    let f = setup();
    let r = responses();
    codex_with_history(&f, "codex-a");
    write_aa_scores(&f);
    write_fresh_arena(&f);
    let (task, _) = f.create_task(Fixture::SPEC);
    switch(&f, "on");
    let mut c = scored(&f, &r, "ok", &task);
    c.args(["recommend", "--role", "implementer", "--task", &task, "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let good: Value = serde_json::from_slice(&out).unwrap();
    assert!(good.get("candidates").is_some(), "{good}");
    assert_eq!(good["score_refresh"]["enabled"], true, "{good}");
    assert_eq!(good["score_refresh"]["refreshed"], json!(["AI Stupid Level"]), "{good}");
    assert!(asl_path(&f).exists());

    fs::remove_file(asl_path(&f)).unwrap();
    let mut c = scored(&f, &r, "ok", &task);
    c.args(["recommend", "--role", "implementer", "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let good: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(good["score_refresh"]["refreshed"], json!(["AI Stupid Level"]), "{good}");
    assert!(asl_path(&f).exists(), "without --task too");

    fs::remove_file(asl_path(&f)).unwrap();
    let mut c = scored(&f, &r, "ok", &task);
    c.env("THOTH_SCORES_URL", "file:///definitely/missing");
    c.args(["recommend", "--role", "implementer", "--task", &task, "--json"]);
    let out = c.output().unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.lines().filter(|l| l.contains("automatic score refresh failed")).count(), 1, "{stderr}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["score_refresh"]["failed"][0]["source"], "AI Stupid Level", "{v}");
    assert!(!asl_path(&f).exists());

    switch(&f, "off");
    let mut c = scored(&f, &r, "ok", &task);
    c.args(["recommend", "--role", "implementer", "--task", &task, "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["score_refresh"]["enabled"], false, "{v}");
    assert_eq!(v["score_refresh"]["skipped"], "off", "{v}");
    assert!(!asl_path(&f).exists());
}

#[test]
fn i_report_fetches_before_it_reads() {
    let f = setup();
    let r = responses();
    switch(&f, "on");
    let mut c = scored(&f, &r, "ok", "NONE");
    c.args(["report", "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert!(v.is_object(), "{v}");
    assert!(asl_path(&f).exists());
    assert!(f.home_path().join("report.html").exists());
}

// --- AC6 (n): MCP ------------------------------------------------------------------------

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture, envs: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("FAKE_TASK_ID")
            // as `Fixture::cmd` does: a machine with real score keys must not
            // send them, and only the keys a test passes count
            .env_remove("ARTIFICIALANALYSIS_API_KEY")
            .env_remove("AISTUPIDLEVEL_API_KEY")
            // RJ-2: Agent Arena needs no key; keep it off the network too
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn n_the_mcp_models_tool_refreshes_through_its_child_process() {
    let f = setup();
    let r = responses();
    write_fresh_arena(&f);
    fs::write(f.home_path().join("config.json"), r#"{"scores_auto_refresh":true}"#).unwrap();
    let mut s = Server::start(
        &f,
        &[
            ("AISTUPIDLEVEL_API_KEY", ASL_KEY),
            ("THOTH_SCORES_URL", &r.asl_url),
        ],
    );
    let response = s.call(1, "thoth_models", json!({}));
    assert_eq!(response["result"]["isError"], false, "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["auto_refresh"]["attempted"], json!(["AI Stupid Level"]), "{response}");
    assert_eq!(content["cache"]["refreshed"], true);
    assert!(asl_path(&f).exists());
}
