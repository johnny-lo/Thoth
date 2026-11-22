//! SMALL-11 (6): after the automatic score refresh of a source fails, the
//! next hour of `models`, `report` and `recommend` calls does not try that
//! source again and prints no new warning. `models --refresh` still fetches
//! every source, and its success ends that source's back-off.
//! Every URL is a `file://` path; no test reaches the network.

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Output;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const ASL: &str = "AI Stupid Level";
const ARENA: &str = "Agent Arena";
const ASL_KEY: &str = "asl-fake-key-1";
const WARNING: &str = "automatic score refresh failed";

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd().args(["config", "set", "scores-auto-refresh", "on"]).assert().success();
    f
}

/// On Windows the canonical path starts with the verbatim prefix `\\?\`,
/// which is not part of a file URL: it is stripped before the URL is built.
fn file_url(path: &Path) -> String {
    let canon = fs::canonicalize(path).unwrap();
    let shown = canon.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{}", shown.trim_start_matches('/'))
}

/// A `file://` URL that serves the AI Stupid Level fixture.
fn serving(f: &Fixture) -> String {
    let file = f.home_path().join("asl-fixture.json");
    fs::write(&file, r#"{"success":true,"data":[]}"#).unwrap();
    file_url(&file)
}

fn missing(f: &Fixture) -> String {
    format!("file:///{}", f.home_path().join("nothing-here.json").to_string_lossy().replace('\\', "/").trim_start_matches('/'))
}

/// `thoth ARGS --json` with the extra environment; panics unless it exits 0.
fn run(f: &Fixture, args: &[&str], env: &[(&str, &str)]) -> (Value, String) {
    let mut c = f.cmd();
    c.args(args).arg("--json");
    for (k, v) in env {
        c.env(k, v);
    }
    let out: Output = c.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{args:?}: {stderr}");
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    (v, stderr)
}

fn warnings(stderr: &str) -> usize {
    stderr.lines().filter(|l| l.contains(WARNING)).count()
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect()
}

fn backed_off(auto: &Value) -> Vec<&str> {
    auto["backed_off"].as_array().unwrap().iter().map(|b| b["source"].as_str().unwrap()).collect()
}

fn instant(v: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(v.as_str().unwrap(), &Rfc3339).unwrap()
}

#[test]
fn a_failed_source_is_left_alone_and_silent_for_the_hour_until_the_test_seam_shortens_it() {
    let f = setup();
    let (v, stderr) = run(&f, &["models"], &[]);
    let auto = &v["auto_refresh"];
    assert_eq!(names(&auto["attempted"]), vec![ARENA], "{v}");
    assert_eq!(auto["failed"][0]["source"], ARENA, "{v}");
    assert_eq!(auto["backed_off"], serde_json::json!([]), "{v}");
    assert_eq!(warnings(&stderr), 1, "{stderr}");

    let (v, stderr) = run(&f, &["models"], &[]);
    let auto = &v["auto_refresh"];
    assert_eq!(auto["attempted"], serde_json::json!([]), "{v}");
    assert_eq!(auto["failed"], serde_json::json!([]), "{v}");
    assert_eq!(backed_off(auto), vec![ARENA], "{v}");
    let (failed_at, retry_after) = (instant(&auto["backed_off"][0]["failed_at"]), instant(&auto["backed_off"][0]["retry_after"]));
    assert_eq!((retry_after - failed_at).whole_seconds(), 3600, "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");
    // the cache is as it was: still none, so its age is not hidden
    let arena = &v["external"]["agent_arena"]["cache"];
    assert_eq!(arena["present"], false, "{v}");
    assert_eq!(arena["fetched_at"], Value::Null, "{v}");

    let first_failed_at = instant(&auto["backed_off"][0]["failed_at"]);

    std::thread::sleep(std::time::Duration::from_secs(2));
    let (v, stderr) = run(&f, &["models"], &[("THOTH_TEST_SCORE_RETRY_SECS", "1")]);
    let auto = &v["auto_refresh"];
    assert_eq!(names(&auto["attempted"]), vec![ARENA], "{v}");
    assert_eq!(auto["failed"][0]["source"], ARENA, "{v}");
    assert_eq!(warnings(&stderr), 1, "{stderr}");

    // the new failure starts a new back-off
    let (v, stderr) = run(&f, &["models"], &[]);
    let auto = &v["auto_refresh"];
    assert_eq!(auto["attempted"], serde_json::json!([]), "{v}");
    assert_eq!(backed_off(auto), vec![ARENA], "{v}");
    assert!(instant(&auto["backed_off"][0]["failed_at"]) > first_failed_at, "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");
}

#[test]
fn a_keyed_source_backs_off_on_its_own_and_a_manual_refresh_fetches_it_anyway() {
    let f = setup();
    let key = [("AISTUPIDLEVEL_API_KEY", ASL_KEY)];
    let good = serving(&f);
    let gone = missing(&f);

    // Agent Arena fails first (no key for the other source yet)
    let (v, _) = run(&f, &["models"], &[]);
    assert_eq!(v["auto_refresh"]["failed"][0]["source"], ARENA, "{v}");

    // AI Stupid Level fails by itself; Agent Arena is held
    let (v, stderr) = run(&f, &["models"], &[key[0], ("THOTH_SCORES_URL", &gone)]);
    let auto = &v["auto_refresh"];
    assert_eq!(names(&auto["attempted"]), vec![ASL], "{v}");
    assert_eq!(auto["failed"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(auto["failed"][0]["source"], ASL, "{v}");
    assert_eq!(backed_off(auto), vec![ARENA], "{v}");
    assert_eq!(warnings(&stderr), 1, "{stderr}");
    assert!(stderr.contains(ASL) && !stderr.contains(ARENA), "{stderr}");

    // the source now answers, but its hour is not over: both are held
    let (v, stderr) = run(&f, &["models"], &[key[0], ("THOTH_SCORES_URL", &good)]);
    let auto = &v["auto_refresh"];
    assert_eq!(auto["attempted"], serde_json::json!([]), "{v}");
    assert_eq!(backed_off(auto), vec![ASL, ARENA], "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");
    assert_eq!(v["cache"]["present"], false, "{v}");

    // models --refresh fetches every source during the hour ...
    let (v, _) = run(&f, &["models", "--refresh"], &[key[0], ("THOTH_SCORES_URL", &good)]);
    assert_eq!(v["cache"]["refreshed"], true, "{v}");
    assert_eq!(v["cache"]["present"], true, "{v}");
    // ... and its success ends that source's back-off, not Agent Arena's
    // (its fetch failed again)
    let (v, stderr) = run(&f, &["models"], &[key[0], ("THOTH_SCORES_URL", &good)]);
    let auto = &v["auto_refresh"];
    assert_eq!(backed_off(auto), vec![ARENA], "{v}");
    assert_eq!(auto["attempted"], serde_json::json!([]), "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");
}

#[test]
fn recommend_shows_the_back_off_and_recommend_and_report_stay_silent() {
    let f = setup();
    f.add_profile("claude-b");
    let (v, _) = run(&f, &["models"], &[]);
    assert_eq!(v["auto_refresh"]["failed"][0]["source"], ARENA, "{v}");

    let (v, stderr) = run(&f, &["recommend", "--role", "implementer"], &[]);
    let refresh = &v["score_refresh"];
    assert_eq!(refresh["attempted"], serde_json::json!([]), "{v}");
    assert_eq!(backed_off(refresh), vec![ARENA], "{v}");
    assert!(refresh["backed_off"][0]["failed_at"].is_string(), "{v}");
    assert!(refresh["backed_off"][0]["retry_after"].is_string(), "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");

    // `report --json` has no score_refresh key; the back-off shows as silence
    let (_, stderr) = run(&f, &["report"], &[]);
    assert_eq!(warnings(&stderr), 0, "{stderr}");
}

// SMALL-12 (3): `report --json` says only where the page was written; the
// back-off shows in models' auto_refresh and recommend's score_refresh.
#[test]
fn report_json_has_only_path_and_generated_at_during_a_back_off() {
    let f = setup();
    let (v, _) = run(&f, &["models"], &[]);
    assert_eq!(v["auto_refresh"]["failed"][0]["source"], ARENA, "{v}");

    let (v, stderr) = run(&f, &["report"], &[]);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["generated_at", "path"], "{v}");
    assert_eq!(warnings(&stderr), 0, "{stderr}");

    let (v, _) = run(&f, &["models"], &[]);
    assert_eq!(backed_off(&v["auto_refresh"]), vec![ARENA], "{v}");
}
