//! MEAS-1: the reviewer of the next attempt marks, for every finding of the
//! previous REJECT, whether it was fixed by changing behaviour
//! (`findings.prior_rejection`); Thoth checks each mark when the review
//! arrives and stores the result as `findings.prior_rejection_check`,
//! without ever changing the verdict; `thoth retro` counts the marks.
//!
//! Everything is read through the CLI with a temporary THOTH_HOME and the
//! fake harnesses; the reviewer's document is handed in through the fakes'
//! `review_fix` scenario and its plan file.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const CODEX_MODEL: &str = "gpt-5.6-luna";

fn json_of(f: &Fixture, args: &[&str], what: &str) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, what);
    serde_json::from_str(String::from_utf8_lossy(&out).trim())
        .unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {}", String::from_utf8_lossy(&out)))
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

/// A fresh fixture with two Claude profiles (same provider) and a codex one.
fn fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    f
}

/// Dispatches the task's next attempt with claude-b and waits for it to
/// reach `in_review`; returns `dispatch --json`.
fn implement(f: &Fixture, task: &str, scenario: &str) -> Value {
    implement_with(f, task, scenario, &[])
}

/// `implement` with extra `dispatch` arguments.
fn implement_with(f: &Fixture, task: &str, scenario: &str, extra: &[&str]) -> Value {
    let d = f.dispatch(task, "claude-b", scenario, extra);
    let (_, status) = f.wait_for(task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(task));
    d
}

/// A new two-criterion task with attempt 1 implemented by claude-b.
fn first_attempt(f: &Fixture) -> String {
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    implement(f, &task, "ok");
    task
}

fn tag() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn write_file(f: &Fixture, name: &str, v: &Value) -> PathBuf {
    let p = f.home_path().join(format!("{name}-{}.json", tag()));
    std::fs::write(&p, v.to_string()).unwrap();
    p
}

struct Reviewed {
    started: Value,
    waited: Value,
    prompt: String,
}

/// A reviewer run of `task` by `profile` that hands in `doc` (the fakes'
/// `review_fix` scenario); `mode` is `--no-edit` or `--edit`. Waits for it.
fn review(f: &Fixture, task: &str, profile: &str, doc: &Value, mode: &str) -> Reviewed {
    let plan = write_file(f, "plan", &json!({"document": doc}));
    let prompt_copy = f.home_path().join(format!("prompt-{}.txt", tag()));
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_fix")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_FIX_PLAN", &plan)
        .env("FAKE_PROMPT_COPY", &prompt_copy)
        .args(["review", "start", task, "--profile", profile, "--json", mode]);
    if profile.starts_with("codex") {
        c.args(["--model", CODEX_MODEL, "--effort", "medium"]);
    } else {
        c.args(["--model", "opus", "--effort", "medium"]);
    }
    let started: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    let run = s(&started["run_id"]);
    let waited = json_of(f, &["wait", "--run", &run, "--timeout", "180s", "--json"], "wait --run");
    let prompt = std::fs::read_to_string(&prompt_copy).unwrap_or_default();
    Reviewed { started, waited, prompt }
}

/// `review record TASK -f FILE --json` of `doc`.
fn record(f: &Fixture, task: &str, doc: &Value) -> Value {
    let file = write_file(f, "record", doc);
    let file = file.display().to_string();
    json_of(f, &["review", "record", task, "-f", &file, "--json"], "review record")
}

fn show(f: &Fixture, task: &str) -> Value {
    json_of(f, &["review", "show", task, "--json"], "review show")
}

fn show_human(f: &Fixture, task: &str) -> String {
    let mut c = f.cmd();
    c.args(["review", "show", task]);
    String::from_utf8(f.run_ok(c, "review show")).unwrap()
}

/// The task's evidence codes, from `evidence list --json`.
fn codes(f: &Fixture, task: &str) -> Vec<String> {
    f.evidence_rows(task).iter().map(|e| s(&e["code"])).collect()
}

/// A path the diff evidence of `run` changed, from `evidence show --json`.
fn changed_path(f: &Fixture, task: &str, run: &str) -> String {
    let row = f
        .evidence_rows(task)
        .into_iter()
        .find(|e| e["kind"] == "diff" && e["run_id"] == run)
        .unwrap_or_else(|| panic!("no diff evidence of {run}: {:?}", f.evidence_rows(task)));
    let code = s(&row["code"]);
    let shown = json_of(f, &["evidence", "show", &code, "--task", task, "--json"], "evidence show");
    s(&shown["detail"]["files_changed"][0])
}

fn verdict_events(f: &Fixture, task: &str) -> Vec<Value> {
    f.events_json(task).into_iter().filter(|e| e["type"] == "verdict").map(|e| e["detail"].clone()).collect()
}

/// Attempt 1's REJECT: AC1 unmet (spec_mismatch, S) and one extra finding
/// (docs, M); AC2 met.
fn rejection(code: &str) -> Value {
    json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting is wrong", "fix_size": "S", "category": "spec_mismatch"},
            {"id": "AC2", "verdict": "met", "evidence": [code]}
        ],
        "extra_findings": [
            {"severity": "minor", "note": "the usage text is stale", "evidence": [], "fix_size": "M", "category": "docs"}
        ]
    }})
}

/// An attempt-2 document: AC1 met, AC2 unmet, a REJECT, with `marks` as
/// `findings.prior_rejection` (left out when `None`).
fn second(code: &str, marks: Option<Value>) -> Value {
    let mut doc = json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "met", "evidence": [code]},
            {"id": "AC2", "verdict": "unmet", "evidence": [], "note": "still exits 1", "fix_size": "S", "category": "error_path"}
        ],
        "extra_findings": []
    }});
    if let Some(m) = marks {
        doc["findings"]["prior_rejection"] = m;
    }
    doc
}

fn mark(review_id: &str, reference: &str, fixed: &str, evidence: &[&str], diff_at: &str, note: &str) -> Value {
    json!({"review_id": review_id, "ref": reference, "fixed_by_behaviour": fixed, "evidence": evidence, "diff_at": diff_at, "note": note})
}

fn check(shown: &Value) -> &Value {
    &shown["findings"]["prior_rejection_check"]
}

fn entry<'a>(check: &'a Value, reference: &str) -> &'a Value {
    check["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("no entries: {check}"))
        .iter()
        .find(|e| e["ref"] == reference)
        .unwrap_or_else(|| panic!("no entry for {reference}: {check}"))
}

fn counts(yes: u64, no: u64, not_fixed: u64, unmarked: u64) -> Value {
    json!({"yes": yes, "no": no, "not_fixed": not_fixed, "unmarked": unmarked})
}

/// (a) a Claude reviewer of attempt 2 marks both findings of attempt 1's
/// REJECT: the prompt names them, the check marks both, the verdict event
/// and the packet carry it.
#[test]
fn a_second_attempt_reviewer_marks_every_finding_of_the_previous_reject() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    let first = review(&f, &task, "claude-a", &rejection(&code), "--no-edit");
    assert_eq!(first.waited["verdict"], "REJECT", "{}", f.explain_task(&task));
    let rejected = show(&f, &task);
    let reject_id = s(&rejected["review_id"]);
    assert!(first.prompt.contains("## Previous rejection to mark"), "{}", first.prompt);
    assert!(!first.prompt.contains("- AC1 [category"), "nothing to mark on attempt 1: {}", first.prompt);

    let d = implement(&f, &task, "hooks");
    assert_eq!(d["attempt"], 2, "{d}");
    let run2 = s(&d["run_id"]);
    let path = changed_path(&f, &task, &run2);
    let marks = json!([
        mark(&reject_id, "AC1", "yes", &[], &format!("{path}:1"), ""),
        mark(&reject_id, "extra_findings[0]", "no", &[&code], "", "the usage text was a doc fix")
    ]);
    let r = review(&f, &task, "claude-a", &second(&code, Some(marks.clone())), "--no-edit");
    assert_eq!(r.waited["verdict"], "REJECT", "{}", f.explain_task(&task));

    // The prompt section, between the required checks and the end.
    let p = &r.prompt;
    let at = p.find("## Previous rejection to mark").unwrap_or_else(|| panic!("no section: {p}"));
    assert!(p.find("## Required checks not run").unwrap() < at, "{p}");
    let section = &p[at..];
    assert!(section.contains(&reject_id), "{section}");
    assert!(section.contains("attempt 1"), "{section}");
    let line = |reference: &str| -> String {
        section
            .lines()
            .find(|l| l.starts_with(&format!("- {reference} [")))
            .unwrap_or_else(|| panic!("no line for {reference}: {section}"))
            .to_string()
    };
    let ac1 = line("AC1");
    assert!(ac1.contains(&format!("category {}", s(&rejected["findings"]["criteria"][0]["category"]))), "{ac1}");
    assert!(ac1.contains("fix_size S"), "{ac1}");
    assert!(ac1.contains("the greeting is wrong"), "{ac1}");
    assert!(ac1.contains(&code), "{ac1}");
    let extra = line("extra_findings[0]");
    assert!(extra.contains("severity minor"), "{extra}");
    assert!(extra.contains("category docs") && extra.contains("fix_size M"), "{extra}");
    assert!(p.find("- AC1 [").unwrap() < p.find("- extra_findings[0] [").unwrap(), "expected order: {section}");

    let shown = show(&f, &task);
    assert_eq!(shown["findings"]["prior_rejection"], marks, "{shown}");
    let c = check(&shown);
    assert_eq!(c["status"], "checked", "{c}");
    assert_eq!(c["review_id"], reject_id.as_str(), "{c}");
    assert_eq!(c["previous_verdict"], "REJECT", "{c}");
    assert_eq!(c["expected"], json!(["AC1", "extra_findings[0]"]), "{c}");
    let e = entry(c, "AC1");
    assert_eq!(e["marked"], true, "{c}");
    assert_eq!(e["fixed_by_behaviour"], "yes", "{c}");
    assert_eq!(e["category"], rejected["findings"]["criteria"][0]["category"], "{c}");
    assert_eq!(e["fix_size"], rejected["findings"]["criteria"][0]["fix_size"], "{c}");
    assert_eq!(e["reason"], Value::Null, "{c}");
    assert_eq!(e["index"], 0, "{c}");
    let e = entry(c, "extra_findings[0]");
    assert_eq!(e["marked"], true, "{c}");
    assert_eq!(e["fixed_by_behaviour"], "no", "{c}");
    assert_eq!(e["category"], rejected["findings"]["extra_findings"][0]["category"], "{c}");
    assert_eq!(e["index"], 1, "{c}");
    assert_eq!(c["extra_entries"], json!([]), "{c}");
    assert_eq!(c["array_error"], Value::Null, "{c}");
    assert_eq!(c["counts"], counts(1, 1, 0, 0), "{c}");

    // The verdict event's summary.
    let v = verdict_events(&f, &task).pop().unwrap();
    assert_eq!(
        v["prior_rejection"],
        json!({"status": "checked", "review_id": reject_id, "expected": 2, "yes": 1, "no": 1, "not_fixed": 0, "unmarked": 0}),
        "{v}"
    );

    // The packet's stored findings carry both keys.
    let packet = json_of(&f, &["review", "packet", &task, "--json"], "review packet");
    let last = packet["reviews"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["findings"]["prior_rejection"], marks, "{last}");
    assert_eq!(last["findings"]["prior_rejection_check"], *c, "{last}");

    // review show's human lines.
    let human = show_human(&f, &task);
    assert!(
        human.lines().any(|l| l == format!("  prior rejection {reject_id}: yes 1, no 1, not_fixed 0, unmarked 0")),
        "{human}"
    );
}

/// (b) a missing mark and a mark citing a code that is not this task's
/// evidence are unmarked, while the review is accepted as declared.
#[test]
fn bad_marks_are_recorded_unmarked_and_never_change_the_verdict() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    review(&f, &task, "claude-a", &rejection(&code), "--no-edit");
    let reject_id = s(&show(&f, &task)["review_id"]);
    implement(&f, &task, "hooks");
    let foreign = "E999";
    assert!(!codes(&f, &task).contains(&foreign.to_string()), "{:?}", codes(&f, &task));
    let marks = json!([mark(&reject_id, "extra_findings[0]", "no", &[foreign], "", "")]);
    let doc = second(&code, Some(marks));
    let r = review(&f, &task, "claude-a", &doc, "--no-edit");
    assert_eq!(r.waited["verdict"], "REJECT", "{}", f.explain_task(&task));
    assert_eq!(f.wait_for_task_status(&task, &["pass", "reject", "unverified"], 30), "reject");
    assert!(!f.events_of(&task).contains(&"review_invalid".to_string()), "{:?}", f.events_of(&task));

    let c = check(&show(&f, &task)).clone();
    let e = entry(&c, "AC1");
    assert_eq!((e["marked"].clone(), e["reason"].clone(), e["index"].clone()), (json!(false), json!("missing"), Value::Null), "{c}");
    let e = entry(&c, "extra_findings[0]");
    assert_eq!(e["marked"], false, "{c}");
    assert_eq!(e["reason"], "foreign_evidence", "{c}");
    assert!(s(&e["detail"]).contains(foreign), "{c}");
    assert_eq!(e["fixed_by_behaviour"], Value::Null, "{c}");
    assert_eq!(c["counts"], counts(0, 0, 0, 2), "{c}");

    // `cited` holds only the criteria's and extra findings' codes.
    let mut from_doc: Vec<String> = Vec::new();
    for c in doc["findings"]["criteria"].as_array().unwrap().iter().chain(doc["findings"]["extra_findings"].as_array().unwrap()) {
        for code in c["evidence"].as_array().unwrap() {
            if !from_doc.contains(&s(code)) {
                from_doc.push(s(code));
            }
        }
    }
    let v = verdict_events(&f, &task).pop().unwrap();
    assert_eq!(v["cited"], json!(from_doc), "{v}");
    assert_eq!(v["prior_rejection"]["unmarked"], 2, "{v}");
}

/// (c) every remaining failing check through `review record`, then a
/// `prior_rejection` that is not an array.
#[test]
fn review_record_checks_every_mark_and_records_the_declared_verdict() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    let extra = |note: &str, category: &str| json!({"severity": "major", "note": note, "evidence": [], "fix_size": "S", "category": category});
    let reject = json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "one", "fix_size": "S", "category": "spec_mismatch"},
            {"id": "AC2", "verdict": "unmet", "evidence": [], "note": "two", "fix_size": "M", "category": "test_missing"}
        ],
        "extra_findings": [extra("e0", "docs"), extra("e1", "evidence"), extra("e2", "platform"), extra("e3", "files"), extra("e4", "other")]
    }});
    record(&f, &task, &reject);
    let reject_id = s(&show(&f, &task)["review_id"]);
    implement(&f, &task, "ok");

    let r = reject_id.as_str();
    let marks = json!([
        mark("01NOTTHEREJECTEDREVIEWXXXX", "AC1", "yes", &[&code], "", ""),
        mark(r, "AC2", "maybe", &[&code], "", ""),
        mark(r, "extra_findings[0]", "yes", &[], "", ""),
        mark(r, "extra_findings[1]", "no", &[&code], "src/nowhere.rs:3", ""),
        mark(r, "extra_findings[2]", "yes", &[&code], "", ""),
        mark(r, "extra_findings[2]", "no", &[&code], "", ""),
        {"review_id": r, "ref": "extra_findings[3]", "fixed_by_behaviour": "yes", "evidence": code, "diff_at": null, "note": null},
        mark(r, "extra_findings[4]", "yes", &[&code], "src/x.rs", ""),
        mark(r, "AC9", "yes", &[&code], "", ""),
        42
    ]);
    let doc = second(&code, Some(marks.clone()));
    let v = record(&f, &task, &doc);
    assert_eq!(v["verdict"], "REJECT", "{v}");
    assert_eq!(v["task_status"], "reject", "{v}");
    let shown = show(&f, &task);
    assert_eq!(shown["findings"]["prior_rejection"], marks, "{shown}");
    let c = check(&shown).clone();
    assert_eq!(c["status"], "checked", "{c}");
    for (reference, reason) in [
        ("AC1", "wrong_review_id"),
        ("AC2", "bad_value"),
        ("extra_findings[0]", "no_support"),
        ("extra_findings[1]", "diff_at_not_in_diff"),
        ("extra_findings[2]", "duplicate_ref"),
        ("extra_findings[3]", "bad_shape"),
        ("extra_findings[4]", "bad_diff_at"),
    ] {
        let e = entry(&c, reference);
        assert_eq!(e["reason"], reason, "{reference}: {c}");
        assert_eq!(e["marked"], false, "{reference}: {c}");
    }
    assert_eq!(entry(&c, "AC2")["category"], "test_missing", "{c}");
    assert_eq!(entry(&c, "extra_findings[1]")["category"], "evidence", "{c}");
    assert_eq!(
        c["extra_entries"],
        json!([{"index": 8, "ref": "AC9", "reason": "unknown_ref"}, {"index": 9, "ref": null, "reason": "bad_shape"}]),
        "{c}"
    );
    assert_eq!(c["counts"], counts(0, 0, 0, 7), "{c}");
    let human = show_human(&f, &task);
    assert!(human.lines().any(|l| l == "    unmarked AC2: bad_value"), "{human}");

    // Attempt 3 (past the default cap of 2): `prior_rejection` given as a string.
    implement_with(&f, &task, "ok", &["--allow-attempt", "3"]);
    let second_id = s(&show(&f, &task)["review_id"]);
    let doc = second(&code, Some(json!("all fixed")));
    let v = record(&f, &task, &doc);
    assert_eq!(v["verdict"], "REJECT", "{v}");
    let shown = show(&f, &task);
    let c = check(&shown).clone();
    assert_eq!(c["review_id"], second_id.as_str(), "{c}");
    assert!(c["array_error"].is_string(), "{c}");
    assert_eq!(c["expected"], json!(["AC2"]), "attempt 2's REJECT had AC2 unmet: {c}");
    assert_eq!(entry(&c, "AC2")["reason"], "missing", "{c}");
    assert_eq!(c["counts"], counts(0, 0, 0, 1), "{c}");
    assert_eq!(shown["findings"]["prior_rejection"], "all fixed", "{shown}");
    assert_eq!(shown["blocking"], json!(["AC2"]), "{shown}");
    assert!(s(&shown["brief"]).contains("AC2 unmet"), "{shown}");
}

/// (d) a first attempt: no array stores nothing and the event says null;
/// an array is kept and its entries are listed apart.
#[test]
fn a_first_attempt_has_nothing_to_mark() {
    let f = fixture();
    let plain = first_attempt(&f);
    let code = codes(&f, &plain)[0].clone();
    review(&f, &plain, "claude-a", &rejection(&code), "--no-edit");
    let shown = show(&f, &plain);
    let findings = shown["findings"].as_object().unwrap();
    assert!(!findings.contains_key("prior_rejection_check"), "{shown}");
    assert!(!findings.contains_key("prior_rejection"), "{shown}");
    let v = verdict_events(&f, &plain).pop().unwrap();
    assert!(v.as_object().unwrap().contains_key("prior_rejection"), "{v}");
    assert_eq!(v["prior_rejection"], Value::Null, "{v}");

    let marked = first_attempt(&f);
    let code = codes(&f, &marked)[0].clone();
    let mut doc = rejection(&code);
    doc["findings"]["prior_rejection"] = json!([mark("01SOMEREVIEW", "AC1", "yes", &[&code], "", "")]);
    record(&f, &marked, &doc);
    let shown = show(&f, &marked);
    let c = check(&shown);
    assert_eq!(c["status"], "no_previous_reject", "{c}");
    assert_eq!(c["review_id"], Value::Null, "{c}");
    assert_eq!(c["previous_verdict"], Value::Null, "{c}");
    assert_eq!(c["expected"], json!([]), "{c}");
    assert_eq!(c["entries"], json!([]), "{c}");
    assert_eq!(c["extra_entries"], json!([{"index": 0, "ref": "AC1", "reason": "nothing_to_mark"}]), "{c}");
    assert_eq!(c["counts"], counts(0, 0, 0, 0), "{c}");
    let human = show_human(&f, &marked);
    assert!(human.lines().any(|l| l == "  prior rejection: no previous rejection, 1 entries ignored"), "{human}");
}

/// (e) a codex reviewer of attempt 2: its schema asks for the marks and
/// its marks are checked like any other.
#[test]
fn a_codex_reviewer_gets_the_schema_property_and_its_marks_are_checked() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    record(&f, &task, &rejection(&code));
    let reject_id = s(&show(&f, &task)["review_id"]);
    implement(&f, &task, "ok");
    let marks = json!([
        mark(&reject_id, "AC1", "not_fixed", &[], "", "the greeting is still wrong"),
        mark(&reject_id, "extra_findings[0]", "no", &[&code], "", "docs only")
    ]);
    let r = review(&f, &task, "codex-a", &second(&code, Some(marks)), "--no-edit");
    assert_eq!(r.waited["verdict"], "REJECT", "{}", f.explain_task(&task));
    let run_dir = PathBuf::from(s(&r.started["run_dir"]));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(run_dir.join("review-schema.json")).unwrap()).unwrap();
    let findings = &schema["properties"]["findings"];
    assert!(findings["required"].as_array().unwrap().contains(&json!("prior_rejection")), "{schema}");
    let item = &findings["properties"]["prior_rejection"]["items"];
    let mut keys: Vec<&String> = item["properties"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["diff_at", "evidence", "fixed_by_behaviour", "note", "ref", "review_id"], "{schema}");
    assert_eq!(item["properties"]["fixed_by_behaviour"]["enum"], json!(["yes", "no", "not_fixed"]), "{schema}");
    assert!(r.prompt.contains("## Previous rejection to mark"), "{}", r.prompt);
    assert!(r.prompt.contains(&reject_id), "{}", r.prompt);

    let c = check(&show(&f, &task)).clone();
    assert_eq!(entry(&c, "AC1")["fixed_by_behaviour"], "not_fixed", "{c}");
    assert_eq!(entry(&c, "extra_findings[0]")["fixed_by_behaviour"], "no", "{c}");
    assert_eq!(c["counts"], counts(0, 1, 1, 0), "{c}");
}

/// Approved check `green` passes (`git --version`).
fn approve_green(f: &Fixture) {
    f.cmd().args(["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"]).assert().success();
    f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
}

/// An edit review of `task` with `marks` and a one-edit fix of AC1 (in
/// a.txt), its verify, and an approving confirm. Returns (the edit review's
/// `review show --json`, the confirm's `review show --json`).
fn edit_then_confirm(f: &Fixture, task: &str, code: &str, marks: Value) -> (Value, Value) {
    let doc = json!({
        "verdict": "REJECT",
        "findings": {
            "criteria": [
                {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the first line is wrong", "fix_size": "S", "category": "spec_mismatch"},
                {"id": "AC2", "verdict": "met", "evidence": [code]}
            ],
            "extra_findings": [],
            "prior_rejection": marks
        },
        "edits": [{"path": "a.txt", "old": "one\n", "new": "uno\n", "criterion": "AC1", "reason": "fixes the finding"}]
    });
    let r = review(f, task, "claude-a", &doc, "--edit");
    assert_eq!(r.waited["pending_fix"], true, "{}\n{}", r.waited, f.explain_task(task));
    let edit_shown = show(f, task);
    let run = s(&r.started["run_id"]);
    let out = f.cmd().args(["verify", &run, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "fix_confirm_approve")
        .env("FAKE_TASK_ID", task)
        .args(["review", "confirm", task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--json"]);
    let started: Value = serde_json::from_slice(&f.run_ok(c, "review confirm")).unwrap();
    let w = json_of(f, &["wait", "--run", &s(&started["run_id"]), "--timeout", "180s", "--json"], "wait --run");
    assert_eq!(w["verdict"], "PASS", "{}", f.explain_task(task));
    (edit_shown, show(f, task))
}

/// (f) an edit review's marks, then its fix confirmed: the confirm's record
/// carries the same marks and check.
#[test]
fn a_confirm_copies_the_edit_reviews_marks_and_check() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    record(&f, &task, &rejection(&code));
    let reject_id = s(&show(&f, &task)["review_id"]);
    implement(&f, &task, "ok");
    approve_green(&f);
    let marks = json!([
        mark(&reject_id, "AC1", "not_fixed", &[], "", "still wrong; the fix below changes it"),
        mark(&reject_id, "extra_findings[0]", "no", &[&code], "", "docs only")
    ]);
    let (edit, confirmed) = edit_then_confirm(&f, &task, &code, marks.clone());
    assert_eq!(edit["findings"]["prior_rejection"], marks, "{edit}");
    assert_eq!(check(&edit)["counts"], counts(0, 1, 1, 0), "{edit}");
    assert_eq!(confirmed["source"], "author_check", "{confirmed}");
    assert_eq!(confirmed["findings"]["prior_rejection"], marks, "{confirmed}");
    assert_eq!(check(&confirmed), check(&edit), "{confirmed}");
    let v = verdict_events(&f, &task).pop().unwrap();
    assert_eq!(v["source"], "author_check", "{v}");
    assert_eq!(
        v["prior_rejection"],
        json!({"status": "checked", "review_id": reject_id, "expected": 2, "yes": 0, "no": 1, "not_fixed": 1, "unmarked": 0}),
        "{v}"
    );
}

/// (g) retro counts the marks of three chains: a same-provider REJECT, a
/// cross-provider one and an external one (marked by an edit review whose
/// fix is then confirmed), each once.
#[test]
fn retro_counts_marks_by_category_and_by_the_rejecting_reviews_provider() {
    let f = fixture();
    let profiles = json_of(&f, &["profile", "list", "--json"], "profile list");
    let provider = |id: &str| -> String {
        let list = profiles["profiles"].as_array().unwrap_or_else(|| panic!("{profiles}"));
        s(&list.iter().find(|p| p["profile_id"] == id).unwrap_or_else(|| panic!("{id}: {profiles}"))["provider"])
    };
    assert_eq!(provider("claude-b"), provider("claude-a"), "{profiles}");
    assert_ne!(provider("claude-b"), provider("codex-a"), "{profiles}");

    // Same provider: claude-a rejects claude-b's attempt 1.
    let same = first_attempt(&f);
    let code_same = codes(&f, &same)[0].clone();
    review(&f, &same, "claude-a", &rejection(&code_same), "--no-edit");
    let same_reject = s(&show(&f, &same)["review_id"]);
    implement(&f, &same, "ok");
    let marks = json!([mark(&same_reject, "AC1", "yes", &[&code_same], "", "")]);
    record(&f, &same, &second(&code_same, Some(marks)));
    let same_check = check(&show(&f, &same)).clone();

    // Cross provider: codex-a rejects claude-b's attempt 1.
    let cross = first_attempt(&f);
    let code_cross = codes(&f, &cross)[0].clone();
    review(&f, &cross, "codex-a", &rejection(&code_cross), "--no-edit");
    let cross_reject = s(&show(&f, &cross)["review_id"]);
    implement(&f, &cross, "ok");
    let marks = json!([
        mark(&cross_reject, "AC1", "no", &[&code_cross], "", "a test change"),
        mark(&cross_reject, "extra_findings[0]", "not_fixed", &[], "", "the usage text is unchanged")
    ]);
    review(&f, &cross, "claude-a", &second(&code_cross, Some(marks)), "--no-edit");
    let cross_check = check(&show(&f, &cross)).clone();

    // Unknown: the REJECT is an external review; the edit review marks one
    // finding and leaves the other out, and its fix is confirmed.
    let ext = first_attempt(&f);
    let code_ext = codes(&f, &ext)[0].clone();
    record(&f, &ext, &rejection(&code_ext));
    let ext_reject = s(&show(&f, &ext)["review_id"]);
    implement(&f, &ext, "ok");
    approve_green(&f);
    let marks = json!([mark(&ext_reject, "AC1", "not_fixed", &[], "", "fixed only by this review's edit")]);
    let (edit, _) = edit_then_confirm(&f, &ext, &code_ext, marks);
    let ext_check = check(&edit).clone();

    let checks = [("same_provider", &same_check), ("cross_provider", &cross_check), ("unknown", &ext_check)];
    for (side, c) in checks {
        assert_eq!(c["status"], "checked", "{side}: {c}");
    }

    let elsewhere = tempfile::tempdir().unwrap();
    let mut c = f.cmd_in(elsewhere.path());
    c.args(["retro", "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "retro --json")).unwrap();
    let fbb = &v["fixed_by_behaviour"];
    let mut keys: Vec<&String> = fbb.as_object().unwrap_or_else(|| panic!("{v}")).keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["by_rejecting_review", "categories", "findings", "no", "not_fixed", "not_recorded", "reviews", "unmarked", "yes"],
        "{v}"
    );
    assert_eq!(fbb["reviews"], 3, "the confirm is counted once, on its edit review: {fbb}");
    assert_eq!(fbb["not_recorded"], 0, "{fbb}");
    let sum = |key: &str| -> u64 { checks.iter().map(|(_, c)| c["counts"][key].as_u64().unwrap()).sum() };
    for key in ["yes", "no", "not_fixed", "unmarked"] {
        assert_eq!(fbb[key].as_u64(), Some(sum(key)), "{key}: {fbb}");
    }
    let expected_total: u64 = checks.iter().map(|(_, c)| c["expected"].as_array().unwrap().len() as u64).sum();
    assert_eq!(fbb["findings"].as_u64(), Some(expected_total), "{fbb}");
    for (side, c) in checks {
        assert_eq!(fbb["by_rejecting_review"][side], c["counts"], "{side}: {fbb}");
    }
    let mut sides: Vec<&String> = fbb["by_rejecting_review"].as_object().unwrap().keys().collect();
    sides.sort();
    assert_eq!(sides, ["cross_provider", "same_provider", "unknown"], "{fbb}");

    // Categories: the checks' entries grouped by category.
    let mut want: std::collections::BTreeMap<String, [u64; 4]> = Default::default();
    for (_, c) in checks {
        for e in c["entries"].as_array().unwrap() {
            let cat = e["category"].as_str().unwrap_or("unclassified").to_string();
            let slot = want.entry(cat).or_default();
            match e["fixed_by_behaviour"].as_str() {
                Some("yes") if e["marked"] == true => slot[0] += 1,
                Some("no") if e["marked"] == true => slot[1] += 1,
                Some("not_fixed") if e["marked"] == true => slot[2] += 1,
                _ => slot[3] += 1,
            }
        }
    }
    let got = fbb["categories"].as_array().unwrap();
    assert_eq!(got.len(), want.len(), "{fbb}");
    for g in got {
        let w = want.get(&s(&g["category"])).unwrap_or_else(|| panic!("unexpected {g}"));
        assert_eq!([g["yes"].as_u64().unwrap(), g["no"].as_u64().unwrap(), g["not_fixed"].as_u64().unwrap(), g["unmarked"].as_u64().unwrap()], *w, "{g}");
        assert_eq!(g["count"].as_u64().unwrap(), w.iter().sum::<u64>(), "{g}");
    }
    let spec_mismatch = got.iter().find(|g| g["category"] == "spec_mismatch").unwrap();
    assert_eq!(spec_mismatch["blocking"], true, "{fbb}");
    let docs = got.iter().find(|g| g["category"] == "docs").unwrap();
    assert_eq!(docs["blocking"], false, "{fbb}");
    let order: Vec<u64> = got.iter().map(|g| g["count"].as_u64().unwrap()).collect();
    let mut sorted = order.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(order, sorted, "most count first: {fbb}");

    let mut c = f.cmd_in(elsewhere.path());
    c.arg("retro");
    let human = String::from_utf8(f.run_ok(c, "retro")).unwrap();
    assert!(
        human.lines().any(|l| l == "fixed by behaviour (marks on the previous rejection): 3 reviews, 0 not recorded (before MEAS-1)"),
        "{human}"
    );
    assert!(
        human.lines().any(|l| l == format!("  yes {} no {} not_fixed {} unmarked {}", fbb["yes"], fbb["no"], fbb["not_fixed"], fbb["unmarked"])),
        "{human}"
    );
    for g in got {
        let line = format!(
            "  {}: yes {} no {} not_fixed {} unmarked {}  ({})",
            s(&g["category"]),
            g["yes"],
            g["no"],
            g["not_fixed"],
            g["unmarked"],
            if g["blocking"] == true { "blocking" } else { "advisory" }
        );
        assert!(human.lines().any(|l| l == line), "{line}\n{human}");
    }
    for side in ["same_provider", "cross_provider", "unknown"] {
        let b = &fbb["by_rejecting_review"][side];
        let line = format!("  rejected by {side}: yes {} no {} not_fixed {} unmarked {}", b["yes"], b["no"], b["not_fixed"], b["unmarked"]);
        assert!(human.lines().any(|l| l == line), "{line}\n{human}");
    }
}

// --- TESTGAP-1 (MEAS-1 gaps) ----------------------------------------------------------------

/// `edit_then_confirm`'s flow with a confirm that rejects (`fix_confirm_revise`):
/// an edit review of `task` with `marks` and a one-edit fix of AC1, its
/// verify, then a confirm that revises the edit. Returns (the edit review's
/// `review show --json`, the confirm's `review show --json`).
fn edit_then_confirm_rejecting(f: &Fixture, task: &str, code: &str, marks: Value) -> (Value, Value) {
    let doc = json!({
        "verdict": "REJECT",
        "findings": {
            "criteria": [
                {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the first line is wrong", "fix_size": "S", "category": "spec_mismatch"},
                {"id": "AC2", "verdict": "met", "evidence": [code]}
            ],
            "extra_findings": [],
            "prior_rejection": marks
        },
        "edits": [{"path": "a.txt", "old": "one\n", "new": "uno\n", "criterion": "AC1", "reason": "fixes the finding"}]
    });
    let r = review(f, task, "claude-a", &doc, "--edit");
    assert_eq!(r.waited["pending_fix"], true, "{}\n{}", r.waited, f.explain_task(task));
    let edit_shown = show(f, task);
    let run = s(&r.started["run_id"]);
    let out = f.cmd().args(["verify", &run, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "fix_confirm_revise")
        .env("FAKE_TASK_ID", task)
        .args(["review", "confirm", task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--json"]);
    let started: Value = serde_json::from_slice(&f.run_ok(c, "review confirm")).unwrap();
    let w = json_of(f, &["wait", "--run", &s(&started["run_id"]), "--timeout", "180s", "--json"], "wait --run");
    assert_eq!(w["verdict"], "REJECT", "{}", f.explain_task(task));
    (edit_shown, show(f, task))
}

/// (h) the same flow as (f), but the confirm rejects: its record copies the
/// edit review's marks and check, and its verdict event says so.
#[test]
fn a_confirm_that_rejects_copies_the_edit_reviews_marks_and_check() {
    let f = fixture();
    let task = first_attempt(&f);
    let code = codes(&f, &task)[0].clone();
    record(&f, &task, &rejection(&code));
    let reject_id = s(&show(&f, &task)["review_id"]);
    implement(&f, &task, "ok");
    approve_green(&f);
    let marks = json!([
        mark(&reject_id, "AC1", "not_fixed", &[], "", "still wrong; the fix below changes it"),
        mark(&reject_id, "extra_findings[0]", "no", &[&code], "", "docs only")
    ]);
    let (edit, confirmed) = edit_then_confirm_rejecting(&f, &task, &code, marks.clone());
    assert_eq!(check(&edit)["counts"], counts(0, 1, 1, 0), "{edit}");
    assert_eq!(confirmed["source"], "author_check", "{confirmed}");
    assert_eq!(confirmed["findings"]["prior_rejection"], marks, "{confirmed}");
    assert_eq!(check(&confirmed), check(&edit), "{confirmed}");
    assert_eq!(f.task_status(&task), "reject");
    let v = verdict_events(&f, &task).pop().unwrap();
    assert_eq!(v["source"], "author_check", "{v}");
    assert_eq!(v["verdict"], "REJECT", "{v}");
    assert_eq!(
        v["prior_rejection"],
        json!({"status": "checked", "review_id": reject_id, "expected": 2, "yes": 0, "no": 1, "not_fixed": 1, "unmarked": 0}),
        "{v}"
    );
    let human = show_human(&f, &task);
    assert!(
        human.lines().any(|l| l == format!("  prior rejection {reject_id}: yes 0, no 1, not_fixed 1, unmarked 0")),
        "{human}"
    );
}

/// An attempt-1 UNVERIFIED record (AC1 unmet), then the next dispatch, left
/// `in_review`. Returns (task, an evidence code, the UNVERIFIED review's id,
/// the dispatch's `attempt`).
fn unverified_then_next_dispatch(f: &Fixture) -> (String, String, String, Value) {
    let task = first_attempt(f);
    let code = codes(f, &task)[0].clone();
    let doc = json!({"verdict": "UNVERIFIED", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting could not be checked", "fix_size": "S", "category": "spec_mismatch"},
            {"id": "AC2", "verdict": "met", "evidence": [code]}
        ],
        "extra_findings": []
    }});
    let v = record(f, &task, &doc);
    assert_eq!(v["verdict"], "UNVERIFIED", "{v}");
    let shown = show(f, &task);
    assert_eq!(shown["verdict"], "UNVERIFIED", "{shown}");
    let id = s(&shown["review_id"]);
    assert_eq!(f.task_status(&task), "unverified");
    let d = implement(f, &task, "hooks");
    (task, code, id, d)
}

/// After an UNVERIFIED review the next dispatch keeps attempt 1 (the worktree
/// is reused: docs/adapter-usage.md, `dispatch`, raises the attempt only out of
/// `reject`), so the review of that run has status `no_previous_reject`: there
/// is no earlier attempt to compare with. A mark it receives is recorded under
/// `extra_entries` as `nothing_to_mark`; without a mark there is no
/// `prior_rejection_check` at all. The status `previous_not_reject` cannot be
/// produced through the CLI, because the review that lets an attempt rise is
/// always a REJECT; it stays covered only by the unit test of the
/// review-target rule.
#[test]
fn after_an_unverified_review_the_next_review_of_attempt_1_has_no_previous_reject_to_mark() {
    let f = fixture();
    for with_mark in [false, true] {
        let (task, code, unverified_id, d) = unverified_then_next_dispatch(&f);
        assert_eq!(d["attempt"], 1, "an unverified task is rerouted in the same attempt: {d}");
        let marks = with_mark.then(|| json!([mark(&unverified_id, "AC1", "yes", &[&code], "", "")]));
        let r = review(&f, &task, "claude-a", &second(&code, marks), "--no-edit");
        assert_eq!(r.waited["verdict"], "REJECT", "[mark {with_mark}] {}", f.explain_task(&task));
        assert!(r.prompt.contains("Nothing to mark"), "[mark {with_mark}] {}", r.prompt);

        let shown = show(&f, &task);
        let v = verdict_events(&f, &task).pop().unwrap();
        let human = show_human(&f, &task);
        if with_mark {
            let c = check(&shown);
            assert_eq!(c["status"], "no_previous_reject", "{c}");
            assert_eq!(c["review_id"], Value::Null, "{c}");
            assert_eq!(c["previous_verdict"], Value::Null, "{c}");
            assert_eq!(c["expected"], json!([]), "{c}");
            assert_eq!(c["entries"], json!([]), "{c}");
            assert_eq!(c["counts"], counts(0, 0, 0, 0), "{c}");
            assert_eq!(c["extra_entries"], json!([{"index": 0, "ref": "AC1", "reason": "nothing_to_mark"}]), "{c}");
            assert_eq!(v["prior_rejection"]["status"], "no_previous_reject", "{v}");
            assert_eq!(v["prior_rejection"]["expected"], 0, "{v}");
            assert!(human.lines().any(|l| l == "  prior rejection: no previous rejection, 1 entries ignored"), "{human}");
        } else {
            assert!(!shown["findings"].as_object().unwrap().contains_key("prior_rejection_check"), "{shown}");
            assert_eq!(v["prior_rejection"], Value::Null, "{v}");
            assert!(!human.contains("prior rejection"), "{human}");
        }
    }
}
