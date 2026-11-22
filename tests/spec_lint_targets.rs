//! SMALL-10 (A): the lint rule `unknown_test_target` of `thoth spec lint`: a
//! spec whose `cargo test` command names a `--test` target that is not an
//! integration test target of the repository is refused before a task is
//! made, instead of failing verify before any test ran. Driven only through
//! the CLI and its `--json` output.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const RULE: &str = "unknown_test_target";

fn json_of(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim()).unwrap()
}

/// An initialised repository with a Cargo.toml, a flat target `flat` and a
/// folder target `folder` (its `main.rs`), all in the main work tree.
fn target_fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let repo = f.repo_path();
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::create_dir_all(repo.join("tests").join("folder")).unwrap();
    std::fs::write(repo.join("tests").join("flat.rs"), "#[test]\nfn t() {}\n").unwrap();
    std::fs::write(repo.join("tests").join("folder").join("main.rs"), "#[test]\nfn t() {}\n").unwrap();
    f
}

fn write_spec(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

/// A spec with the given goal and acceptance criteria.
fn spec(goal: &str, acs: &[&str]) -> String {
    json!({"goal": goal, "acceptance_criteria": acs}).to_string()
}

/// `spec lint --json` of `raw`: the exit code and the report.
fn lint(f: &Fixture, raw: &str) -> (i32, Value) {
    let file = write_spec(f, "spec.json", raw);
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").output().unwrap();
    (out.status.code().unwrap_or(-1), json_of(&out.stdout))
}

fn found<'a>(report: &'a Value, list: &str) -> Vec<&'a Value> {
    report[list].as_array().unwrap().iter().filter(|e| e["rule"] == RULE).collect()
}

fn names_in(findings: &[&Value]) -> Vec<String> {
    findings.iter().map(|e| e["message"].as_str().unwrap().to_string()).collect()
}

fn task_count(f: &Fixture) -> usize {
    let out = f.cmd().args(["status", "--json"]).assert().success().get_output().stdout.clone();
    json_of(&out)["tasks"].as_array().unwrap().len()
}

#[test]
fn a_flat_and_a_folder_target_are_known() {
    let f = target_fixture();
    let (code, v) = lint(&f, &spec("g", &["AC1: run `cargo test --test flat` and cargo test --test folder"]));
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["ok"], true);
    assert!(found(&v, "errors").is_empty(), "{v}");
    assert!(found(&v, "warnings").is_empty(), "{v}");
}

#[test]
fn a_missing_target_is_one_error_with_both_file_shapes() {
    let f = target_fixture();
    let (code, v) = lint(&f, &spec("g", &["AC1: x", "AC2: run `cargo test --test ghost_one`"]));
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
    let errors = found(&v, "errors");
    assert_eq!(errors.len(), 1, "{v}");
    assert!(found(&v, "warnings").is_empty(), "it is an error, not a warning: {v}");
    assert_eq!(errors[0]["location"], "acceptance_criteria[1]");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(message.contains("ghost_one"), "{message}");
    assert!(message.contains("no integration test target"), "{message}");
    assert!(message.contains("tests/ghost_one.rs"), "{message}");
    assert!(message.contains("tests/ghost_one/main.rs"), "{message}");
}

#[test]
fn only_the_missing_names_of_a_list_and_of_the_equals_form_are_reported() {
    let f = target_fixture();
    let (code, v) = lint(
        &f,
        &spec(
            "g",
            &["AC1: cargo test --test flat --test gone_a --test=gone_b --test=folder --test gone_c"],
        ),
    );
    assert_eq!(code, 1, "{v}");
    let errors = found(&v, "errors");
    assert_eq!(errors.len(), 3, "{v}");
    let messages = names_in(&errors);
    for (message, name) in messages.iter().zip(["gone_a", "gone_b", "gone_c"]) {
        assert!(message.contains(name), "{name} in {message}");
        assert!(!message.contains("flat") && !message.contains("folder"), "{message}");
    }
    assert!(errors.iter().all(|e| e["location"] == "acceptance_criteria[0]"));
}

#[test]
fn one_name_in_two_fields_is_reported_once_at_the_first() {
    let f = target_fixture();
    let raw = json!({
        "goal": "run cargo test --test ghost",
        "acceptance_criteria": ["AC1: cargo test --test ghost again"],
        "constraints": ["cargo test --test ghost"],
    })
    .to_string();
    let (code, v) = lint(&f, &raw);
    assert_eq!(code, 1, "{v}");
    let errors = found(&v, "errors");
    assert_eq!(errors.len(), 1, "{v}");
    assert_eq!(errors[0]["location"], "goal");

    // in the order of first mention, each at its own first field
    let raw = json!({
        "goal": "g",
        "acceptance_criteria": ["AC1: cargo test --test second"],
        "constraints": ["cargo test --test first", "cargo test --test second"],
    })
    .to_string();
    let (_, v) = lint(&f, &raw);
    let errors = found(&v, "errors");
    assert_eq!(errors.len(), 2, "{v}");
    assert_eq!(errors[0]["location"], "acceptance_criteria[0]");
    assert!(errors[0]["message"].as_str().unwrap().contains("second"));
    assert_eq!(errors[1]["location"], "constraints[0]");
    assert!(errors[1]["message"].as_str().unwrap().contains("first"));
}

#[test]
fn a_name_whose_new_test_file_the_spec_names_is_not_reported() {
    let f = target_fixture();
    let (code, v) = lint(
        &f,
        &spec(
            "g",
            &[
                "AC1: run cargo test --test fresh and cargo test --test fresh_dir",
                "AC2: write new tests/fresh.rs and new tests/fresh_dir/main.rs",
            ],
        ),
    );
    assert_eq!(code, 0, "{v}");
    assert!(found(&v, "errors").is_empty(), "{v}");

    // the path in another field, and a name that no path names stays reported
    let raw = json!({
        "goal": "g",
        "acceptance_criteria": ["AC1: cargo test --test fresh --test other"],
        "constraints": ["new tests/fresh.rs"],
    })
    .to_string();
    let (code, v) = lint(&f, &raw);
    assert_eq!(code, 1, "{v}");
    let errors = found(&v, "errors");
    assert_eq!(errors.len(), 1, "{v}");
    assert!(errors[0]["message"].as_str().unwrap().contains("other"));
}

#[test]
fn a_missing_name_outside_goal_acceptance_criteria_and_constraints_is_not_reported() {
    let f = target_fixture();
    let raw = json!({
        "goal": "g",
        "acceptance_criteria": ["AC1: x"],
        "non_goals": ["do not run cargo test --test ghost"],
        "excluded_alternatives": ["cargo test --test ghost"],
    })
    .to_string();
    let (code, v) = lint(&f, &raw);
    assert_eq!(code, 0, "{v}");
    assert!(found(&v, "errors").is_empty(), "{v}");
}

#[test]
fn the_check_is_silent_when_targets_may_live_elsewhere() {
    let missing = spec("g", &["AC1: cargo test --test ghost"]);

    // no Cargo.toml at the top
    let f = target_fixture();
    std::fs::remove_file(f.repo_path().join("Cargo.toml")).unwrap();
    let (code, v) = lint(&f, &missing);
    assert_eq!(code, 0, "no Cargo.toml: {v}");
    assert!(found(&v, "errors").is_empty());

    // a workspace
    let f = target_fixture();
    std::fs::write(f.repo_path().join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n").unwrap();
    let (code, v) = lint(&f, &missing);
    assert_eq!(code, 0, "[workspace]: {v}");
    assert!(found(&v, "errors").is_empty());

    // an explicit [[test]] table
    let f = target_fixture();
    std::fs::write(
        f.repo_path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"ghost\"\npath = \"elsewhere/x.rs\"\n",
    )
    .unwrap();
    let (code, v) = lint(&f, &missing);
    assert_eq!(code, 0, "[[test]]: {v}");
    assert!(found(&v, "errors").is_empty());
}

#[test]
fn task_create_and_update_spec_refuse_a_missing_target() {
    let f = target_fixture();
    let bad = write_spec(&f, "bad.json", &spec("g", &["AC1: cargo test --test ghost"]));
    let good = write_spec(&f, "good.json", &spec("g", &["AC1: cargo test --test flat"]));
    let before = task_count(&f);
    let out = f.cmd().args(["task", "create", "-f"]).arg(&bad).arg("--json").assert().failure().get_output().clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("spec lint: 1 blocking problem(s): "), "{err}");
    assert!(err.contains(RULE), "{err}");
    assert_eq!(task_count(&f), before, "nothing was created");

    let out = f.cmd().args(["task", "create", "-f"]).arg(&good).arg("--json").assert().success().get_output().clone();
    let task = json_of(&out.stdout)["task_id"].as_str().unwrap().to_string();
    assert_eq!(task_count(&f), before + 1);
    let shown = |f: &Fixture| {
        let out = f.cmd().args(["task", "show", &task, "--json"]).assert().success().get_output().stdout.clone();
        json_of(&out)["spec"].clone()
    };
    let spec_before = shown(&f);
    let out = f
        .cmd()
        .args(["task", "update-spec", &task, "-f"])
        .arg(&bad)
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("spec lint: 1 blocking problem(s): "), "{err}");
    assert!(err.contains(RULE), "{err}");
    assert_eq!(shown(&f), spec_before, "the spec is unchanged");
    assert_eq!(task_count(&f), before + 1);
}

/// A temporary folder git does not see as a repository.
fn outside() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(dir.path())
        .output()
        .expect("git must be on PATH");
    assert!(!out.status.success(), "{} is inside a git repository", dir.path().display());
    dir
}

#[test]
fn outside_a_repository_the_lint_fails_as_before_and_prints_no_report() {
    let f = target_fixture();
    let file = write_spec(&f, "bad.json", &spec("g", &["AC1: cargo test --test ghost"]));
    let dir = outside();
    let out = f.cmd_in(dir.path()).args(["spec", "lint"]).arg(&file).arg("--json").output().unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is not inside a git repository"), "{err}");
    assert!(!err.contains(RULE), "{err}");
}

#[test]
fn the_human_output_has_the_usual_error_line() {
    let f = target_fixture();
    let file = write_spec(&f, "bad.json", &spec("g", &["AC1: x", "AC2: cargo test --test ghost"]));
    let out = f.cmd().args(["spec", "lint"]).arg(&file).assert().code(1).get_output().clone();
    let text = String::from_utf8(out.stdout).unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("error: unknown_test_target (acceptance_criteria[1]): "))
        .unwrap_or_else(|| panic!("no unknown_test_target line in\n{text}"));
    assert!(line.contains("ghost") && line.contains("no integration test target"), "{line}");
    assert!(text.trim_end().ends_with("1 error(s), 0 warning(s)"), "{text}");
}
