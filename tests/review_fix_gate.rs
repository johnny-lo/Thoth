//! GATE-1: a reviewer fix's protected changes are approved on the edit
//! review run before `review confirm` (AC1); a diff evidence row written
//! before LEAN-1 is recorded again by the gates rather than read as current
//! (AC2); a fix covers every extra finding that is not minor (AC3).
//! Everything goes through the CLI with fake workers, in a temporary
//! THOTH_HOME and repo, and is observed through `--json`, events and
//! evidence.

mod common;

use common::Fixture;
use serde_json::{json, Value};

const SPEC: &str = r#"{"goal":"keep the greeting right","acceptance_criteria":["AC1: greets the user","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;
const CODEX_MODEL: &str = "gpt-5.6-luna";
const REASON: &str = "gate fixture: fake workers in a temporary repo";
const PRE_LEAN: (&str, &str) = ("THOTH_TEST_DIFF_PRE_LEAN", "1");
const INLINE_BASE: &str = "pub fn one() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn one_is_one() {\n        assert_eq!(super::one(), 1);\n    }\n}\n";

// --- running thoth ------------------------------------------------------------

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names only are read), and without the two benchmark
/// keys.
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c.env_remove("AISTUPIDLEVEL_API_KEY").env_remove("ARTIFICIALANALYSIS_API_KEY");
    c.env_remove(PRE_LEAN.0);
    c
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}\nstderr: {}", self.stdout, self.stderr))
    }
}

fn exec(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let mut c = cmd(f);
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
    let mut c = cmd(f);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

/// A new file under THOTH_HOME holding `body`; its path.
fn home_file(f: &Fixture, stem: &str, body: &str) -> String {
    let tag = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let p = f.home_path().join(format!("{stem}-{tag}.json"));
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

// --- fixtures ---------------------------------------------------------------------

fn lines(prefix: &str, count: usize) -> String {
    (1..=count).map(|i| format!("{prefix} {i}\n")).collect()
}

/// The files every candidate starts with.
fn base_files() -> Vec<(&'static str, String)> {
    vec![
        ("src/greet.rs", lines("line", 30)),
        ("src/other.rs", lines("other", 5)),
        ("tests/greet_check.rs", "fn helper() {}\nfn checks() { let x = 1; }\n".into()),
        ("src/guarded.rs", "#[test]\nfn guarded() { assert_eq!(1, 1); }\n#[cfg(test)]\nmod t {}\nplain line\n".into()),
        ("src/inline.rs", INLINE_BASE.into()),
        (".gitattributes", "* -text\n".into()),
    ]
}

struct Setup {
    f: Fixture,
    task: String,
    implementer: String,
    candidate: String,
    code: String,
}

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let mut c = cmd(f);
    c.args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"]).arg(&cfg);
    f.run_ok(c, "profile add");
}

/// A task at `in_review` whose implementer ran `scenario` (with `envs`) on a
/// repo holding `base_files()`.
fn setup_with(scenario: &str, envs: &[(&str, &str)]) -> Setup {
    let f = common::fixture();
    let repo = f.repo_path();
    common::git(&repo, &["config", "core.autocrlf", "false"]);
    for (p, body) in base_files() {
        let path = repo.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-q", "-m", "candidate files"]);
    let mut c = cmd(&f);
    c.arg("init");
    f.run_ok(c, "init");
    add_profile(&f, "claude-b", "anthropic", "claude-code");
    add_profile(&f, "claude-a", "anthropic", "claude-code");
    add_profile(&f, "codex-a", "openai", "codex");
    let spec = home_file(&f, "spec", SPEC);
    let task = s(&ok(&f, &["task", "create", "-f", &spec, "--json"], &[])["task_id"]);
    let mut e = vec![("FAKE_SCENARIO", scenario), ("FAKE_TASK_ID", task.as_str())];
    e.extend_from_slice(envs);
    let d = ok(
        &f,
        &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &e,
    );
    let implementer = s(&d["run_id"]);
    assert_eq!(s(&ok(&f, &["wait", &task, "--timeout", "180s", "--json"], &[])["status"]), "in_review", "{}", f.explain_task(&task));
    let candidate = s(&run_of(&f, &task, &implementer)["candidate_commit"]);
    let code = s(&evidence(&f, &task)[0]["code"]);
    Setup { f, task, implementer, candidate, code }
}

fn setup() -> Setup {
    setup_with("ok", &[])
}

/// AC2: an implementer run, dispatched with `THOTH_TEST_DIFF_PRE_LEAN`, that
/// changes only an assertion inside `src/inline.rs`'s inline test region.
/// Returns the setup and the code of its pre-LEAN diff row.
fn pre_lean_implementer() -> (Setup, String) {
    let changed = INLINE_BASE.replace("assert_eq!(super::one(), 1)", "assert!(super::one() > 0)");
    let f_plan = json!({"write": [{"path": "src/inline.rs", "text": changed}]}).to_string();
    // the plan file has to exist before the fixture does; it lives in a
    // directory of its own that outlives the dispatch
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("plan.json");
    std::fs::write(&plan, f_plan).unwrap();
    let plan = plan.display().to_string();
    let st = setup_with("hooks", &[("FAKE_EDIT_PLAN", plan.as_str()), PRE_LEAN]);
    let rows = diff_rows(&st.f, &st.task, &st.implementer);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let detail = &rows[0]["detail"];
    assert_eq!(detail["protected_changes"], json!([]), "{detail:#}");
    assert!(detail.get("inline_test_errors").is_none(), "{detail:#}");
    assert!(
        detail["files_changed"].as_array().unwrap().iter().any(|p| p == "src/inline.rs"),
        "{detail:#}"
    );
    let code = s(&rows[0]["code"]);
    (st, code)
}

// --- reading -----------------------------------------------------------------------

fn evidence(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["evidence", "list", task, "--json"], &[])["evidence"].as_array().unwrap().clone()
}

fn code_number(code: &str) -> u64 {
    code.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap()
}

/// A run's diff evidence rows, oldest first.
fn diff_rows(f: &Fixture, task: &str, run: &str) -> Vec<Value> {
    let mut rows: Vec<Value> =
        evidence(f, task).into_iter().filter(|e| e["kind"] == "diff" && e["run_id"] == run).collect();
    rows.sort_by_key(|e| code_number(&s(&e["code"])));
    rows
}

fn run_of(f: &Fixture, task: &str, run: &str) -> Value {
    ok(f, &["status", task, "--json"], &[])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run)
        .cloned()
        .unwrap_or_else(|| panic!("task {task} has no run {run}"))
}

fn run_count(f: &Fixture, task: &str) -> usize {
    ok(f, &["status", task, "--json"], &[])["runs"].as_array().unwrap().len()
}

fn events(f: &Fixture, task: &str, ty: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

fn last_event(f: &Fixture, task: &str, ty: &str) -> Value {
    events(f, task, ty).last().cloned().unwrap_or_else(|| panic!("no {ty} event\n{}", f.explain_task(task)))["detail"].clone()
}

fn show(st: &Setup) -> Value {
    ok(&st.f, &["review", "show", &st.task, "--json"], &[])
}

fn wait_run(f: &Fixture, run: &str) -> Value {
    ok(f, &["wait", "--run", run, "--timeout", "180s", "--json"], &[])
}

fn entry(path: &str, kind: &str) -> Value {
    json!({"path": path, "status": "modified", "kind": kind})
}

// --- review steps -------------------------------------------------------------------

fn edit(path: &str, old: &str, new: &str, criterion: &str) -> Value {
    json!({"path": path, "old": old, "new": new, "criterion": criterion, "reason": "fixes the finding"})
}

fn extra(severity: &str, note: &str, size: &str) -> Value {
    json!({"severity": severity, "note": note, "evidence": [], "fix_size": size, "category": "docs"})
}

/// A REJECT: AC1 unmet (size S) unless `ac1_met`, AC2 met, `extras`, `edits`.
fn reject(st: &Setup, ac1_met: bool, extras: Value, edits: Value) -> Value {
    let ac1 = if ac1_met {
        json!({"id": "AC1", "verdict": "met", "evidence": [st.code]})
    } else {
        json!({"id": "AC1", "verdict": "unmet", "evidence": [st.code], "note": "the greeting is wrong", "fix_size": "S", "category": "spec_mismatch"})
    };
    json!({
        "verdict": "REJECT",
        "findings": {"criteria": [ac1, {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": extras},
        "edits": edits,
    })
}

/// A PASS over both criteria; `edits` [] when `edit_mode`.
fn pass_doc(st: &Setup, edit_mode: bool) -> Value {
    let mut d = json!({"verdict": "PASS", "findings": {"criteria": [
        {"id": "AC1", "verdict": "met", "evidence": [st.code]},
        {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": []}});
    if edit_mode {
        d["edits"] = json!([]);
    }
    d
}

fn review_args(task: &str, edit_mode: bool) -> Vec<&str> {
    let mut a = vec!["review", "start", task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--reason", REASON, "--json"];
    if edit_mode {
        a.push("--edit");
    }
    a
}

/// `review start --edit` whose fake reviewer hands in `document`, with
/// `envs`; waits for the run and returns (run, `wait --run --json`).
fn edit_review(st: &Setup, document: &Value, envs: &[(&str, &str)]) -> (String, Value) {
    let plan = home_file(&st.f, "plan", &json!({"document": document}).to_string());
    let mut e = vec![("FAKE_SCENARIO", "review_fix"), ("FAKE_TASK_ID", st.task.as_str()), ("FAKE_FIX_PLAN", plan.as_str())];
    e.extend_from_slice(envs);
    let started = ok(&st.f, &review_args(&st.task, true), &e);
    assert_eq!(started["mode"], "edit", "{started}");
    let run = s(&started["run_id"]);
    let w = wait_run(&st.f, &run);
    (run, w)
}

/// An edit review whose fix is applied and pending; (edit run, fix commit).
fn applied_fix(st: &Setup, document: &Value, envs: &[(&str, &str)]) -> (String, String) {
    let (run, w) = edit_review(st, document, envs);
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["pending_fix"], true, "{}", st.f.explain_task(&st.task));
    let fix = s(&last_event(&st.f, &st.task, "review_fix_applied")["fix_commit"]);
    (run, fix)
}

/// Approved checks: one that passes, so a verify of the fix is green.
fn add_green_check(st: &Setup) {
    ok(&st.f, &["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"], &[]);
    ok(&st.f, &["checks", "approve", "--reason", "test approval", "--json"], &[]);
}

fn verify_green(st: &Setup, run: &str) {
    let out = exec(&st.f, &["verify", run, "--json"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
}

fn confirm_args(task: &str) -> Vec<&str> {
    vec!["review", "confirm", task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--reason", REASON, "--json"]
}

fn confirm_env(task: &str) -> [(&str, &str); 2] {
    [("FAKE_SCENARIO", "fix_confirm_approve"), ("FAKE_TASK_ID", task)]
}

/// `review confirm --json` that must exit 1 and start no run; the `error`
/// of the JSON document it writes to stderr.
fn confirm_refused(st: &Setup) -> String {
    let before = run_count(&st.f, &st.task);
    let started_before = events(&st.f, &st.task, "review_started").len();
    let out = exec(&st.f, &confirm_args(&st.task), &confirm_env(&st.task));
    assert_eq!(out.code, 1, "stdout {}\nstderr {}", out.stdout, out.stderr);
    assert_eq!(run_count(&st.f, &st.task), before, "a refused confirm writes no run row");
    assert_eq!(events(&st.f, &st.task, "review_started").len(), started_before);
    assert!(
        !events(&st.f, &st.task, "review_started").iter().any(|e| e["detail"]["mode"] == "confirm"),
        "no confirm run was started"
    );
    let v: Value = serde_json::from_str(out.stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", out.stderr));
    assert_eq!(v["ok"], false, "{v}");
    s(&v["error"])
}

/// A confirm that starts and passes the task on the fix branch.
fn confirm_passes(st: &Setup) {
    let c = ok(&st.f, &confirm_args(&st.task), &confirm_env(&st.task));
    assert_eq!(c["mode"], "confirm", "{c}");
    let w = wait_run(&st.f, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    assert_eq!(show(st)["merge_branch"], format!("thoth/{}/a1-fix", st.task));
}

fn unapproved(task: &str, run: &str, listed: &str) -> String {
    format!(
        "refusing to start the confirm of task {task}: run {run} changed protected files that nobody has approved yet: {listed}; read the change (thoth review packet {task}), then approve it with: thoth checks approve --run {run} --reason \"WHY\" (MCP: thoth_checks_approve)"
    )
}

fn approve(st: &Setup, run: &str) -> Out {
    let out = exec(&st.f, &["checks", "approve", "--run", run, "--reason", "R", "--json"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    out
}

// --- AC1: a fix's protected changes are approved on the edit review run ------------

/// AC1 (a) and (b): the confirm waits for `checks approve --run EDIT_RUN`;
/// an approval of the implementer run does not cover the fix.
fn fix_needs_approval_on_the_edit_run(fix_edit: Value, path: &str, kind: &str) {
    let st = setup();
    add_green_check(&st);
    let (edit_run, _) = applied_fix(&st, &reject(&st, false, json!([]), json!([fix_edit])), &[]);
    verify_green(&st, &edit_run);
    let listed = format!("{path} (modified, {kind})");
    let err = confirm_refused(&st);
    assert!(err.contains(&unapproved(&st.task, &edit_run, &listed)), "{err}");

    approve(&st, &st.implementer);
    let err = confirm_refused(&st);
    assert!(err.contains(&unapproved(&st.task, &edit_run, &listed)), "an approval of the implementer run does not cover the fix: {err}");

    let v = approve(&st, &edit_run).json();
    assert_eq!(v["protected_approved"], json!([entry(path, kind)]), "{v:#}");
    confirm_passes(&st);
}

#[test]
fn a_fix_that_adds_lines_to_an_existing_test_needs_approval_on_the_edit_run() {
    fix_needs_approval_on_the_edit_run(
        edit("tests/greet_check.rs", "fn helper() {}\n", "fn helper() {}\nfn more() {}\n", "AC1"),
        "tests/greet_check.rs",
        "test",
    );
}

#[test]
fn a_fix_inside_an_inline_test_region_needs_approval_on_the_edit_run() {
    fix_needs_approval_on_the_edit_run(
        edit("src/guarded.rs", "plain line\n", "plain line, fixed\n", "AC1"),
        "src/guarded.rs",
        "inline_test",
    );
}

/// AC1 (c): a fix without protected changes confirms with no approval.
#[test]
fn a_fix_without_protected_changes_confirms_as_before() {
    let st = setup();
    add_green_check(&st);
    let (edit_run, _) = applied_fix(&st, &reject(&st, false, json!([]), json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")])), &[]);
    verify_green(&st, &edit_run);
    assert_eq!(diff_rows(&st.f, &st.task, &edit_run).len(), 1, "the fix's diff, read as it is");
    confirm_passes(&st);
    assert_eq!(diff_rows(&st.f, &st.task, &edit_run).len(), 1, "a readable fix diff is not recorded again");
}

// --- AC2: a diff row written before LEAN-1 is recorded again ------------------------

/// `checks approve --run RUN` approves nothing from a pre-LEAN row and warns.
fn approve_warns(st: &Setup, run: &str, code: &str, again: &str) {
    let out = approve(st, run);
    assert_eq!(out.json()["protected_approved"], json!([]), "{}", out.stdout);
    let warning = format!("warning: run {run}'s diff evidence {code} has no readable protected_changes; run {again}, then approve again");
    assert!(out.stderr.contains(&warning), "{}", out.stderr);
}

/// The run's diff evidence recorded again after `old`: newer, with an
/// `inline_test_errors` array, listing `listed`. Returns the new row.
fn recorded_again(st: &Setup, run: &str, old: &str, listed: &Value) -> Value {
    let rows = diff_rows(&st.f, &st.task, run);
    let newest = rows.last().unwrap().clone();
    assert!(code_number(&s(&newest["code"])) > code_number(old), "{rows:#?}");
    assert_eq!(newest["detail"]["inline_test_errors"], json!([]), "{newest:#}");
    assert!(newest["detail"]["protected_changes"].as_array().unwrap().contains(listed), "{newest:#}");
    newest
}

#[test]
fn review_start_records_a_pre_lean_diff_again_and_judges_the_new_row() {
    let (st, code) = pre_lean_implementer();
    approve_warns(&st, &st.implementer, &code, "review start");
    let env = [("FAKE_SCENARIO", "review_pass"), ("FAKE_TASK_ID", st.task.as_str())];
    let refused = exec(&st.f, &review_args(&st.task, false), &env);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    assert!(refused.stderr.contains("src/inline.rs (modified, inline_test)"), "{}", refused.stderr);
    recorded_again(&st, &st.implementer, &code, &entry("src/inline.rs", "inline_test"));
    let v = approve(&st, &st.implementer).json();
    assert_eq!(v["protected_approved"], json!([entry("src/inline.rs", "inline_test")]), "{v:#}");
    let started = ok(&st.f, &review_args(&st.task, false), &env);
    wait_run(&st.f, &s(&started["run_id"]));
    assert_eq!(st.f.wait_for_task_status(&st.task, &["pass", "reject", "unverified"], 60), "pass");
}

#[test]
fn review_start_edit_records_a_pre_lean_diff_again_and_judges_the_new_row() {
    let (st, code) = pre_lean_implementer();
    approve_warns(&st, &st.implementer, &code, "review start");
    let plan = home_file(&st.f, "plan", &json!({"document": pass_doc(&st, true)}).to_string());
    let env = [("FAKE_SCENARIO", "review_fix"), ("FAKE_TASK_ID", st.task.as_str()), ("FAKE_FIX_PLAN", plan.as_str())];
    let refused = exec(&st.f, &review_args(&st.task, true), &env);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    assert!(refused.stderr.contains("src/inline.rs (modified, inline_test)"), "{}", refused.stderr);
    recorded_again(&st, &st.implementer, &code, &entry("src/inline.rs", "inline_test"));
    let v = approve(&st, &st.implementer).json();
    assert_eq!(v["protected_approved"], json!([entry("src/inline.rs", "inline_test")]), "{v:#}");
    let started = ok(&st.f, &review_args(&st.task, true), &env);
    assert_eq!(started["mode"], "edit");
    wait_run(&st.f, &s(&started["run_id"]));
    assert_eq!(st.f.wait_for_task_status(&st.task, &["pass", "reject", "unverified"], 60), "pass");
}

#[test]
fn a_pass_review_record_records_a_pre_lean_diff_again_and_judges_the_new_row() {
    let (st, code) = pre_lean_implementer();
    approve_warns(&st, &st.implementer, &code, "review start");
    let pass = home_file(&st.f, "pass", &pass_doc(&st, false).to_string());
    let refused = exec(&st.f, &["review", "record", &st.task, "-f", &pass, "--json"], &[]);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    assert!(refused.stderr.contains(&format!("refusing to record a PASS for task {}", st.task)), "{}", refused.stderr);
    assert!(refused.stderr.contains("src/inline.rs (modified, inline_test)"), "{}", refused.stderr);
    recorded_again(&st, &st.implementer, &code, &entry("src/inline.rs", "inline_test"));
    let v = approve(&st, &st.implementer).json();
    assert_eq!(v["protected_approved"], json!([entry("src/inline.rs", "inline_test")]), "{v:#}");
    ok(&st.f, &["review", "record", &st.task, "-f", &pass, "--json"], &[]);
    assert_eq!(s(&ok(&st.f, &["status", &st.task, "--json"], &[])["status"]), "pass");
}

#[test]
fn review_confirm_records_a_pre_lean_fix_diff_again_and_judges_the_new_row() {
    let st = setup();
    add_green_check(&st);
    let fix_edit = edit("src/guarded.rs", "plain line\n", "plain line, fixed\n", "AC1");
    let (edit_run, fix) = applied_fix(&st, &reject(&st, false, json!([]), json!([fix_edit])), &[PRE_LEAN]);
    let rows = diff_rows(&st.f, &st.task, &edit_run);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let code = s(&rows[0]["code"]);
    assert_eq!(rows[0]["detail"]["protected_changes"], json!([]), "{:#}", rows[0]);
    assert!(rows[0]["detail"].get("inline_test_errors").is_none(), "{:#}", rows[0]);
    verify_green(&st, &edit_run);
    approve_warns(&st, &edit_run, &code, "review confirm");

    let err = confirm_refused(&st);
    assert!(err.contains(&unapproved(&st.task, &edit_run, "src/guarded.rs (modified, inline_test)")), "{err}");
    let newest = recorded_again(&st, &edit_run, &code, &entry("src/guarded.rs", "inline_test"));
    assert!(s(&newest["label"]).starts_with("review fix: "), "{newest:#}");
    assert_eq!(newest["detail"]["from_commit"], st.candidate.as_str());
    assert_eq!(newest["detail"]["to_commit"], fix.as_str());

    let v = approve(&st, &edit_run).json();
    assert_eq!(v["protected_approved"], json!([entry("src/guarded.rs", "inline_test")]), "{v:#}");
    confirm_passes(&st);
}

/// AC2: a row written with `inline_test_errors` is read as it is.
#[test]
fn a_current_diff_row_is_not_recorded_again() {
    let changed = INLINE_BASE.replace("assert_eq!(super::one(), 1)", "assert!(super::one() > 0)");
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("plan.json");
    std::fs::write(&plan, json!({"write": [{"path": "src/inline.rs", "text": changed}]}).to_string()).unwrap();
    let plan = plan.display().to_string();
    let st = setup_with("hooks", &[("FAKE_EDIT_PLAN", plan.as_str())]);
    let rows = diff_rows(&st.f, &st.task, &st.implementer);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["detail"]["inline_test_errors"], json!([]));
    let env = [("FAKE_SCENARIO", "review_pass"), ("FAKE_TASK_ID", st.task.as_str())];
    let refused = exec(&st.f, &review_args(&st.task, false), &env);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    assert!(refused.stderr.contains("src/inline.rs (modified, inline_test)"), "{}", refused.stderr);
    assert_eq!(diff_rows(&st.f, &st.task, &st.implementer).len(), 1, "no second diff row");
    approve(&st, &st.implementer);
    let started = ok(&st.f, &review_args(&st.task, false), &env);
    wait_run(&st.f, &s(&started["run_id"]));
    assert_eq!(diff_rows(&st.f, &st.task, &st.implementer).len(), 1, "no second diff row");
}

// --- AC3: a fix covers every extra finding that is not minor ------------------------

/// A refused fix at `size` with `reason`: the REJECT settles the task, the
/// fix is not applied, the confirm is refused and `dispatch` reworks.
fn refused_at_size(st: &Setup, document: &Value, reason: &str) {
    let (_, w) = edit_review(st, document, &[]);
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["task_status"], "reject", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["pending_fix"], false);
    let refused = last_event(&st.f, &st.task, "review_fix_refused");
    assert_eq!(refused["stage"], "size", "{refused}");
    assert_eq!(refused["reason"], reason, "{refused}");
    let shown = show(st);
    assert_eq!(shown["pending_fix"], false);
    assert_eq!(shown["fix"]["applied"], false);
    assert_eq!(shown["fix"]["stage"], "size");
    let err = confirm_refused(st);
    assert!(err.contains("needs in_review"), "{err}");
    let d = ok(
        &st.f,
        &["dispatch", &st.task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "ok"), ("FAKE_TASK_ID", st.task.as_str())],
    );
    assert_eq!(d["attempt"], 2, "{d}");
    assert_eq!(s(&ok(&st.f, &["wait", &st.task, "--timeout", "180s", "--json"], &[])["status"]), "in_review");
}

#[test]
fn a_major_extra_finding_without_an_edit_refuses_the_fix() {
    let st = setup();
    let document = reject(
        &st,
        true,
        json!([extra("major", "first", "S"), extra("major", "second", "L")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "extra_findings[0]")]),
    );
    refused_at_size(
        &st,
        &document,
        "extra_findings[1] (severity major) has no edit; a fix covers every unmet criterion and every extra finding that is not minor, or the task is redone",
    );
}

#[test]
fn an_unknown_severity_without_an_edit_refuses_the_fix() {
    let st = setup();
    let document = reject(
        &st,
        false,
        json!([extra("Important", "unbound", "S")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")]),
    );
    refused_at_size(
        &st,
        &document,
        "extra_findings[0] (severity Important) has no edit; a fix covers every unmet criterion and every extra finding that is not minor, or the task is redone",
    );
}

#[test]
fn a_minor_extra_finding_may_stay_and_the_pass_keeps_it() {
    let st = setup();
    add_green_check(&st);
    let document = reject(
        &st,
        false,
        json!([extra("major", "bound", "S"), extra(" Minor ", "left for later", "S")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1"), edit("src/other.rs", "other 2\n", "other two\n", "extra_findings[0]")]),
    );
    let (edit_run, _) = applied_fix(&st, &document, &[]);
    verify_green(&st, &edit_run);
    confirm_passes(&st);
    let kept = show(&st)["findings"]["extra_findings"].clone();
    let notes: Vec<String> = kept.as_array().unwrap().iter().map(|e| s(&e["note"])).collect();
    assert_eq!(notes, ["left for later"], "{kept:#}");
}
