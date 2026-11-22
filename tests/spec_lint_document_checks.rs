//! SMALL-11 (3): `thoth spec lint` warns when a docs or research spec lists no
//! checks, because `verify` then runs every approved check of the repository.
//! A warning only; no other task type gets it, and neither does a repository
//! whose check catalogue is empty.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const RULE: &str = "no_checks_for_document_task";

fn spec(extra: Value) -> String {
    let mut v = json!({"goal": "write the guide", "acceptance_criteria": ["AC1: the guide exists"]});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}

fn write_spec(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

/// `spec lint FILE --json`: (exit code, document).
fn lint(f: &Fixture, contents: &str) -> (i32, Value) {
    let file = write_spec(f, "spec-under-lint.json", contents);
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("spec lint printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr))
    });
    (out.status.code().unwrap(), v)
}

fn hits(v: &Value) -> Vec<&Value> {
    v["warnings"].as_array().unwrap().iter().filter(|w| w["rule"] == RULE).collect()
}

fn with_check(f: &Fixture) {
    f.cmd().args(["checks", "add", "c1", "--cmd", "git --version"]).assert().success();
}

fn initialized() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

#[test]
fn a_docs_or_research_spec_without_checks_gets_a_warning_and_still_passes() {
    let f = initialized();
    with_check(&f);
    for raw in [
        spec(json!({"task_type": "docs"})),
        spec(json!({"task_type": "research", "checks": []})),
    ] {
        let (code, v) = lint(&f, &raw);
        assert_eq!(code, 0, "{v}");
        assert_eq!(v["ok"], true, "{v}");
        let found = hits(&v);
        assert_eq!(found.len(), 1, "{v}");
        let w = found[0];
        assert_eq!(w["location"], "checks", "{w}");
        let task_type = serde_json::from_str::<Value>(&raw).unwrap()["task_type"]
            .as_str()
            .unwrap()
            .to_string();
        let message = w["message"].as_str().unwrap();
        assert!(message.contains(&task_type), "{message}");
        assert!(message.contains("every approved check"), "{message}");
        assert!(message.contains("list the checks"), "{message}");
        assert!(v["errors"].as_array().unwrap().is_empty(), "{v}");
    }
}

#[test]
fn no_warning_with_a_listed_check_for_another_task_type_or_before_any_check_exists() {
    let f = initialized();
    // the catalogue is still empty: nothing for verify to run by accident
    let (code, v) = lint(&f, &spec(json!({"task_type": "docs"})));
    assert_eq!((code, hits(&v).len()), (0, 0), "{v}");

    with_check(&f);
    for raw in [
        spec(json!({"task_type": "docs", "checks": ["c1"]})),
        spec(json!({"task_type": "code"})),
        spec(json!({"task_type": "review"})),
        spec(json!({"task_type": "mechanical", "checks": []})),
        spec(json!({})),
    ] {
        let (code, v) = lint(&f, &raw);
        assert_eq!(code, 0, "{raw}: {v}");
        assert!(hits(&v).is_empty(), "{raw}: {v}");
    }
}

#[test]
fn task_create_goes_on_and_shows_the_warning() {
    let f = initialized();
    with_check(&f);
    let file = write_spec(&f, "docs-spec.json", &spec(json!({"task_type": "docs"})));
    let out = f.cmd().args(["task", "create", "-f"]).arg(&file).arg("--json").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["task_id"].as_str().is_some(), "{v}");
    let warnings: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .filter(|w| w.starts_with(&format!("spec lint: {RULE} (checks)")))
        .collect();
    assert_eq!(warnings.len(), 1, "{v}");
}
