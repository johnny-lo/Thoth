//! MEAS-0: every review start records whether the reviewer shares the
//! implementer's provider; `review show --json` carries the same flag, and
//! `thoth retro` splits its counts into same-provider, cross-provider and
//! unknown reviews. Everything is read through the CLI.

mod common;

use common::Fixture;
use serde_json::Value;

/// A provider no other profile in this file uses, for the cross-provider
/// reviewer (still the claude-code harness, so fake-claude runs it).
const OTHER_PROVIDER: &str = "acme";

fn json_of(f: &Fixture, args: &[&str], what: &str) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, what);
    serde_json::from_str(String::from_utf8_lossy(&out).trim())
        .unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {}", String::from_utf8_lossy(&out)))
}

/// A new task run to `in_review` by claude-b's `hooks` scenario.
fn task_in_review(f: &Fixture) -> String {
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task, "claude-b", "hooks", &[]);
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    task
}

fn show(f: &Fixture, task: &str) -> Value {
    json_of(f, &["review", "show", task, "--json"], "review show")
}

/// The `review_started` event of `run`, from `events --since 0 --task TASK --json`.
fn started_event(f: &Fixture, task: &str, run: &str) -> Value {
    f.events_json(task)
        .into_iter()
        .find(|e| e["type"] == "review_started" && e["run_id"] == run)
        .unwrap_or_else(|| panic!("no review_started event for {run}\n{}", f.explain_task(task)))
}

/// Starts a review of `task` by `profile` with `scenario`, waits for it, and
/// returns (`review start --json`, `wait --run --json`).
fn review(f: &Fixture, task: &str, profile: &str, scenario: &str) -> (Value, Value) {
    let started = f.review_start(task, profile, scenario, &["--reason", "MEAS-0 test"]);
    let run = started["run_id"].as_str().unwrap_or_else(|| panic!("no run_id: {started}")).to_string();
    let waited = json_of(f, &["wait", "--run", &run, "--timeout", "120s", "--json"], "wait --run");
    (started, waited)
}

/// One test for the whole file: it shares one fixture with three implementer
/// runs and two reviewer runs.
#[test]
fn reviews_record_and_count_same_and_cross_provider() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_profile_with("claude-x", OTHER_PROVIDER, "claude-code");

    let profiles = json_of(&f, &["profile", "list", "--json"], "profile list");
    let provider = |id: &str| -> String {
        profiles["profiles"]
            .as_array()
            .unwrap_or_else(|| panic!("no profiles array: {profiles}"))
            .iter()
            .find(|p| p["profile_id"] == id)
            .and_then(|p| p["provider"].as_str())
            .unwrap_or_else(|| panic!("no provider for {id}: {profiles}"))
            .to_string()
    };
    let implementer_provider = provider("claude-b");
    assert_eq!(implementer_provider, provider("claude-a"), "claude-a is the same-provider reviewer: {profiles}");
    assert_ne!(implementer_provider, provider("claude-x"), "claude-x is the cross-provider reviewer: {profiles}");

    // (a) no review yet: the key is there, null.
    let same_task = task_in_review(&f);
    let before = show(&f, &same_task);
    assert!(before.as_object().unwrap().contains_key("same_provider_as_implementer"), "{before}");
    assert_eq!(before["same_provider_as_implementer"], Value::Null, "{before}");
    assert_eq!(before["review_count"], 0, "{before}");

    // (b) same provider: claude-b implemented, claude-a reviews.
    let (started, same_wait) = review(&f, &same_task, "claude-a", "review_reject");
    let run = started["run_id"].as_str().unwrap();
    let event = started_event(&f, &same_task, run);
    for (what, v) in [("review start --json", &started), ("review_started detail", &event["detail"])] {
        assert_eq!(v["implementer_provider"], implementer_provider.as_str(), "{what}: {v}");
        assert_eq!(v["same_provider_as_implementer"], true, "{what}: {v}");
    }
    let shown = show(&f, &same_task);
    assert_eq!(shown["reviewer_run_id"], run, "{shown}");
    assert_eq!(shown["same_provider_as_implementer"], true, "{shown}");

    // (c) cross provider: claude-b implemented, claude-x reviews.
    let cross_task = task_in_review(&f);
    let (started, cross_wait) = review(&f, &cross_task, "claude-x", "review_pass");
    let run = started["run_id"].as_str().unwrap();
    let event = started_event(&f, &cross_task, run);
    for (what, v) in [("review start --json", &started), ("review_started detail", &event["detail"])] {
        assert_eq!(v["implementer_provider"], implementer_provider.as_str(), "{what}: {v}");
        assert_eq!(v["same_provider_as_implementer"], false, "{what}: {v}");
    }
    let shown = show(&f, &cross_task);
    assert_eq!(shown["reviewer_run_id"], run, "{shown}");
    assert_eq!(shown["same_provider_as_implementer"], false, "{shown}");

    // (d) an external review has no reviewer run: null.
    let external_task = task_in_review(&f);
    let file = f.home_path().join("external-review.json");
    std::fs::write(
        &file,
        r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"the greeting is missing","fix_size":"S","category":"docs"},
            {"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#,
    )
    .unwrap();
    let file_arg = file.display().to_string();
    let recorded = json_of(&f, &["review", "record", &external_task, "-f", &file_arg, "--json"], "review record");
    let shown = show(&f, &external_task);
    assert_eq!(shown["review_id"], recorded["review_id"], "{shown}\n{recorded}");
    assert!(shown.as_object().unwrap().contains_key("same_provider_as_implementer"), "{shown}");
    assert_eq!(shown["same_provider_as_implementer"], Value::Null, "{shown}");

    // (e) retro, from outside any repo, splits the three reviews.
    let elsewhere = tempfile::tempdir().unwrap();
    let mut c = f.cmd_in(elsewhere.path());
    c.args(["retro", "--json"]);
    let out = f.run_ok(c, "retro --json");
    let v: Value = serde_json::from_slice(&out).unwrap();
    let split = &v["provider_split"];
    let mut sides: Vec<String> = split.as_object().unwrap_or_else(|| panic!("{v}")).keys().cloned().collect();
    sides.sort();
    assert_eq!(sides, ["cross_provider", "same_provider", "unknown"], "{v}");
    let expected = [
        ("same_provider", &same_wait["verdict"]),
        ("cross_provider", &cross_wait["verdict"]),
        ("unknown", &recorded["verdict"]),
    ];
    for (side, verdict) in expected {
        let g = &split[side];
        assert_eq!(g["reviews"], 1, "{side}: {v}");
        let verdicts = g["verdicts"].as_object().unwrap_or_else(|| panic!("{side}: {v}"));
        let mut names: Vec<&String> = verdicts.keys().collect();
        names.sort();
        assert_eq!(names, ["PASS", "REJECT", "UNVERIFIED"], "{side}: {v}");
        let verdict = verdict.as_str().unwrap_or_else(|| panic!("{side}: no verdict to expect"));
        for (name, n) in verdicts {
            assert_eq!(n, if name == verdict { 1 } else { 0 }, "{side} {name}: {v}");
        }
    }
    let sum = |key: &str| -> u64 { expected.iter().map(|(side, _)| split[side][key].as_u64().unwrap()).sum() };
    assert_eq!(sum("reviews"), v["reviews"].as_u64().unwrap(), "{v}");
    assert_eq!(sum("findings"), v["findings"].as_u64().unwrap(), "{v}");
    let top = v["categories"].as_array().unwrap();
    assert!(!top.is_empty(), "the REJECTs give findings: {v}");
    for cat in top {
        let name = &cat["category"];
        let count: u64 = expected
            .iter()
            .map(|(side, _)| {
                split[side]["categories"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| &c["category"] == name)
                    .map_or(0, |c| c["count"].as_u64().unwrap())
            })
            .sum();
        assert_eq!(count, cat["count"].as_u64().unwrap(), "{name}: {v}");
    }

    // The human retro of the same state shows each side's numbers.
    let mut c = f.cmd_in(elsewhere.path());
    c.arg("retro");
    let human = String::from_utf8(f.run_ok(c, "retro")).unwrap();
    for (side, _) in expected {
        let g = &split[side];
        let line = human
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{side}:")))
            .unwrap_or_else(|| panic!("no {side} line:\n{human}"));
        assert!(line.contains(&format!("{} reviews", g["reviews"])), "{line}");
        for name in ["PASS", "REJECT", "UNVERIFIED"] {
            assert!(line.contains(&format!("{name} {}", g["verdicts"][name])), "{line}");
        }
        assert!(line.contains(&format!("{} findings", g["findings"])), "{line}");
    }
}
