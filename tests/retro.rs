mod common;

use common::Fixture;

/// Runs the `hooks` implementer scenario on a new task until it is
/// `in_review`, and returns the task id.
fn task_in_review(f: &Fixture) -> String {
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    task_id
}

/// Records `review_json` for `task_id` through `thoth review record`.
fn record(f: &Fixture, task_id: &str, review_json: &str) {
    let file = f.home_path().join(format!("review-{task_id}.json"));
    std::fs::write(&file, review_json).unwrap();
    f.cmd()
        .args(["review", "record", task_id, "-f"])
        .arg(&file)
        .assert()
        .success();
}

/// H10: two tasks rejected for the same category make that category a
/// candidate for a check; a finding without a category is unclassified.
#[test]
fn retro_groups_recorded_findings_by_category_across_tasks() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let first = task_in_review(&f);
    let second = task_in_review(&f);

    record(&f, &first, r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the test reads the database","fix_size":"S","category":"test_interface"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#);
    record(&f, &second, r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the test only checks the exit code","fix_size":"M","category":"test_interface"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}],
        "extra_findings":[{"severity":"minor","note":"a stray TODO","evidence":["E001"]}]}}"#);

    // Run from outside any repo: retro reads every registered repo.
    let elsewhere = tempfile::tempdir().unwrap();
    let out = f
        .cmd()
        .current_dir(elsewhere.path())
        .args(["retro", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["days"], 7, "{v}");
    assert_eq!(v["reviews"], 2, "{v}");
    assert_eq!(v["findings"], 3, "{v}");
    assert_eq!(v["sizes"], serde_json::json!({"S": 1, "M": 1, "L": 0, "unsized": 1}), "{v}");

    let top = &v["categories"][0];
    assert_eq!(top["category"], "test_interface", "{v}");
    assert_eq!(top["count"], 2, "{v}");
    assert_eq!(top["task_count"], 2, "{v}");
    assert_eq!(top["suggest_check"], true, "{v}");
    assert_eq!(top["sizes"], serde_json::json!({"S": 1, "M": 1, "L": 0, "unsized": 0}), "{v}");
    let tasks: Vec<&str> = top["tasks"].as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap()).collect();
    assert_eq!(tasks, vec![first.as_str(), second.as_str()], "{v}");
    assert_eq!(top["findings"][0]["source"], "criterion", "{v}");
    assert_eq!(top["findings"][0]["id"], "AC1", "{v}");
    assert_eq!(top["findings"][0]["fix_size"], "S", "{v}");

    // The AC7 contract: every key, and the values that come from the task and
    // the recorded review.
    let keys = |o: &serde_json::Value| {
        let mut k: Vec<String> = o.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let sorted = |names: &[&str]| {
        let mut k: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(&v), sorted(&["days", "since", "reviews", "findings", "sizes", "categories"]), "{v}");
    assert_eq!(
        keys(top),
        sorted(&["category", "count", "task_count", "tasks", "sizes", "suggest_check", "findings"]),
        "{v}"
    );
    assert_eq!(keys(&top["tasks"][0]), sorted(&["repo", "task_id", "title"]), "{v}");
    assert_eq!(
        keys(&top["findings"][0]),
        sorted(&["repo", "task_id", "review_id", "created_at", "source", "id", "severity", "fix_size", "note"]),
        "{v}"
    );
    let title = |task_id: &str| {
        let out = f.cmd().args(["status", task_id, "--json"]).assert().success().get_output().stdout.clone();
        let s: serde_json::Value = serde_json::from_slice(&out).unwrap();
        s["title"].as_str().unwrap_or_else(|| panic!("no title in {s}")).to_string()
    };
    assert_eq!(top["tasks"][0]["title"], title(&first), "{v}");
    let same_dir = |a: &str| std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(f.repo_path()).unwrap();
    assert!(same_dir(top["tasks"][0]["repo"].as_str().unwrap()), "{v}");
    assert!(same_dir(top["findings"][0]["repo"].as_str().unwrap()), "{v}");
    assert_eq!(top["findings"][0]["task_id"], first.as_str(), "{v}");
    assert_eq!(top["findings"][0]["note"], "the test reads the database", "{v}");
    assert!(top["findings"][0]["created_at"].as_str().unwrap().ends_with('Z'), "{v}");
    let shown = f.cmd().args(["review", "show", &first, "--json"]).assert().success().get_output().stdout.clone();
    let shown: serde_json::Value = serde_json::from_slice(&shown).unwrap();
    assert_eq!(top["findings"][0]["review_id"], shown["review_id"], "{v}\n{shown}");

    let unclassified = v["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["category"] == "unclassified")
        .expect("an unclassified group");
    assert_eq!(unclassified["count"], 1, "{v}");
    assert_eq!(unclassified["suggest_check"], false, "{v}");
    assert_eq!(unclassified["findings"][0]["source"], "extra", "{v}");
    assert_eq!(unclassified["findings"][0]["id"], serde_json::Value::Null, "{v}");
    assert_eq!(unclassified["findings"][0]["severity"], "minor", "{v}");
    assert_eq!(unclassified["findings"][0]["fix_size"], serde_json::Value::Null, "{v}");

    let human = f
        .cmd()
        .current_dir(elsewhere.path())
        .arg("retro")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("test_interface"), "{human}");
    assert!(human.contains("seen 2 times in 2 tasks"), "{human}");
    assert!(human.contains(&first) && human.contains(&second), "{human}");
}

/// H10: with nothing recorded, retro still answers, with the same keys.
#[test]
fn retro_with_no_state_reports_nothing() {
    let f = common::fixture();
    let out = f
        .cmd()
        .args(["retro", "--days", "30", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["days"], 30, "{v}");
    assert_eq!(v["reviews"], 0, "{v}");
    assert_eq!(v["findings"], 0, "{v}");
    assert_eq!(v["categories"], serde_json::json!([]), "{v}");
    assert!(v["since"].as_str().unwrap().ends_with('Z'), "{v}");
    let human = f.cmd().arg("retro").assert().success().get_output().stdout.clone();
    assert_eq!(String::from_utf8(human).unwrap().trim(), "retro: no review findings in the last 7 days");
}

/// `--days` is a whole number from 1 to 3650.
#[test]
fn retro_refuses_days_outside_its_range() {
    let f = common::fixture();
    for bad in ["0", "3651", "seven"] {
        f.cmd().args(["retro", "--days", bad]).assert().code(2);
    }
}
