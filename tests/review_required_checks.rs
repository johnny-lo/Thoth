//! REV-3: a check the spec's `checks` field lists that did not run on the
//! run under review blocks. A code review report that disagrees is refused
//! (`review_invalid`, stage `required_checks`); a REJECT carrying a
//! `check_not_run` finding is taken, and that finding is blocking in
//! `review show`, the rework fold and `retro`. Everything goes through the
//! CLI in a temporary THOTH_HOME, with fake-claude as worker and reviewer
//! and fake-check as the registered check.

mod common;

use common::Fixture;
use serde_json::{json, Value};

const REASON: &str = "REV-3 fixture: fake workers in a temporary repo";
const SKIP_SPEC_REVIEW: &str = "REV-3 fixture: the spec review gate is covered elsewhere";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");
const CHECK: &str = "req-check";

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

fn init(f: &Fixture) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
}

/// A two-criterion spec at `risk`; `checks` `None` leaves the field out.
fn spec(checks: Option<&[&str]>, risk: &str) -> String {
    let mut s = json!({"goal": "say hello twice",
        "acceptance_criteria": ["AC1: prints hello", "AC2: exits zero"],
        "constraints": [], "non_goals": [], "excluded_alternatives": [], "risk": risk});
    if let Some(c) = checks {
        s["checks"] = json!(c);
    }
    s.to_string()
}

/// `checks add ID --cmd "<fake-check> MODE" --shell argv --json`: registers
/// (or re-registers, which clears its approval) a check left unapproved.
fn add_check(f: &Fixture, id: &str, mode: &str) {
    let cmd = format!("\"{FAKE_CHECK}\" {mode}");
    ok(f, &["checks", "add", id, "--cmd", &cmd, "--shell", "argv", "--json"], &[]);
}

fn approve(f: &Fixture) {
    ok(f, &["checks", "approve", "--reason", REASON, "--json"], &[]);
}

fn dispatch(f: &Fixture, task: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json",
    ];
    args.extend_from_slice(extra);
    ok(f, &args, &[("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", task)])
}

/// Waits for implementer run `run` and checks the task is `in_review`.
fn wait_in_review(f: &Fixture, task: &str, run: &str) {
    let w = ok(f, &["wait", "--run", run, "--timeout", "180s", "--json"], &[]);
    assert_eq!(w["task_status"], "in_review", "{w}\n{}", f.explain_task(task));
}

/// A new task with `spec_json` whose first implementer run is `in_review`:
/// (task, RUN).
fn task_in_review(f: &Fixture, spec_json: &str) -> (String, String) {
    let (task, _) = f.create_task(spec_json);
    let d = dispatch(f, &task, &["--skip-spec-review", SKIP_SPEC_REVIEW]);
    let run = d["run_id"].as_str().unwrap_or_else(|| panic!("dispatch gave no run_id: {d}")).to_string();
    wait_in_review(f, &task, &run);
    (task, run)
}

/// `verify RUN [extra] --json`, whose exit code is 0 or 1 (a refused or
/// failed check makes it 1); its JSON document.
fn verify(f: &Fixture, run: &str, extra: &[&str]) -> Value {
    let mut args = vec!["verify", run];
    args.extend_from_slice(extra);
    args.push("--json");
    let out = exec(f, &args, &[]);
    assert!(out.code == 0 || out.code == 1, "verify exited {}: {}\n{}", out.code, out.stdout, out.stderr);
    serde_json::from_str(out.stdout.trim()).unwrap_or_else(|e| panic!("verify printed no JSON ({e}): {}\n{}", out.stdout, out.stderr))
}

/// The one result row of `verify --json` for `CHECK`.
fn verify_row(v: &Value) -> Value {
    v["results"]
        .as_array()
        .and_then(|r| r.iter().find(|x| x["check_id"] == CHECK))
        .cloned()
        .unwrap_or_else(|| panic!("no {CHECK} row in {v}"))
}

fn packet_not_run(f: &Fixture, task: &str) -> Value {
    let p = ok(f, &["review", "packet", task, "--json"], &[]);
    p.get("checks_not_run").cloned().unwrap_or_else(|| panic!("no checks_not_run in review packet --json: {p}"))
}

fn not_run(reason: &str) -> Value {
    json!([{"check_id": CHECK, "reason": reason}])
}

/// The text of the Markdown packet's `## Required checks not run` section,
/// which must come before `## Diffs`.
fn packet_section(markdown: &str) -> String {
    let start = markdown.find("## Required checks not run\n").unwrap_or_else(|| panic!("no section in:\n{markdown}"));
    let end = markdown.find("## Diffs\n").unwrap_or_else(|| panic!("no ## Diffs in:\n{markdown}"));
    assert!(start < end, "the section is not before ## Diffs:\n{markdown}");
    markdown[start..end].to_string()
}

fn met(id: &str) -> Value {
    json!({"id": id, "verdict": "met", "evidence": ["E001"]})
}

fn extra(category: &str, note: &str) -> Value {
    json!({"severity": "major", "note": note, "evidence": [], "fix_size": "S", "category": category})
}

fn not_run_finding() -> Value {
    extra("check_not_run", &format!("{CHECK} did not run on the reviewed run"))
}

fn review(verdict: &str, extras: Vec<Value>) -> Value {
    json!({"verdict": verdict, "findings": {"criteria": [met("AC1"), met("AC2")], "extra_findings": extras}})
}

/// `review record TASK -f FILE --json` with `doc`.
fn record(f: &Fixture, task: &str, doc: &Value) -> Out {
    let tag = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let file = f.home_path().join(format!("review-{tag}.json"));
    std::fs::write(&file, doc.to_string()).unwrap();
    let file = file.display().to_string();
    exec(f, &["review", "record", task, "-f", &file, "--json"], &[])
}

fn of_type(f: &Fixture, task: &str, ty: &str) -> Vec<Value> {
    f.events_json(task).into_iter().filter(|e| e["type"] == ty).collect()
}

/// `doc` is refused: exit 1, one more `review_invalid` whose stage is
/// `required_checks`, and the task still `in_review`.
fn assert_refused(f: &Fixture, task: &str, doc: &Value) {
    let before = of_type(f, task, "review_invalid").len();
    let out = record(f, task, doc);
    assert_eq!(out.code, 1, "{doc}\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
    let invalid = of_type(f, task, "review_invalid");
    assert_eq!(invalid.len(), before + 1, "{doc}: {invalid:?}");
    assert_eq!(invalid.last().unwrap()["detail"]["stage"], "required_checks", "{doc}: {invalid:?}");
    assert_eq!(f.task_status(task), "in_review", "{doc}");
}

/// `doc` is taken; `review record --json`.
fn assert_taken(f: &Fixture, task: &str, doc: &Value) -> Value {
    let out = record(f, task, doc);
    assert_eq!(out.code, 0, "{doc}\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
    serde_json::from_str(out.stdout.trim()).unwrap()
}

/// The `detail` of the task's newest `verdict` event.
fn verdict_detail(f: &Fixture, task: &str) -> Value {
    of_type(f, task, "verdict").last().unwrap_or_else(|| panic!("no verdict event for {task}"))["detail"].clone()
}

fn retro(f: &Fixture) -> Value {
    let elsewhere = tempfile::tempdir().unwrap();
    let out = f.cmd().current_dir(elsewhere.path()).args(["retro", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// (1), (8), (9), (10) and AC4: no verify at all. The packet lists the check
/// as no_verify_row, in --json and in both Markdown outputs; a PASS, a REJECT
/// without a check_not_run finding and an UNVERIFIED with or without one are
/// refused; a REJECT with one is taken. Its finding is blocking in `review
/// show`, `retro` and the rework fold, while an evidence finding next to it
/// stays advisory.
#[test]
fn without_a_verify_only_a_reject_with_check_not_run_is_taken_and_it_blocks() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));

    // AC4: the packet
    assert_eq!(packet_not_run(&f, &task), not_run("no_verify_row"));
    let out = exec(&f, &["review", "packet", &task], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let section = packet_section(&out.stdout);
    assert!(section.contains(&format!("{CHECK} (no_verify_row)\n")), "{section}");
    assert!(!section.contains("(none)"), "{section}");
    let file = f.home_path().join("packet.md");
    let out = exec(&f, &["review", "packet", &task, "--out", &file.display().to_string()], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(packet_section(&written).contains(&format!("{CHECK} (no_verify_row)\n")), "{written}");

    // (1), (10): refused
    assert_refused(&f, &task, &review("PASS", vec![]));
    assert_refused(&f, &task, &review("REJECT", vec![extra("evidence", "the check has no verify row")]));
    assert_refused(&f, &task, &review("UNVERIFIED", vec![]));
    assert_refused(&f, &task, &review("UNVERIFIED", vec![not_run_finding()]));

    // (1): taken, and the verdict event records what it was checked against
    let v = assert_taken(&f, &task, &review("REJECT", vec![not_run_finding(), extra("evidence", "a claim cites nothing")]));
    assert_eq!(v["task_status"], "reject", "{v}");
    let d = verdict_detail(&f, &task);
    assert_eq!(d["checks_not_run"], not_run("no_verify_row"), "{d}");
    assert_eq!(d["has_blocking_findings"], true, "{d}");

    // (8) review show
    let shown = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(shown["blocking"], json!(["extra_findings[0]"]), "{shown}");
    assert_eq!(shown["advisory"], json!(["extra_findings[1]"]), "{shown}");
    assert_eq!(shown["has_blocking_findings"], true, "{shown}");

    // (8), (9) retro
    let r = retro(&f);
    let group = |name: &str| {
        r["categories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["category"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} group in {r}"))
    };
    assert_eq!(group("check_not_run")["blocking"], true, "{r}");
    assert_eq!(group("check_not_run")["count"], 1, "{r}");
    assert_eq!(group("evidence")["blocking"], false, "{r}");
    assert_eq!(r["blocking_findings"], 1, "{r}");
    assert_eq!(r["advisory_findings"], 1, "{r}");

    // (8) the rework folds it as a required finding
    let d = dispatch(&f, &task, &[]);
    assert_eq!(d["rework"], true, "{d}");
    assert_eq!(d["folded_findings"], 2, "{d}");
    assert_eq!(d["folded_advisory"], 1, "{d}");
    wait_in_review(&f, &task, d["run_id"].as_str().unwrap());
}

/// (2): a verify run only with --at-base proves nothing about the run.
#[test]
fn an_at_base_verify_alone_leaves_no_verify_row() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let v = verify(&f, &run, &["--at-base"]);
    let row = verify_row(&v);
    assert_eq!(row["exit_code"], 0, "the baseline check itself completed: {v}");
    assert_eq!(packet_not_run(&f, &task), not_run("no_verify_row"));
    assert_refused(&f, &task, &review("PASS", vec![]));
}

/// (3), (11): an unapproved check is refused by verify, so it did not
/// complete and a PASS is refused; once approved and verified again, the
/// same run has nothing missing and the PASS is taken.
#[test]
fn an_unapproved_check_did_not_complete_until_a_later_verify_completes_it() {
    let f = common::fixture();
    init(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    add_check(&f, CHECK, "exit 0");
    let v = verify(&f, &run, &[]);
    let row = verify_row(&v);
    assert!(row["error"].is_string(), "the unapproved check is refused with an error: {v}");
    assert_eq!(packet_not_run(&f, &task), not_run("did_not_complete"));
    assert_refused(&f, &task, &review("PASS", vec![]));

    approve(&f);
    let v = verify(&f, &run, &[]);
    assert_eq!(verify_row(&v)["exit_code"], 0, "{v}");
    assert_eq!(packet_not_run(&f, &task), json!([]));
    let taken = assert_taken(&f, &task, &review("PASS", vec![]));
    assert_eq!(taken["task_status"], "pass", "{taken}");
    assert_eq!(verdict_detail(&f, &task)["checks_not_run"], json!([]));
}

/// (11): one completed verify of the check is enough, even when a later
/// verify of the same run does not complete it (re-registering the check
/// cleared its approval).
#[test]
fn a_later_unfinished_verify_does_not_undo_a_completed_one() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let v = verify(&f, &run, &[]);
    assert_eq!(verify_row(&v)["exit_code"], 0, "{v}");
    add_check(&f, CHECK, "exit 0");
    let v = verify(&f, &run, &[]);
    assert!(verify_row(&v)["error"].is_string(), "the re-registered check is refused: {v}");
    assert_eq!(packet_not_run(&f, &task), json!([]));
}

/// (4): a check that ran and exited non-zero ran; it is not listed and a
/// PASS is taken (judging the failure stays the reviewer's).
#[test]
fn a_check_that_ran_and_failed_is_not_listed() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 3");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let v = verify(&f, &run, &[]);
    let row = verify_row(&v);
    assert_eq!(row["exit_code"], 3, "{v}");
    assert!(row["error"].is_null(), "{v}");
    assert_eq!(packet_not_run(&f, &task), json!([]));
    let taken = assert_taken(&f, &task, &review("PASS", vec![]));
    assert_eq!(taken["task_status"], "pass", "{taken}");
    let d = verdict_detail(&f, &task);
    assert_eq!(d["checks_not_run"], json!([]), "{d}");
    assert_eq!(d["has_blocking_findings"], false, "{d}");
}

/// (5) and AC5: a spec without `checks` requires nothing, even with an
/// approved check never verified; a PASS with an evidence finding is taken
/// as before, with no blocking finding.
#[test]
fn a_spec_without_checks_requires_nothing() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(None, "normal"));
    assert_eq!(packet_not_run(&f, &task), json!([]));
    let out = exec(&f, &["review", "packet", &task], &[]);
    assert!(packet_section(&out.stdout).contains("\n(none)\n"), "{}", out.stdout);
    let taken = assert_taken(&f, &task, &review("PASS", vec![extra("evidence", "cargo test --test review was not run")]));
    assert_eq!(taken["task_status"], "pass", "{taken}");
    let d = verdict_detail(&f, &task);
    assert_eq!(d["checks_not_run"], json!([]), "{d}");
    assert_eq!(d["has_blocking_findings"], false, "{d}");
}

/// (6), (10): a check that passed is not listed and a PASS is taken. With
/// nothing missing, a reviewer's own check_not_run finding still blocks: a
/// PASS or UNVERIFIED carrying one is refused, a REJECT carrying one taken.
#[test]
fn a_check_that_passed_is_not_listed_and_a_reported_check_not_run_still_needs_a_reject() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    verify(&f, &run, &[]);
    assert_eq!(packet_not_run(&f, &task), json!([]));
    let taken = assert_taken(&f, &task, &review("PASS", vec![]));
    assert_eq!(taken["task_status"], "pass", "{taken}");

    let (other, other_run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    verify(&f, &other_run, &[]);
    assert_eq!(packet_not_run(&f, &other), json!([]));
    assert_refused(&f, &other, &review("PASS", vec![not_run_finding()]));
    assert_refused(&f, &other, &review("UNVERIFIED", vec![not_run_finding()]));
    let taken = assert_taken(&f, &other, &review("REJECT", vec![not_run_finding()]));
    assert_eq!(taken["task_status"], "reject", "{taken}");
    let d = verdict_detail(&f, &other);
    assert_eq!(d["checks_not_run"], json!([]), "{d}");
    assert_eq!(d["has_blocking_findings"], true, "{d}");
}

/// (7): a reviewer run that delivers a PASS while the required check has not
/// run is refused, and the task lands unverified, not pass.
#[test]
fn a_reviewer_runs_pass_with_a_check_not_run_lands_unverified() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let copy = f.home_path().join("review-prompt.txt");
    let copy_s = copy.display().to_string();
    let r = ok(
        &f,
        &["review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "review_pass"), ("FAKE_TASK_ID", task.as_str()), ("FAKE_PROMPT_COPY", copy_s.as_str())],
    );
    let review_run = r["run_id"].as_str().unwrap_or_else(|| panic!("review start gave no run_id: {r}")).to_string();
    ok(&f, &["wait", "--run", &review_run, "--timeout", "180s", "--json"], &[]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status = ok(&f, &["status", &task, "--json"], &[]);
        if status["status"] == "unverified" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline && status["status"] != "pass",
            "task {task} is {}, not unverified\nevents: {:#?}",
            status["status"],
            f.events_json(&task)
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let invalid = of_type(&f, &task, "review_invalid");
    assert!(
        invalid.iter().any(|e| e["detail"]["stage"] == "required_checks" && e["run_id"] == review_run.as_str()),
        "{invalid:?}"
    );
    assert!(of_type(&f, &task, "verdict").is_empty(), "a refused report records no verdict");

    // AC2: the reviewer was told the rule and which check had not run
    let prompt = std::fs::read_to_string(&copy).unwrap();
    let rules = prompt.find("\n## Rules\n").unwrap_or_else(|| panic!("no ## Rules in:\n{prompt}"));
    let output = prompt.find("\n## Output\n").unwrap_or_else(|| panic!("no ## Output in:\n{prompt}"));
    assert!(prompt[rules..output].contains("category check_not_run"), "{prompt}");
    let listed = prompt.find("\n## Required checks not run\n").unwrap_or_else(|| panic!("no list in:\n{prompt}"));
    assert!(prompt[listed..].contains(&format!("\n- {CHECK} (no_verify_row)\n")), "{prompt}");
    assert!(prompt.contains("- check_not_run (blocking): "), "{prompt}");
}

/// `review start TASK MODE...` with fake-claude as a claude-a reviewer and
/// `envs` (FAKE_SCENARIO among them); checks `--json`'s `mode` is
/// `expect_mode`, waits for the reviewer run and returns its id.
fn start_review(f: &Fixture, task: &str, mode: &[&str], expect_mode: &str, envs: &[(&str, &str)]) -> String {
    let mut args = vec![
        "review", "start", task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--reason", REASON, "--json",
    ];
    args.extend_from_slice(mode);
    let mut all = vec![("FAKE_TASK_ID", task)];
    all.extend_from_slice(envs);
    let r = ok(f, &args, &all);
    assert_eq!(r["mode"], expect_mode, "review start {mode:?}: {r}");
    let run = r["run_id"].as_str().unwrap_or_else(|| panic!("review start gave no run_id: {r}")).to_string();
    ok(f, &["wait", "--run", &run, "--timeout", "180s", "--json"], &[]);
    run
}

/// The task's status from `status TASK --json` once it has left
/// `in_review` (within 30 s of its reviewer run ending).
fn settled_status(f: &Fixture, task: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status = ok(f, &["status", task, "--json"], &[]);
        let s = status["status"].as_str().unwrap_or_default().to_string();
        if s != "in_review" || std::time::Instant::now() >= deadline {
            return s;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// The fix for RFIX-2 x REV-3: a reviewer run's PASS on a code task whose
/// required check never ran, through `review start` with `mode`, is refused
/// and the task lands unverified, the refusal naming the check.
fn a_reviewer_pass_is_refused_in(mode: &[&str], expect_mode: &str) {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let review_run = start_review(&f, &task, mode, expect_mode, &[("FAKE_SCENARIO", "review_pass")]);
    let status = settled_status(&f, &task);
    assert_eq!(status, "unverified", "review start {mode:?}: task {task} is {status}\n{}", f.explain_task(&task));
    let invalid: Vec<Value> =
        of_type(&f, &task, "review_invalid").into_iter().filter(|e| e["run_id"] == review_run.as_str()).collect();
    assert_eq!(invalid.len(), 1, "review start {mode:?}: {invalid:?}");
    let d = &invalid[0]["detail"];
    assert_eq!(d["stage"], "required_checks", "{d}");
    assert_eq!(d["checks_not_run"], not_run("no_verify_row"), "{d}");
    let reason = d["reason"].as_str().unwrap_or_else(|| panic!("no reason: {d}"));
    assert!(reason.contains("PASS") && reason.contains(&format!("{CHECK} (no_verify_row)")), "{reason}");
    assert!(of_type(&f, &task, "verdict").is_empty(), "review start {mode:?}: a refused report records no verdict");
}

/// AC2: `review start` with no edit option (edit mode for a code task).
#[test]
fn a_default_mode_reviewer_pass_with_a_check_not_run_is_refused() {
    a_reviewer_pass_is_refused_in(&[], "edit");
}

/// AC2: `review start --edit`.
#[test]
fn an_edit_mode_reviewer_pass_with_a_check_not_run_is_refused() {
    a_reviewer_pass_is_refused_in(&["--edit"], "edit");
}

/// AC2: `review start --no-edit`.
#[test]
fn a_no_edit_reviewer_pass_with_a_check_not_run_is_refused() {
    a_reviewer_pass_is_refused_in(&["--no-edit"], "review");
}

/// AC4: with the required check run and completed on the reviewed run, a
/// default-mode reviewer PASS settles the task pass as before, and its
/// verdict records that nothing was missing.
#[test]
fn with_the_required_check_run_a_default_mode_pass_settles_pass() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let v = verify(&f, &run, &[]);
    assert_eq!(verify_row(&v)["exit_code"], 0, "{v}");
    let review_run = start_review(&f, &task, &[], "edit", &[("FAKE_SCENARIO", "review_pass")]);
    let status = settled_status(&f, &task);
    assert_eq!(status, "pass", "task {task} is {status}\n{}", f.explain_task(&task));
    assert!(of_type(&f, &task, "review_invalid").is_empty());
    let verdicts = of_type(&f, &task, "verdict");
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    assert_eq!(verdicts[0]["run_id"], review_run.as_str());
    let d = &verdicts[0]["detail"];
    assert_eq!(d["verdict"], "PASS", "{d}");
    assert_eq!(d["mode"], "edit", "{d}");
    assert_eq!(d["checks_not_run"], json!([]), "{d}");
}

/// AC3: a pending fix whose verify did not complete the required check
/// cannot be confirmed into pass: the confirmer's APPROVE settles the task
/// reject, with a blocking check_not_run finding naming the check, and the
/// verdict records what did not run on the fix.
#[test]
fn a_confirm_cannot_pass_a_fix_whose_required_check_did_not_run() {
    let f = common::fixture();
    let repo = f.repo_path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join(".gitattributes"), "* -text\n").unwrap();
    std::fs::write(repo.join("src/greet.rs"), "line 1\nline 2\n").unwrap();
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-m", "greeting"]);
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    // The candidate itself was verified, so the edit review's REJECT needs
    // no check_not_run finding and its fix is applied.
    assert_eq!(verify_row(&verify(&f, &run, &[]))["exit_code"], 0);
    let doc = json!({"verdict": "REJECT", "findings": {"criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": ["E001"], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"},
            met("AC2")], "extra_findings": []},
        "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}]});
    let plan = f.home_path().join("fix-plan.json");
    std::fs::write(&plan, json!({"document": doc}).to_string()).unwrap();
    let plan_s = plan.display().to_string();
    let edit_run =
        start_review(&f, &task, &["--edit"], "edit", &[("FAKE_SCENARIO", "review_fix"), ("FAKE_FIX_PLAN", plan_s.as_str())]);
    let shown = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(shown["pending_fix"], true, "{shown}\n{}", f.explain_task(&task));

    // Re-registering the check clears its approval: the fix's verify refuses
    // it, so it did not complete on the edit review run.
    add_check(&f, CHECK, "exit 0");
    let v = verify(&f, &edit_run, &[]);
    assert!(verify_row(&v)["error"].is_string(), "{v}");

    let c = ok(
        &f,
        &["review", "confirm", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "fix_confirm_approve"), ("FAKE_TASK_ID", task.as_str())],
    );
    let confirm_run = c["run_id"].as_str().unwrap_or_else(|| panic!("review confirm gave no run_id: {c}")).to_string();
    ok(&f, &["wait", "--run", &confirm_run, "--timeout", "180s", "--json"], &[]);
    let status = settled_status(&f, &task);
    assert_eq!(status, "reject", "task {task} is {status}, not reject\n{}", f.explain_task(&task));

    let shown = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(shown["verdict"], "REJECT", "{shown}");
    assert_eq!(shown["reviewer_run_id"], confirm_run.as_str(), "{shown}");
    assert_eq!(shown["has_blocking_findings"], true, "{shown}");
    let extras = shown["findings"]["extra_findings"].as_array().cloned().unwrap_or_default();
    let named: Vec<&Value> = extras
        .iter()
        .filter(|e| e["category"] == "check_not_run" && e["note"].as_str().is_some_and(|n| n.contains(CHECK)))
        .collect();
    assert_eq!(named.len(), 1, "one check_not_run finding naming {CHECK}: {extras:?}");
    let d = verdict_detail(&f, &task);
    assert_eq!(d["author_verdict"], "APPROVE", "{d}");
    assert_eq!(d["checks_not_run"], not_run("did_not_complete"), "{d}");
}

/// An edit review's REJECT whose fix is refused records a verdict, so it is
/// held to the rule like a plain REJECT: with the required check not run
/// and no check_not_run finding it is refused, the task lands unverified and
/// no fix is applied.
#[test]
fn an_edit_reject_whose_fix_is_refused_is_held_to_the_rule() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(Some(&[CHECK]), "normal"));
    let doc = json!({"verdict": "REJECT", "findings": {"criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": ["E001"], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"},
            met("AC2")], "extra_findings": []},
        "edits": [{"path": "src/no_such_file.rs", "old": "x\n", "new": "y\n", "criterion": "AC1", "reason": "fixes the finding"}]});
    let plan = f.home_path().join("fix-plan.json");
    std::fs::write(&plan, json!({"document": doc}).to_string()).unwrap();
    let plan_s = plan.display().to_string();
    let review_run =
        start_review(&f, &task, &["--edit"], "edit", &[("FAKE_SCENARIO", "review_fix"), ("FAKE_FIX_PLAN", plan_s.as_str())]);
    let status = settled_status(&f, &task);
    assert_eq!(status, "unverified", "task {task} is {status}\n{}", f.explain_task(&task));
    let invalid: Vec<Value> =
        of_type(&f, &task, "review_invalid").into_iter().filter(|e| e["run_id"] == review_run.as_str()).collect();
    assert_eq!(invalid.len(), 1, "{invalid:?}");
    let d = &invalid[0]["detail"];
    assert_eq!(d["stage"], "required_checks", "{d}");
    assert_eq!(d["checks_not_run"], not_run("no_verify_row"), "{d}");
    let reason = d["reason"].as_str().unwrap_or_else(|| panic!("no reason: {d}"));
    assert!(reason.contains(&format!("{CHECK} (no_verify_row)")) && reason.contains("its fix was refused"), "{reason}");
    assert!(of_type(&f, &task, "verdict").is_empty());
    assert!(of_type(&f, &task, "review_fix_applied").is_empty());
}

/// AC5: an important-risk task still rewrites its spec first; the rework
/// after a check_not_run REJECT folds nothing.
#[test]
fn an_important_task_reworks_after_a_spec_rewrite_and_folds_nothing() {
    let f = common::fixture();
    init(&f);
    add_check(&f, CHECK, "exit 0");
    approve(&f);
    let (task, _run) = task_in_review(&f, &spec(Some(&[CHECK]), "important"));
    let taken = assert_taken(&f, &task, &review("REJECT", vec![not_run_finding()]));
    assert_eq!(taken["task_status"], "reject", "{taken}");
    let rewritten = json!({"goal": "say hello twice, with the check run",
        "acceptance_criteria": ["AC1: prints hello", "AC2: exits zero", "AC3: the required check runs"],
        "constraints": [], "non_goals": [], "excluded_alternatives": [], "risk": "important", "checks": [CHECK]});
    let file = f.home_path().join("rewritten.json");
    std::fs::write(&file, rewritten.to_string()).unwrap();
    ok(&f, &["task", "update-spec", &task, "-f", &file.display().to_string(), "--json"], &[]);
    let d = dispatch(&f, &task, &[]);
    assert_eq!(d["rework"], true, "{d}");
    assert_eq!(d["folded_findings"], 0, "{d}");
    assert_eq!(d["folded_advisory"], 0, "{d}");
    wait_in_review(&f, &task, d["run_id"].as_str().unwrap());
}
