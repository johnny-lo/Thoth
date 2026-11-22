//! CLAIMKIND-1: a worker that puts a note, a decision or a finding in
//! result.json is told which claim kind to use. The two rule-1 rejections about
//! claim kinds say it (read through `events --task TASK --json`, and in the
//! prompt of the next dispatch in the same attempt, as the fake records it),
//! and every implementer prompt of a task that is not a research task says it
//! up front. Fake workers only, in a temporary THOTH_HOME and repo.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const REASON: &str = "claim kind guidance fixture: fake worker in a temporary repo";
const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";

/// Today's rule-1 sentence for a claim of kind `other`, which must stay the
/// start of the reason.
const BAD_KIND_SENTENCE: &str = "result.json claim kind must be one of test_result/doc_reference/completion/behavior/protected_change/research, got 'other'";
/// Today's rule-1 sentence for the fake's `research_no_sources` delivery (a
/// completion claim, then a research claim with `sources: []`).
const NO_SOURCES_SENTENCE: &str = "result.json claim 2 is kind research but its 'sources' is empty";

fn spec(task_type: &str) -> String {
    format!(
        r#"{{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"{task_type}"}}"#
    )
}

fn setup() -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

/// `dispatch TASK --profile PROFILE --model MODEL --json` with `envs`.
fn dispatch(f: &Fixture, task: &str, profile: &str, model: &str, envs: &[(&str, &str)]) -> Value {
    let mut c = f.cmd();
    c.env("FAKE_TASK_ID", task);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(["dispatch", task, "--profile", profile, "--model", model, "--effort", "medium", "--reason", REASON, "--json"]);
    serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap()
}

/// The task's status once `wait` returns.
fn settle(f: &Fixture, task: &str) -> String {
    let (_, status) = f.wait_for(task, "180s");
    status
}

/// The detail of the task's only result_invalid event, from
/// `events --since 0 --task TASK --json`.
fn the_rejection(f: &Fixture, task: &str) -> Value {
    let invalid: Vec<Value> = f
        .events_json(task)
        .into_iter()
        .filter(|e| e["type"] == "result_invalid")
        .collect();
    assert_eq!(invalid.len(), 1, "one result_invalid event expected: {invalid:?}");
    invalid[0]["detail"].clone()
}

/// What the reason adds after today's sentence `today`; panics when the
/// reason does not start with it.
fn guidance_after<'a>(reason: &'a str, today: &str) -> &'a str {
    reason
        .strip_prefix(today)
        .unwrap_or_else(|| panic!("the reason does not start with today's sentence {today:?}:\n{reason}"))
}

fn names_behavior_or_completion(text: &str) -> bool {
    text.contains("behavior") || text.contains("completion")
}

/// A fake-claude `hooks` delivery whose only claim is of kind `other`.
fn deliver_kind_other(f: &Fixture, task: &str) -> Value {
    let patch = f.home_path().join(format!("patch-{task}.json"));
    std::fs::write(
        &patch,
        json!({"claims": [{"kind": "other", "text": "a note for the commander", "evidence": []}]}).to_string(),
    )
    .unwrap();
    let patch = patch.display().to_string();
    dispatch(f, task, "claude-b", "sonnet", &[("FAKE_SCENARIO", "hooks"), ("FAKE_RESULT_PATCH", patch.as_str())])
}

/// (a) a claim of kind `other`: unverified, rule 1, today's sentence first and
/// then a sentence that names behavior or completion.
#[test]
fn a_claim_of_an_unknown_kind_is_told_to_use_behavior_or_completion() {
    let f = setup();
    let (task, _) = f.create_task(&spec("code"));
    deliver_kind_other(&f, &task);
    assert_eq!(settle(&f, &task), "unverified", "{}", f.explain_task(&task));
    let detail = the_rejection(&f, &task);
    assert_eq!(detail["rule"], 1, "{detail}");
    let reason = s(&detail["reason"]);
    let added = guidance_after(&reason, BAD_KIND_SENTENCE);
    assert!(names_behavior_or_completion(added), "the reason adds no kind to use:\n{reason}");
    assert!(added.contains("research task"), "the reason does not say when research applies:\n{reason}");
    // the event's other detail keys are today's
    let mut keys: Vec<&str> = detail.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["bytes", "reason", "rule"], "{detail}");
}

/// (b) a code task's claim of kind research with no sources: unverified, rule
/// 1, today's sentence first and then a pointer to behavior or completion.
#[test]
fn a_code_tasks_research_claim_without_sources_is_told_to_use_behavior_or_completion() {
    let f = setup();
    let (task, _) = f.create_task(&spec("code"));
    dispatch(&f, &task, "claude-b", "sonnet", &[("FAKE_SCENARIO", "research_no_sources")]);
    assert_eq!(settle(&f, &task), "unverified", "{}", f.explain_task(&task));
    let detail = the_rejection(&f, &task);
    assert_eq!(detail["rule"], 1, "{detail}");
    let reason = s(&detail["reason"]);
    let added = guidance_after(&reason, NO_SOURCES_SENTENCE);
    assert!(names_behavior_or_completion(added), "the reason adds no kind to use:\n{reason}");
    assert!(added.contains("not a research task"), "the reason does not say when it applies:\n{reason}");
}

/// (c) after (a), the next dispatch in the same attempt hands the worker the
/// whole reason, guidance included.
#[test]
fn the_next_dispatch_in_the_same_attempt_quotes_the_whole_reason() {
    let f = setup();
    let (task, _) = f.create_task(&spec("code"));
    let first = deliver_kind_other(&f, &task);
    assert_eq!(settle(&f, &task), "unverified", "{}", f.explain_task(&task));
    let reason = s(&the_rejection(&f, &task)["reason"]);
    assert!(names_behavior_or_completion(guidance_after(&reason, BAD_KIND_SENTENCE)), "{reason}");

    let copy = f.home_path().join("second-prompt.txt");
    let copy_s = copy.display().to_string();
    let second = dispatch(&f, &task, "claude-b", "sonnet", &[("FAKE_SCENARIO", "ok"), ("FAKE_PROMPT_COPY", copy_s.as_str())]);
    assert_eq!(second["attempt"], first["attempt"], "a reroute stays in the same attempt: {first} {second}");
    assert_eq!(settle(&f, &task), "in_review", "{}", f.explain_task(&task));
    let prompt = std::fs::read_to_string(&copy).unwrap();
    assert!(prompt.contains(&reason), "the prompt lacks the whole reason {reason:?}:\n{prompt}");
}

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) -> PathBuf {
    f.add_profile_with(id, provider, harness)
}

/// The implementer prompt the fake of `profile` received for a new task of
/// `task_type`, delivered with `scenario`.
fn recorded_prompt(f: &Fixture, profile: &str, model: &str, task_type: &str, scenario: &str, extra: &[(&str, &str)]) -> String {
    let (task, _) = f.create_task(&spec(task_type));
    let copy: PathBuf = f.home_path().join(format!("prompt-{profile}-{task_type}.txt"));
    let copy_s = copy.display().to_string();
    let mut envs = vec![("FAKE_SCENARIO", scenario), ("FAKE_PROMPT_COPY", copy_s.as_str())];
    envs.extend_from_slice(extra);
    dispatch(f, &task, profile, model, &envs);
    assert_eq!(settle(f, &task), "in_review", "{profile} {task_type}: {}", f.explain_task(&task));
    read(&copy)
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The sentences about kinds a code task's prompt carries, exactly.
const KINDS_SENTENCE: &str = "Use these claim kinds: test_result and doc_reference (each must cite at least one evidence code), completion and behavior (a summary, a note, a decision you took, or a finding for the commander), and protected_change. The kind research is only for a research task, and this is not one.";

/// (d) each harness: a code task's prompt names the kinds to use in the part
/// about result.json; a research task's prompt does not carry that sentence.
#[test]
fn every_implementer_prompt_of_a_code_task_names_the_kinds_to_use() {
    let f = setup();
    add_profile(&f, "codex-a", "openai", "codex");
    let agy_cfg = add_profile(&f, "agy-a", "google", "antigravity").display().to_string();
    for (profile, model, extra) in [
        ("claude-b", "sonnet", vec![]),
        ("codex-a", CODEX_MODEL, vec![]),
        ("agy-a", AGY_MODEL, vec![("FAKE_AGY_DATA_DIR", agy_cfg.as_str())]),
    ] {
        let code = recorded_prompt(&f, profile, model, "code", "ok", &extra);
        let at = code
            .find(KINDS_SENTENCE)
            .unwrap_or_else(|| panic!("{profile}: a code task's prompt lacks the kinds sentence\n{code}"));
        assert_eq!(code.matches(KINDS_SENTENCE).count(), 1, "{profile}: once\n{code}");
        // in the part that describes result.json: after its shape, before the evidence section
        let shape = code.find("write the file `.agent/result.json`").unwrap_or_else(|| panic!("{profile}: no shape\n{code}"));
        let evidence = code.find("\n## Evidence\n").unwrap_or_else(|| panic!("{profile}: no evidence section\n{code}"));
        assert!(shape < at && at < evidence, "{profile}: the kinds sentence is outside the result.json part\n{code}");

        let research = recorded_prompt(&f, profile, model, "research", "research_ok", &extra);
        assert!(research.contains("\n## Research claims\n"), "{profile}: not a research prompt\n{research}");
        assert!(!research.contains(KINDS_SENTENCE), "{profile}: a research task's prompt carries the kinds sentence\n{research}");
        assert!(!research.contains("this is not one"), "{profile}: {research}");
    }
}
