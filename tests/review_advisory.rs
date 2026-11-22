//! REV-2: blocking findings are required, advisory ones are listed apart.
//! A rework folds advisory findings into their own list and prompt section
//! (AC1), stuck detection counts only blocking findings (AC2), `review show`
//! splits the newest record (AC5) and `retro` tells the two apart (AC6).
//! Everything goes through the CLI with fake workers in a temporary
//! THOTH_HOME; external verdicts are recorded with `review record`.

mod common;

use common::Fixture;
use serde_json::{json, Value};

const REASON: &str = "REV-2 fixture: fake workers in a temporary repo";
const BLOCKING_NOTE: &str = "the error is swallowed when the file is missing";
const ADVISORY_NOTE: &str = "the usage text still names the old flag";
const NO_FOLD_WARNING: &str = "no reviewer findings could be folded";

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn exec(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let mut c = f.cmd();
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.args(args).output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

/// Runs `args`, which must exit 0, and returns its stdout as JSON.
fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut c = f.cmd();
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// A new file under THOTH_HOME holding `body`; its path.
fn home_file(f: &Fixture, stem: &str, body: &str) -> String {
    let tag = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = f.home_path().join(format!("{stem}-{tag}.json"));
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

fn init(f: &Fixture) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
}

/// Waits for `task` to settle and checks it is back at `in_review`.
fn wait_in_review(f: &Fixture, task: &str) {
    let w = ok(f, &["wait", task, "--timeout", "180s", "--json"], &[]);
    assert_eq!(w["status"], "in_review", "{w}\n{}", f.explain_task(task));
}

/// A new normal-risk task whose first implementer run is `in_review`.
fn task_in_review(f: &Fixture) -> String {
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task, "claude-b", "hooks", &["--reason", REASON, "--skip-spec-review", "REV-2 fixture: the spec review gate is covered elsewhere"]);
    wait_in_review(f, &task);
    task
}

/// Records `document` as an external review of `task`.
fn record(f: &Fixture, task: &str, document: &Value) -> Value {
    let file = home_file(f, "review", &document.to_string());
    ok(f, &["review", "record", task, "-f", &file, "--json"], &[])
}

fn show(f: &Fixture, task: &str) -> Value {
    ok(f, &["review", "show", task, "--json"], &[])
}

fn show_human(f: &Fixture, task: &str) -> String {
    let out = f.cmd().args(["review", "show", task]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn met(id: &str) -> Value {
    json!({"id": id, "verdict": "met", "evidence": ["E001"]})
}

/// An unmet criterion; `category` `None` leaves the label out.
fn unmet(id: &str, note: &str, category: Option<&str>) -> Value {
    let mut c = json!({"id": id, "verdict": "unmet", "evidence": ["E001"], "note": note, "fix_size": "S"});
    if let Some(category) = category {
        c["category"] = json!(category);
    }
    c
}

fn extra(note: &str, category: &str) -> Value {
    json!({"severity": "major", "note": note, "evidence": ["E001"], "fix_size": "S", "category": category})
}

fn review(verdict: &str, criteria: Vec<Value>, extras: Vec<Value>) -> Value {
    json!({"verdict": verdict, "findings": {"criteria": criteria, "extra_findings": extras}})
}

fn dispatch_args<'a>(task: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"];
    args.extend_from_slice(extra);
    args
}

/// Reworks `task` with the `hooks` scenario plus `envs`, and waits for it.
fn rework(f: &Fixture, task: &str, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut e = vec![("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", task)];
    e.extend_from_slice(envs);
    let d = ok(f, &dispatch_args(task, extra), &e);
    assert_eq!(d["rework"], true, "{d}");
    wait_in_review(f, task);
    d
}

fn warnings(d: &Value) -> Vec<String> {
    d["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect()
}

/// AC7 (a), (f), (g): a REJECT with a blocking (error_path) unmet AC and an
/// advisory (docs) extra finding is split by `review show` and `retro`, and
/// its rework folds both, the docs one into the prompt's advisory section
/// only. A PASS with only a docs extra finding is all advisory, and no
/// review at all gives null.
#[test]
fn a_rework_and_review_show_and_retro_list_advisory_findings_apart() {
    let f = common::fixture();
    init(&f);
    let task = task_in_review(&f);

    // (f) no review yet: the three keys are null
    let none = show(&f, &task);
    for k in ["blocking", "advisory", "has_blocking_findings"] {
        assert_eq!(none[k], Value::Null, "{k}: {none}");
    }

    record(
        &f,
        &task,
        &review("REJECT", vec![unmet("AC1", BLOCKING_NOTE, Some("error_path")), met("AC2")], vec![extra(ADVISORY_NOTE, "docs")]),
    );
    // (f) the REJECT: AC1 blocks, the docs extra finding is advisory
    let shown = show(&f, &task);
    assert_eq!(shown["blocking"], json!(["AC1"]), "{shown}");
    assert_eq!(shown["advisory"], json!(["extra_findings[0]"]), "{shown}");
    assert_eq!(shown["has_blocking_findings"], true, "{shown}");
    let human = show_human(&f, &task);
    assert!(human.contains("\n  blocking: AC1\n"), "{human}");
    assert!(human.contains("\n  advisory (not required): extra_findings[0]"), "{human}");

    // (f) a PASS carrying only a docs extra finding
    let passed = task_in_review(&f);
    record(&f, &passed, &review("PASS", vec![met("AC1"), met("AC2")], vec![extra("a comment is out of date", "docs")]));
    let shown = show(&f, &passed);
    assert_eq!(shown["blocking"], json!([]), "{shown}");
    assert_eq!(shown["advisory"], json!(["extra_findings[0]"]), "{shown}");
    assert_eq!(shown["has_blocking_findings"], false, "{shown}");
    let human = show_human(&f, &passed);
    assert!(human.contains("\n  blocking: -\n"), "{human}");

    // (g) retro over the two records: error_path blocks, docs does not;
    // one blocking finding (AC1) and two advisory (the two docs extras)
    let elsewhere = tempfile::tempdir().unwrap();
    let out = f.cmd().current_dir(elsewhere.path()).args(["retro", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    let group = |name: &str| {
        r["categories"].as_array().unwrap().iter().find(|g| g["category"] == name).cloned().unwrap_or_else(|| panic!("no {name} group in {r}"))
    };
    assert_eq!(group("docs")["blocking"], false, "{r}");
    assert_eq!(group("docs")["count"], 2, "{r}");
    assert_eq!(group("error_path")["blocking"], true, "{r}");
    assert_eq!(group("error_path")["count"], 1, "{r}");
    assert_eq!(r["blocking_findings"], 1, "{r}");
    assert_eq!(r["advisory_findings"], 2, "{r}");
    assert_eq!(r["findings"], 3, "{r}");
    let out = f.cmd().current_dir(elsewhere.path()).arg("retro").output().unwrap();
    let human = String::from_utf8(out.stdout).unwrap();
    assert!(human.lines().next().unwrap().contains("3 findings, 1 blocking, 2 advisory"), "{human}");
    assert!(human.lines().any(|l| l.starts_with("docs ") && l.ends_with("(advisory)")), "{human}");
    assert!(human.lines().any(|l| l.starts_with("error_path ") && l.ends_with("(blocking)")), "{human}");

    // (a) the rework folds both; the prompt the worker received lists the
    // error_path note as required and the docs note only as advisory
    let copy = f.home_path().join("rework-prompt.txt");
    let copy_s = copy.display().to_string();
    let d = rework(&f, &task, &[], &[("FAKE_PROMPT_COPY", copy_s.as_str())]);
    assert_eq!(d["folded_findings"], 2, "{d}");
    assert_eq!(d["folded_advisory"], 1, "{d}");
    assert!(warnings(&d).iter().all(|w| !w.contains(NO_FOLD_WARNING)), "{d}");
    let prompt = std::fs::read_to_string(&copy).unwrap();
    let section = prompt.find("## Previous attempt was rejected").expect("rework section");
    let advisory = prompt.find("### Advisory findings (not required)").unwrap_or_else(|| panic!("no advisory section:\n{prompt}"));
    let fix = prompt.find("Fix exactly these.").unwrap();
    assert!(section < fix && fix < advisory, "{prompt}");
    let required = &prompt[section..advisory];
    assert!(required.contains(BLOCKING_NOTE), "{prompt}");
    assert!(!required.contains(ADVISORY_NOTE), "{prompt}");
    let after = &prompt[advisory..];
    assert!(after.contains(&format!("- major: {ADVISORY_NOTE} (evidence E001)")), "{prompt}");
    assert!(!after.contains(BLOCKING_NOTE), "{prompt}");
}

/// AC7 (b): a REJECT with only an advisory finding still reworks with it,
/// all of it advisory, and without the "nothing folded" warning.
#[test]
fn a_rework_of_an_advisory_only_reject_folds_only_advisory_findings() {
    let f = common::fixture();
    init(&f);
    let task = task_in_review(&f);
    record(&f, &task, &review("REJECT", vec![met("AC1"), met("AC2")], vec![extra(ADVISORY_NOTE, "docs")]));
    let d = rework(&f, &task, &[], &[]);
    assert_eq!(d["folded_findings"], 1, "{d}");
    assert_eq!(d["folded_advisory"], d["folded_findings"], "{d}");
    assert!(warnings(&d).iter().all(|w| !w.contains(NO_FOLD_WARNING)), "{d}");
}

/// AC7 (c): three REJECTs in a row with AC1 unmet. Labelled docs each time,
/// the next dispatch goes ahead; labelled error_path, or with no category,
/// it is refused as stuck on AC1.
#[test]
fn stuck_detection_counts_only_blocking_unmet_criteria() {
    for (category, stuck) in [(Some("docs"), false), (Some("error_path"), true), (None, true)] {
        let f = common::fixture();
        init(&f);
        let task = task_in_review(&f);
        let rejected = review("REJECT", vec![unmet("AC1", "the greeting is wrong", category), met("AC2")], vec![]);
        record(&f, &task, &rejected);
        rework(&f, &task, &[], &[]);
        record(&f, &task, &rejected);
        rework(&f, &task, &["--allow-attempt", "3"], &[]);
        record(&f, &task, &rejected);

        let fourth = dispatch_args(&task, &["--allow-attempt", "4"]);
        let envs = [("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", task.as_str())];
        if stuck {
            let out = exec(&f, &fourth, &envs);
            assert_eq!(out.code, 1, "{category:?}: {}\n{}", out.stdout, out.stderr);
            let err: Value = serde_json::from_str(out.stderr.trim())
                .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", out.stderr));
            assert_eq!(err["stuck"]["kind"], "criterion", "{category:?}: {err}");
            assert_eq!(err["stuck"]["value"], "AC1", "{category:?}: {err}");
        } else {
            let d = ok(&f, &fourth, &envs);
            assert_eq!(d["attempt"], 4, "{d}");
            assert_eq!(d["folded_advisory"], 1, "{d}");
            wait_in_review(&f, &task);
        }
    }
}

/// The line of `prompt` that starts with `prefix`; exactly one must.
fn the_line_starting<'a>(prompt: &'a str, prefix: &str) -> &'a str {
    let lines: Vec<&str> = prompt.lines().filter(|l| l.starts_with(prefix)).collect();
    assert_eq!(lines.len(), 1, "lines starting {prefix:?}: {lines:?}\n{prompt}");
    lines[0]
}

/// TESTRULE-1 (AC2): the worker prompt of a rework and of a reroute carries
/// the reviewers' interface-test rule once, in its place after the added
/// section; the text is the reviewer prompt's rule 6.
#[test]
fn rework_and_reroute_prompts_carry_the_interface_test_rule_line() {
    const LEAD_IN: &str = "Reviewers apply this rule to the tests you add or change: ";
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");

    // (a) a first dispatch, then a review that rejects
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let first_copy = f.home_path().join("first-prompt.txt");
    let first_s = first_copy.display().to_string();
    let skip = ["--skip-spec-review", "REV-2 fixture: the spec review gate is covered elsewhere"];
    ok(&f, &dispatch_args(&task, &skip), &[("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", task.as_str()), ("FAKE_PROMPT_COPY", first_s.as_str())]);
    wait_in_review(&f, &task);
    let first = std::fs::read_to_string(&first_copy).unwrap();
    let rule_line = the_line_starting(&first, LEAD_IN).to_string();
    let rule = rule_line.strip_prefix(LEAD_IN).unwrap().to_string();
    assert!(!rule.is_empty(), "{first}");

    let review_copy = f.home_path().join("review-prompt.txt");
    let review_s = review_copy.display().to_string();
    let started = ok(
        &f,
        &["review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "review_reject"), ("FAKE_TASK_ID", task.as_str()), ("FAKE_PROMPT_COPY", review_s.as_str())],
    );
    let review_run = started["run_id"].as_str().unwrap_or_else(|| panic!("no run_id: {started}")).to_string();
    ok(&f, &["wait", "--run", &review_run, "--timeout", "180s", "--json"], &[]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status = ok(&f, &["status", &task, "--json"], &[]);
        if status["status"] == "reject" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "task {task} is {}, not reject\n{}", status["status"], f.explain_task(&task));
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let reviewer_prompt = std::fs::read_to_string(&review_copy).unwrap();
    let rule6 = the_line_starting(&reviewer_prompt, "6. ");
    assert_eq!(rule6.strip_prefix("6. ").unwrap(), rule, "the reviewer prompt's rule 6 differs from the worker's line");

    // (b) the rework
    let rework_copy = f.home_path().join("rework-prompt.txt");
    let rework_s = rework_copy.display().to_string();
    rework(&f, &task, &[], &[("FAKE_PROMPT_COPY", rework_s.as_str())]);
    let prompt = std::fs::read_to_string(&rework_copy).unwrap();
    let section = prompt.find("## Previous attempt was rejected").unwrap_or_else(|| panic!("no rework section:\n{prompt}"));
    assert_eq!(prompt.matches(rule_line.as_str()).count(), 1, "{prompt}");
    let at = prompt.find(rule_line.as_str()).unwrap();
    let result = prompt.find("write the file `.agent/result.json`").unwrap();
    assert!(section < at && at < result, "{prompt}");

    // (c) a reroute after a result.json that was not accepted
    let (other, _) = f.create_task(Fixture::SPEC_TWO_AC);
    ok(&f, &dispatch_args(&other, &skip), &[("FAKE_SCENARIO", "bad_result"), ("FAKE_TASK_ID", other.as_str())]);
    let w = ok(&f, &["wait", &other, "--timeout", "180s", "--json"], &[]);
    assert_eq!(w["status"], "unverified", "{w}\n{}", f.explain_task(&other));
    let reroute_copy = f.home_path().join("reroute-prompt.txt");
    let reroute_s = reroute_copy.display().to_string();
    ok(&f, &dispatch_args(&other, &skip), &[("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", other.as_str()), ("FAKE_PROMPT_COPY", reroute_s.as_str())]);
    wait_in_review(&f, &other);
    let prompt = std::fs::read_to_string(&reroute_copy).unwrap();
    let section = prompt.find("## Previous run's result.json was not accepted").unwrap_or_else(|| panic!("no reroute section:\n{prompt}"));
    assert_eq!(prompt.matches(rule_line.as_str()).count(), 1, "{prompt}");
    let at = prompt.find(rule_line.as_str()).unwrap();
    let result = prompt.find("write the file `.agent/result.json`").unwrap();
    assert!(section < at && at < result, "{prompt}");
}

/// REV-2 (AC4): a first dispatch is not a rework and folds nothing.
#[test]
fn a_first_dispatch_folds_no_advisory_finding() {
    let f = common::fixture();
    init(&f);
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let d = ok(
        &f,
        &dispatch_args(&task, &["--skip-spec-review", "REV-2 fixture: the spec review gate is covered elsewhere"]),
        &[("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", task.as_str())],
    );
    assert_eq!(d["rework"], false, "{d}");
    assert_eq!(d["folded_findings"], 0, "{d}");
    assert_eq!(d["folded_advisory"], 0, "{d}");
    wait_in_review(&f, &task);
}
