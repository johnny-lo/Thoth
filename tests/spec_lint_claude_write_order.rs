//! REVFIX-1: the `claude_write` rule of `thoth spec lint` refuses again a
//! sentence that names a `.claude` folder in one clause and puts the write in
//! a later clause with no target of its own (the folder-first order), and
//! still passes the forms SPECLINT-1 freed, where the write clause has its own
//! target, the folder clause carries a negation or a read-only marker, or a
//! trailing negation clause covers the sentence. Driven only through
//! `thoth spec lint FILE --json` and `thoth task create -f FILE --json` in an
//! initialised `common::fixture()` repository: no database is opened.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const FIXTURE: &str = include_str!("fixtures/spec_lint_claude_write_order.json");
const PREFIX: &str = "asks to write under a .claude folder";

struct Entry {
    name: String,
    field: String,
    text: String,
}

fn entries(list: &str) -> Vec<Entry> {
    let v: Value = serde_json::from_str(FIXTURE).unwrap();
    v[list]
        .as_array()
        .unwrap_or_else(|| panic!("fixture has no {list} list"))
        .iter()
        .map(|e| Entry {
            name: e["name"].as_str().unwrap().to_string(),
            field: e["field"].as_str().unwrap().to_string(),
            text: e["text"].as_str().unwrap().to_string(),
        })
        .collect()
}

fn entry(list: &str, name: &str) -> Entry {
    entries(list)
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no {name} in the fixture's {list} list"))
}

fn lint_fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

/// A spec with `text` in `field` and neutral text everywhere else.
fn spec_with(field: &str, text: &str) -> Value {
    match field {
        "goal" => json!({"goal": text, "acceptance_criteria": ["AC1: x"]}),
        "acceptance_criteria" => json!({"goal": "g", "acceptance_criteria": [text]}),
        "constraints" => json!({"goal": "g", "acceptance_criteria": ["AC1: x"], "constraints": [text]}),
        other => panic!("unknown field {other}"),
    }
}

/// Where the lint reports a finding on `text` placed by `spec_with`.
fn location_of(field: &str) -> &'static str {
    match field {
        "goal" => "goal",
        "acceptance_criteria" => "acceptance_criteria[0]",
        "constraints" => "constraints[0]",
        other => panic!("unknown field {other}"),
    }
}

fn write_spec(f: &Fixture, name: &str, spec: &Value) -> PathBuf {
    let p = f.home_path().join(format!("{name}.json"));
    std::fs::write(&p, spec.to_string()).unwrap();
    p
}

/// The exit code and the `--json` report of `thoth spec lint` on `spec`.
fn run_lint(f: &Fixture, name: &str, spec: &Value) -> (i32, Value) {
    let file = write_spec(f, name, spec);
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").assert().get_output().clone();
    let report: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code().unwrap(), report)
}

fn errors(report: &Value) -> Vec<Value> {
    report["errors"].as_array().unwrap().clone()
}

/// `thoth spec lint` refuses the entry: exit 1, `ok` false and exactly one
/// error, a `claude_write` at the entry's field. Returns that error.
fn assert_refused_in(f: &Fixture, e: &Entry) -> Value {
    let (code, report) = run_lint(f, &e.name, &spec_with(&e.field, &e.text));
    let errs = errors(&report);
    assert_eq!(code, 1, "{}: should be refused: {}\n{report}", e.name, e.text);
    assert_eq!(report["ok"], json!(false), "{}: {report}", e.name);
    assert_eq!(errs.len(), 1, "{}: one error expected: {report}", e.name);
    assert_eq!(errs[0]["rule"], "claude_write", "{}: {report}", e.name);
    assert_eq!(errs[0]["location"], location_of(&e.field), "{}: {report}", e.name);
    let message = errs[0]["message"].as_str().unwrap();
    assert!(message.starts_with(PREFIX), "{}: {message}", e.name);
    errs[0].clone()
}

/// `thoth spec lint` passes the entry: exit 0, `ok` true, no error at all.
fn assert_accepted_in(f: &Fixture, e: &Entry) {
    let (code, report) = run_lint(f, &e.name, &spec_with(&e.field, &e.text));
    assert!(
        errors(&report).is_empty(),
        "{}: should pass but has errors: {}\n{report}",
        e.name,
        e.text
    );
    assert_eq!(report["ok"], json!(true), "{}: {report}", e.name);
    assert_eq!(code, 0, "{}: {report}", e.name);
}

fn refused(name: &str) {
    assert_refused_in(&lint_fixture(), &entry("refused", name));
}

fn accepted(name: &str) {
    assert_accepted_in(&lint_fixture(), &entry("accepted", name));
}

// ------------------------------------------------ AC4: refused after the fix

#[test]
fn refuses_the_english_probe_folder_then_replace() {
    refused("probe_en_folder_first");
}

#[test]
fn refuses_under_the_agents_folder_then_delete() {
    refused("en_under_agents_then_delete");
}

#[test]
fn refuses_open_the_skill_then_add_a_line() {
    refused("en_open_then_add");
}

#[test]
fn refuses_a_backticked_folder_then_update() {
    refused("en_backticked_folder_then_update");
}

#[test]
fn refuses_the_chinese_probe_folder_then_change() {
    refused("probe_zh_folder_first");
}

#[test]
fn refuses_chinese_open_then_add_a_line() {
    refused("zh_open_then_add");
}

#[test]
fn refuses_chinese_old_file_in_the_agents_folder_then_delete() {
    refused("zh_agents_old_file_then_delete");
}

#[test]
fn refuses_when_the_nearest_earlier_target_is_the_folder_clause() {
    refused("zh_nearest_earlier_target_is_the_folder");
}

#[test]
fn the_write_first_sentences_stay_refused() {
    let f = lint_fixture();
    assert_refused_in(&f, &entry("refused", "en_write_first_stays_refused"));
    assert_refused_in(&f, &entry("refused", "zh_write_first_stays_refused"));
}

// ------------------------------------------------ AC5: accepted after the fix

#[test]
fn passes_read_in_the_folder_then_change_another_file() {
    accepted("speclint1_read_then_change_other");
}

#[test]
fn passes_chinese_read_the_folder_then_edit_another_path() {
    accepted("speclint1_zh_read_then_edit_other");
}

#[test]
fn passes_the_read_only_source_then_edit_a_file() {
    accepted("speclint1_read_only_marker");
}

#[test]
fn passes_the_speclint1_trailing_negation() {
    accepted("speclint1_trailing_negation");
}

#[test]
fn passes_read_the_folder_then_edit_a_bare_file_name() {
    accepted("en_file_name_is_a_target");
}

#[test]
fn passes_when_the_write_clause_has_its_own_target() {
    accepted("zh_write_clause_has_its_own_target");
}

#[test]
fn passes_a_chinese_read_only_marker_in_the_folder_clause() {
    accepted("zh_read_only_in_the_folder_clause");
}

#[test]
fn passes_an_english_read_only_marker_in_the_folder_clause() {
    accepted("en_read_only_in_the_folder_clause");
}

#[test]
fn passes_a_negation_in_the_folder_clause() {
    accepted("zh_negation_in_the_folder_clause");
}

#[test]
fn passes_a_trailing_negation_clause_after_the_write() {
    accepted("zh_trailing_negation_clause");
}

#[test]
fn passes_when_the_nearest_earlier_target_is_another_file() {
    accepted("zh_nearest_earlier_target_is_another_file");
}

// ------------------------------------------------ task create, message, all

#[test]
fn task_create_refuses_each_folder_first_sentence_and_stores_no_task() {
    let f = lint_fixture();
    for e in entries("refused") {
        let file = write_spec(&f, &format!("create-{}", e.name), &spec_with(&e.field, &e.text));
        let out = f.cmd().args(["task", "create", "-f"]).arg(&file).arg("--json").assert().get_output().clone();
        assert_eq!(out.status.code(), Some(1), "{}: task create should refuse", e.name);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let doc: Value = stderr
            .lines()
            .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok().filter(|v| v["ok"] == json!(false)))
            .unwrap_or_else(|| panic!("{}: no JSON error document on stderr: {stderr}", e.name));
        let message = doc["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: no error message in {doc}", e.name));
        assert!(
            message.starts_with("spec lint: 1 blocking problem(s): "),
            "{}: {message}",
            e.name
        );
        assert!(message.contains("claude_write"), "{}: {message}", e.name);
    }
    let status = f.cmd().args(["status", "--json"]).assert().success().get_output().stdout.clone();
    let status: Value = serde_json::from_slice(&status).unwrap();
    assert_eq!(status["tasks"], json!([]), "a refused spec was stored: {status}");
}

#[test]
fn a_folder_first_refusal_quotes_the_folder_clause_through_the_write_clause() {
    let f = lint_fixture();
    let cases = [
        (
            "In .claude/skills/x/SKILL.md, replace the first line with a new heading, keeping the tone",
            "In .claude/skills/x/SKILL.md, replace the first line with a new heading",
        ),
        (
            "在 .claude/skills/x/SKILL.md 裡，把第一段改成新的說明，語氣保持一樣",
            "在 .claude/skills/x/SKILL.md 裡，把第一段改成新的說明",
        ),
    ];
    for (i, (text, quoted)) in cases.iter().enumerate() {
        let e = Entry { name: format!("message-{i}"), field: "goal".into(), text: text.to_string() };
        let error = assert_refused_in(&f, &e);
        let message = error["message"].as_str().unwrap();
        let want = format!("{PREFIX} (\"{quoted}\")");
        assert!(message.starts_with(&want), "want {want}\ngot  {message}");
        assert!(!message.contains("keeping") && !message.contains("語氣"), "{message}");
    }
}

#[test]
fn every_fixture_entry_is_judged_as_listed() {
    let f = lint_fixture();
    let refused = entries("refused");
    let accepted = entries("accepted");
    assert!(refused.len() >= 10 && accepted.len() >= 11, "fixture lists shrank");
    for e in &refused {
        assert_refused_in(&f, e);
    }
    for e in &accepted {
        assert_accepted_in(&f, e);
    }
}
