//! SPECLINT-1: the `claude_write` rule of `thoth spec lint` no longer refuses
//! the specs of its measured false blocks (a source named in a clause apart
//! from the write, a read-only marker, a quoted ban, a bare list entry) and
//! still refuses every true block. Driven only through `thoth spec lint FILE
//! --json` on spec files in an initialised `common::fixture()` repository: no
//! database is opened.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const FIXTURE: &str = include_str!("fixtures/spec_lint_claude_write.json");
const PREFIX: &str = "asks to write under a .claude folder";

fn lint_fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn write_spec(f: &Fixture, name: &str, spec: &Value) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, spec.to_string()).unwrap();
    p
}

/// A spec with `text` in `field` (`goal`, `acceptance_criteria` or
/// `constraints`) and neutral text everywhere else.
fn spec_with(field: &str, text: &str) -> Value {
    match field {
        "goal" => json!({"goal": text, "acceptance_criteria": ["AC1: x"]}),
        "acceptance_criteria" => json!({"goal": "g", "acceptance_criteria": [text]}),
        "constraints" => json!({"goal": "g", "acceptance_criteria": ["AC1: x"], "constraints": [text]}),
        other => panic!("unknown field {other}"),
    }
}

/// The exit code and the `--json` output of `thoth spec lint` on `spec`.
fn run_lint(f: &Fixture, name: &str, spec: &Value) -> (i32, Value) {
    let file = write_spec(f, name, spec);
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").assert().get_output().clone();
    let json: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code().unwrap(), json)
}

fn claude_write_errors(report: &Value) -> Vec<&Value> {
    report["errors"].as_array().unwrap().iter().filter(|e| e["rule"] == "claude_write").collect()
}

/// The `claude_write` errors of one spec with each of `forms` as its own
/// constraint: one refused flag per form, read from the error's `location`.
fn refused_forms(f: &Fixture, forms: &[&str]) -> Vec<bool> {
    let spec = json!({"goal": "g", "acceptance_criteria": ["AC1: x"], "constraints": forms});
    let (code, report) = run_lint(f, "forms.json", &spec);
    let errors = report["errors"].as_array().unwrap();
    assert!(errors.iter().all(|e| e["rule"] == "claude_write"), "other errors: {errors:?}");
    assert_eq!(code, if errors.is_empty() { 0 } else { 1 });
    assert_eq!(report["ok"], json!(errors.is_empty()));
    (0..forms.len())
        .map(|i| {
            let here: Vec<_> =
                errors.iter().filter(|e| e["location"] == json!(format!("constraints[{i}]"))).collect();
            assert!(here.len() <= 1, "{}: one finding per field, got {here:?}", forms[i]);
            !here.is_empty()
        })
        .collect()
}

fn assert_passes(forms: &[&str]) {
    let f = lint_fixture();
    let refused = refused_forms(&f, forms);
    for (form, r) in forms.iter().zip(&refused) {
        assert!(!r, "should pass but is refused: {form}");
    }
}

fn assert_refused(forms: &[&str]) {
    let f = lint_fixture();
    let refused = refused_forms(&f, forms);
    for (form, r) in forms.iter().zip(&refused) {
        assert!(*r, "should be refused but passes: {form}");
    }
}

fn blocks(list: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(FIXTURE).unwrap();
    v[list].as_array().unwrap().clone()
}

/// The false block of the fixture with this name, linted as the field it
/// came from: exit 0, `ok`, and no `claude_write` in `errors`.
fn assert_false_block_passes(name: &str) {
    let block = blocks("false_blocks")
        .into_iter()
        .find(|b| b["name"] == name)
        .unwrap_or_else(|| panic!("no false block named {name}"));
    let f = lint_fixture();
    let spec = spec_with(block["field"].as_str().unwrap(), block["text"].as_str().unwrap());
    let (code, report) = run_lint(&f, "false.json", &spec);
    assert!(claude_write_errors(&report).is_empty(), "{name}: {report}");
    assert_eq!(report["ok"], json!(true), "{name}: {report}");
    assert_eq!(code, 0, "{name}: {report}");
}

// (a) one test per false form

#[test]
fn measured_copy_source_after_a_clause_comma_passes() {
    assert_false_block_passes("measured_ag1_copy_source");
}

#[test]
fn measured_read_only_marker_passes() {
    assert_false_block_passes("measured_f4_read_only_marker");
}

#[test]
fn measured_quoted_ban_in_a_help_test_sentence_passes() {
    assert_false_block_passes("measured_pub2_quoted_ban");
}

#[test]
fn measured_bare_list_entry_in_an_exclude_list_passes() {
    assert_false_block_passes("measured_a2_2_list_entry");
}

#[test]
fn measured_trailing_negation_clause_passes() {
    assert_false_block_passes("measured_ck1_trailing_negation");
}

#[test]
fn measured_aside_in_a_ban_list_passes() {
    assert_false_block_passes("measured_dt5_aside_in_a_ban_list");
}

#[test]
fn measured_aside_with_commas_in_a_ban_list_passes() {
    assert_false_block_passes("measured_tests1_aside_with_commas");
}

#[test]
fn measured_aside_before_a_backticked_list_passes() {
    assert_false_block_passes("measured_pub2_aside_before_a_list");
}

#[test]
fn small_aside_form_passes() {
    assert_false_block_passes("aside_keeps_the_ban_list_whole");
}

#[test]
fn small_clause_form_passes() {
    assert_false_block_passes("clause_source_then_other_file");
}

#[test]
fn small_zh_clause_form_passes() {
    assert_false_block_passes("clause_zh_read_then_edit_other");
}

#[test]
fn small_quoted_ban_comment_form_passes() {
    assert_false_block_passes("quoted_ban_comment_line");
}

#[test]
fn small_quoted_ban_do_not_edit_form_passes() {
    assert_false_block_passes("quoted_ban_do_not_edit");
}

#[test]
fn small_read_only_zh_form_passes() {
    assert_false_block_passes("read_only_marker_zh");
}

#[test]
fn small_read_only_en_form_passes() {
    assert_false_block_passes("read_only_marker_en");
}

#[test]
fn small_continuation_after_a_read_passes() {
    assert_false_block_passes("continuation_after_read");
}

#[test]
fn small_continuation_after_a_negated_write_passes() {
    assert_false_block_passes("continuation_after_negated_write");
}

#[test]
fn small_list_entry_zh_form_passes() {
    assert_false_block_passes("list_entry_zh");
}

#[test]
fn small_list_entry_comma_form_passes() {
    assert_false_block_passes("list_entry_comma_clauses");
}

// (b) one test per rule, with its own passing and refused forms

#[test]
fn ac2_clauses_split_at_commas_and_judge_each_clause_alone() {
    assert_passes(&[
        "read a file in .claude/x, then change src/a.rs",
        "讀 .claude/skills/x，改 plugins/y",
        // an ASCII comma with no space after it does not split: the negation covers the write
        "不改 a.txt,改 .claude/x",
        // `、` does not split
        "不改 plugins/x、.claude/y",
        // no separator splits inside a pair of backticks
        "改完那行是 `never touch ~/.claude. then stop; 勿動, ok`",
        // the clauses of one parenthetical aside share their negation
        "不改 Cargo.toml、plugins/thoth/（含 skills，由指揮官改）、.claude/",
        "do not edit src/a.rs (see b, c), .claude/x",
    ]);
    assert_refused(&[
        // a negation in one clause no longer covers a write in another
        "不改 a.txt，改 .claude/x",
        "not a.txt, edit .claude/x",
        "改 plugins/x、.claude/y",
        "改 a.txt,.claude/y",
        // the shared negation stops at the end of the aside; an opener with no closer shares nothing
        "不改 a.txt（見 b，c），改 .claude/x",
        "不改 a（b，改 .claude/x",
    ]);
}

#[test]
fn ac2_a_trailing_negation_clause_covers_the_clauses_before_it() {
    assert_passes(&[
        ".claude/skills/ 的私下版由指揮官之後自己改，這件不碰",
        "改 .claude/x，這件不用做",
        "edit .claude/x later, but this task does not touch it",
        "改 a.txt，改 .claude/x，這件不碰",
    ]);
    assert_refused(&[
        // the negation clause has a write word of its own: it is not a plain "do not touch"
        "改 .claude/x，不改 src/a.rs",
        // the negation clause names a target of its own
        "改 .claude/x，不碰 .claude/y",
        // only the last clause of the sentence covers; a negation in the middle does not
        "改 .claude/x，這件不碰，另外改 .claude/y",
        // a negation in the first clause covers nothing after it
        "不改 a.txt，改 .claude/x",
        // the sentence ends before the negation clause
        "改 .claude/x。這件不碰",
        // a read-only marker counts only in its own clause: a trailing source clause covers nothing
        "改 .claude/skills/x/SKILL.md，內容取自 plugins/x/SKILL.md",
        "edit .claude/x, copy it from a.txt (read-only)",
        "改 .claude/x，只讀 a.txt",
    ]);
}

#[test]
fn ac3_a_clause_that_starts_with_the_name_takes_the_verdict_before_it() {
    assert_refused(&[
        "edit src/a.rs, .claude/skills/x/SKILL.md",
        "改 plugins/x，.claude/y",
        "改 plugins/x，和 .claude/y",
        "edit src/a.rs, and `.claude/x`",
        "edit src/a.rs, or ~/.claude/x",
    ]);
    assert_passes(&[
        "read src/a.rs, .claude/x",
        "不改 plugins/x，.claude/y",
        // the name is later in the clause: not a continuation
        "edit src/a.rs, then see .claude/x",
        "改 plugins/x，內容見 .claude/y",
    ]);
}

#[test]
fn ac4_read_only_markers_are_negations_in_their_clause() {
    assert_passes(&[
        "以 .claude/x（只讀）為準，改 a.txt",
        "edit a.txt, copy it from .claude/x (read-only)",
        "edit .claude/x（唯讀）",
        "改 內容取自 .claude/x",
        "edit .claude/x (readonly)",
        "edit .claude/x (read only)",
        "edit .claude/x (READ-ONLY)",
        // the existing negation words still work inside their clause
        "never edit .claude/skills",
        "plugins/、.claude/、scripts/ 不碰",
    ]);
    assert_refused(&[
        // the marker stands in another clause
        "只讀 a.txt，改 .claude/x",
        "read-only a.txt, edit .claude/x",
        // not a whole word
        "edit .claude/x (readonlyish)",
    ]);
}

#[test]
fn ac5_a_quoted_ban_is_not_a_write_target() {
    assert_passes(&[
        "改完那一行是 `// never touch the operator's own ~/.claude`",
        "update the comment to `do not edit ~/.claude`",
        "update the comment to `禁止 改 ~/.claude`",
        "update the comment to `read only ~/.claude`",
    ]);
    assert_refused(&[
        // no whitespace: a path
        "update `.claude/skills/x/SKILL.md`",
        "改 `.claude/never/x`",
        // whitespace but no negation word: a command
        "刪 `rm -rf ~/.claude/x`",
        // the ban stands in another fragment than the name
        "update `.claude/x` like `do not hurry`",
    ]);
}

#[test]
fn ac6_a_bare_entry_in_an_adding_list_passes_and_the_rest_stays_refused() {
    assert_passes(&[
        "排除清單新增：docs/、.claude/、scripts/x.py",
        "add: docs/, .claude/, scripts/x.py",
        "add : docs/, .claude",
        "新增:docs/、.claude/",
        "加入：.claude/、docs/",
        "add: .claude/, docs/",
    ]);
    assert_refused(&[
        "新增：.claude/",
        "新增：.claude/skills/x/SKILL.md、docs/",
        "刪除：docs/、.claude/",
        "新增 .claude/、docs/",
        "新增：docs/、.claude/ 並刪 .claude/x",
        "add: docs/, .claude/skills/x",
        "add: .claude/",
    ]);
}

#[test]
fn rule_keeps_one_finding_per_field_and_skips_non_goals() {
    let f = lint_fixture();
    let spec = json!({
        "goal": "Edit .claude/a. Also update .claude/b",
        "acceptance_criteria": ["AC1: x"],
        "non_goals": ["改 .claude/skills/x/SKILL.md"],
        "excluded_alternatives": ["delete ~/.claude/agents/y.md"],
    });
    let (code, report) = run_lint(&f, "goal.json", &spec);
    assert_eq!(code, 1);
    let errors = claude_write_errors(&report);
    assert_eq!(errors.len(), 1, "{report}");
    assert_eq!(errors[0]["location"], json!("goal"));
    let spec = json!({
        "goal": "g",
        "acceptance_criteria": ["AC1: x"],
        "non_goals": ["改 .claude/skills/x/SKILL.md"],
        "excluded_alternatives": ["delete ~/.claude/agents/y.md"],
    });
    let (code, report) = run_lint(&f, "ignored.json", &spec);
    assert_eq!(code, 0, "{report}");
    assert!(claude_write_errors(&report).is_empty());
}

// (c) the fixture, one spec file per sentence

#[test]
fn fixture_true_blocks_are_refused_and_false_blocks_pass() {
    let f = lint_fixture();
    let true_blocks = blocks("true_blocks");
    let false_blocks = blocks("false_blocks");
    assert!(true_blocks.len() >= 17 && false_blocks.len() >= 14);
    for (i, b) in true_blocks.iter().enumerate() {
        let (field, text) = (b["field"].as_str().unwrap(), b["text"].as_str().unwrap());
        let (code, report) = run_lint(&f, &format!("true-{i}.json"), &spec_with(field, text));
        assert_eq!(code, 1, "{}: {report}", b["name"]);
        let errors = claude_write_errors(&report);
        assert_eq!(errors.len(), 1, "{}: {report}", b["name"]);
        let want = if field == "goal" { "goal".to_string() } else { format!("{field}[0]") };
        assert_eq!(errors[0]["location"], json!(want), "{}: {report}", b["name"]);
    }
    for (i, b) in false_blocks.iter().enumerate() {
        let (field, text) = (b["field"].as_str().unwrap(), b["text"].as_str().unwrap());
        let (code, report) = run_lint(&f, &format!("false-{i}.json"), &spec_with(field, text));
        assert_eq!(code, 0, "{}: {report}", b["name"]);
        assert!(claude_write_errors(&report).is_empty(), "{}: {report}", b["name"]);
        assert_eq!(report["ok"], json!(true), "{}: {report}", b["name"]);
    }
}

// (d) the message

#[test]
fn message_quotes_only_the_refused_clause_behind_the_prefix() {
    let f = lint_fixture();
    let (code, report) = run_lint(
        &f,
        "message.json",
        &spec_with("constraints", "read src/a.rs, then delete ~/.claude/x, then read b.txt"),
    );
    assert_eq!(code, 1);
    let errors = claude_write_errors(&report);
    assert_eq!(errors.len(), 1, "{report}");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("{PREFIX} (\"then delete ~/.claude/x\")")),
        "{message}"
    );
    assert!(!message.contains("src/a.rs") && !message.contains("b.txt"), "{message}");

    // a continuation clause is quoted alone too
    let (_, report) = run_lint(&f, "message2.json", &spec_with("goal", "edit src/a.rs, .claude/y"));
    let errors = claude_write_errors(&report);
    assert_eq!(errors.len(), 1, "{report}");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(message.starts_with(&format!("{PREFIX} (\".claude/y\")")), "{message}");
    assert!(!message.contains("src/a.rs"), "{message}");
}
