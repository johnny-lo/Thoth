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
        // GITENV-1: with GIT_CONFIG_KEY_n the count and values go too, so git never sees a count without its keys
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) || upper == "GIT_CONFIG_COUNT" || upper.starts_with("GIT_CONFIG_VALUE_") {
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
        // RFIX-2: more files to edit, a check input and a Thoth spec
        ("src/third.rs", lines("third", 5)),
        ("src/fourth.rs", lines("fourth", 5)),
        ("config/check_in.txt", "input\n".into()),
        ("config/data.json", "{\"name\": \"x\"}\n".into()),
        ("plans/spec.json", format!("{SPEC}\n")),
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
    setup_full(SPEC, scenario, envs, |_| {})
}

/// `setup_with` for a task of `spec`, with `before_task` run (checks added,
/// say) after `init` and before the task is created.
fn setup_full(spec: &str, scenario: &str, envs: &[(&str, &str)], before_task: impl Fn(&Fixture)) -> Setup {
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
    before_task(&f);
    let spec = home_file(&f, "spec", spec);
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

/// REV-2: `finding` with its category replaced.
fn labelled(mut finding: Value, category: &str) -> Value {
    finding["category"] = json!(category);
    finding
}

/// REV-2 (AC3): the rule a `size` refusal for a missing edit spells out.
const SIZE_COVERAGE: &str = "a fix covers every blocking unmet criterion and every extra finding that is neither minor nor advisory (an advisory category or a minor severity needs no edit; no category counts as blocking), or the task is redone";

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
    assert_eq!(ok(&st.f, &["status", &st.task, "--json"], &[])["status"], "pass");
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
    assert_eq!(ok(&st.f, &["status", &st.task, "--json"], &[])["status"], "pass");
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
        // REV-2: `extra` labels a finding docs, which is advisory and needs
        // no edit; the unbound one is labelled error_path (blocking).
        json!([extra("major", "first", "S"), labelled(extra("major", "second", "L"), "error_path")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "extra_findings[0]")]),
    );
    refused_at_size(
        &st,
        &document,
        &format!("extra_findings[1] (severity major, category error_path) has no edit; {SIZE_COVERAGE}"),
    );
}

#[test]
fn an_unknown_severity_without_an_edit_refuses_the_fix() {
    let st = setup();
    let document = reject(
        &st,
        false,
        json!([labelled(extra("Important", "unbound", "S"), "error_path")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")]),
    );
    refused_at_size(
        &st,
        &document,
        &format!("extra_findings[0] (severity Important, category error_path) has no edit; {SIZE_COVERAGE}"),
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

// --- REV-2 (AC3, AC4): advisory findings need no edit, and a PASS keeps them -------

/// An edit-mode REJECT: AC1 unmet (spec_mismatch, S) with an edit bound to
/// it; ① when `extra_category` is given, one major extra finding of that
/// category with no edit; ② when `ac2_category` is given, AC2 unmet with that
/// category, fix_size S and no edit (otherwise AC2 is met).
fn advisory_doc(st: &Setup, extra_category: Option<&str>, ac2_category: Option<&str>) -> Value {
    let extras: Vec<Value> = extra_category
        .map(|c| labelled(extra("major", "the usage text is stale", "S"), c))
        .into_iter()
        .collect();
    let mut document = reject(st, false, json!(extras), json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")]));
    if let Some(c) = ac2_category {
        document["findings"]["criteria"][1] = json!({
            "id": "AC2", "verdict": "unmet", "evidence": [st.code], "note": "the exit code is not documented",
            "fix_size": "S", "category": c,
        });
    }
    document
}

/// AC7 (d) ①: a major extra finding labelled docs may go without an edit;
/// labelled error_path it refuses the fix at `size`, naming it.
#[test]
fn rev2_an_advisory_extra_finding_without_an_edit_lets_the_fix_apply() {
    let st = setup();
    applied_fix(&st, &advisory_doc(&st, Some("docs"), None), &[]);
    let shown = show(&st);
    assert_eq!(shown["fix"]["applied"], true, "{shown}");
    assert_eq!(shown["pending_fix"], true, "{shown}");

    let st = setup();
    refused_at_size(
        &st,
        &advisory_doc(&st, Some("error_path"), None),
        &format!("extra_findings[0] (severity major, category error_path) has no edit; {SIZE_COVERAGE}"),
    );
}

/// AC7 (d) ②: an unmet AC2 labelled docs may go without an edit; labelled
/// error_path it refuses the fix at `size`, naming AC2.
#[test]
fn rev2_an_advisory_unmet_criterion_without_an_edit_lets_the_fix_apply() {
    let st = setup();
    applied_fix(&st, &advisory_doc(&st, None, Some("docs")), &[]);
    let shown = show(&st);
    assert_eq!(shown["fix"]["applied"], true, "{shown}");
    assert_eq!(shown["pending_fix"], true, "{shown}");

    let st = setup();
    refused_at_size(
        &st,
        &advisory_doc(&st, None, Some("error_path")),
        &format!("unmet criterion AC2 (category error_path) has no edit; {SIZE_COVERAGE}"),
    );
}

/// AC7 (e): ① and ② in one fix, verified and confirmed. The PASS record
/// meets AC1 through the fix, keeps AC2 unmet exactly as the edit review
/// judged it and keeps the docs extra finding with its category; `review
/// show` lists both as advisory, and the confirm's verdict event says it has
/// no blocking finding.
#[test]
fn rev2_a_confirmed_fix_keeps_the_advisory_findings_it_did_not_fix() {
    let st = setup();
    add_green_check(&st);
    let document = advisory_doc(&st, Some("docs"), Some("docs"));
    let (edit_run, _) = applied_fix(&st, &document, &[]);
    let edit_record = show(&st);
    let edit_ac2 = edit_record["findings"]["criteria"][1].clone();
    assert_eq!(edit_ac2["id"], "AC2", "{edit_record}");
    verify_green(&st, &edit_run);
    confirm_passes(&st);

    let shown = show(&st);
    let criteria = shown["findings"]["criteria"].as_array().unwrap();
    let ac1 = criteria.iter().find(|c| c["id"] == "AC1").unwrap();
    let ac2 = criteria.iter().find(|c| c["id"] == "AC2").unwrap();
    assert_eq!(ac1["verdict"], "met", "{shown}");
    assert_eq!(ac2["verdict"], "unmet", "{shown}");
    assert_eq!(ac2["category"], "docs", "{shown}");
    assert_eq!(ac2["fix_size"], "S", "{shown}");
    assert_eq!(ac2["note"], edit_ac2["note"], "{shown}");
    assert_eq!(ac2["evidence"], edit_ac2["evidence"], "{shown}");
    assert_eq!(shown["findings"]["extra_findings"][0]["category"], "docs", "{shown}");
    assert_eq!(shown["findings"]["extra_findings"][0]["fix_size"], "S", "{shown}");
    assert_eq!(shown["advisory"], json!(["AC2", "extra_findings[0]"]), "{shown}");
    assert_eq!(shown["blocking"], json!([]), "{shown}");
    assert_eq!(shown["has_blocking_findings"], false, "{shown}");

    let confirm_verdict = events(&st.f, &st.task, "verdict")
        .into_iter()
        .rfind(|e| e["detail"]["mode"] == "confirm")
        .unwrap_or_else(|| panic!("no confirm verdict event\n{}", st.f.explain_task(&st.task)));
    assert_eq!(confirm_verdict["detail"]["verdict"], "PASS", "{confirm_verdict}");
    assert_eq!(confirm_verdict["detail"]["has_blocking_findings"], false, "{confirm_verdict}");
}

// --- RFIX-2: a docs or review task's fix goes through the code task's gates --------

/// `SPEC` as a task of `task_type`, with `checks` (raw JSON) when given.
fn typed_spec(task_type: &str, checks: Option<&str>) -> String {
    let mut extra = format!(",\"task_type\":\"{task_type}\"");
    if let Some(c) = checks {
        extra.push_str(&format!(",\"checks\":{c}"));
    }
    SPEC.replacen("\"risk\":\"normal\"", &format!("\"risk\":\"normal\"{extra}"), 1)
}

fn setup_typed(task_type: &str) -> Setup {
    setup_full(&typed_spec(task_type, None), "ok", &[], |_| {})
}

/// A REJECT whose unmet AC1 is sized `size` (`None`: no size) with `edits`.
fn reject_sized(st: &Setup, size: Option<&str>, edits: Value) -> Value {
    let mut d = reject(st, false, json!([]), edits);
    match size {
        Some(sz) => d["findings"]["criteria"][0]["fix_size"] = json!(sz),
        None => {
            d["findings"]["criteria"][0].as_object_mut().unwrap().remove("fix_size");
        }
    }
    d
}

/// The one-line fix of AC1 every passing case uses.
fn small_edit() -> Value {
    json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")])
}

/// Adds `id` running `git --version`, optionally depending on `input`.
fn add_check(f: &Fixture, id: &str, input: Option<&str>) {
    let mut args = vec!["checks", "add", id, "--cmd", "git --version", "--shell", "argv", "--json"];
    if let Some(i) = input {
        args.extend(["--input", i]);
    }
    ok(f, &args, &[]);
}

fn approve_all(f: &Fixture) {
    ok(f, &["checks", "approve", "--reason", "test approval", "--json"], &[]);
}

/// The fix `edits` of a size-`size` REJECT handed in by a `--edit` review of
/// a fresh `task_type` task (with `checks_first` run before the review):
/// applied (`stage` None), or refused at `stage`.
fn fix_case(task_type: &str, size: Option<&str>, edits: Value, checks_first: impl Fn(&Fixture), stage: Option<&str>) {
    let st = setup_typed(task_type);
    checks_first(&st.f);
    let (run, w) = edit_review(&st, &reject_sized(&st, size, edits.clone()), &[]);
    assert_eq!(w["timed_out"], false, "{w}");
    assert_eq!(w["verdict"], "REJECT", "{task_type} {edits}: {}", st.f.explain_task(&st.task));
    let fix_events: Vec<Value> = ok(&st.f, &["events", "--since", "0", "--task", &st.task, "--json"], &[])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["run_id"] == run.as_str() && (e["type"] == "review_fix_applied" || e["type"] == "review_fix_refused"))
        .cloned()
        .collect();
    assert_eq!(fix_events.len(), 1, "{task_type} {edits}: {fix_events:#?}");
    match stage {
        None => {
            assert_eq!(fix_events[0]["type"], "review_fix_applied", "{task_type} {edits}: {:#}", fix_events[0]);
            assert_eq!(w["task_status"], "in_review", "{w}");
            assert_eq!(w["pending_fix"], true, "{w}");
            let shown = show(&st);
            assert_eq!(shown["fix"]["applied"], true, "{shown}");
            assert_eq!(shown["mode"], "edit", "{shown}");
        }
        Some(want) => {
            assert_eq!(fix_events[0]["type"], "review_fix_refused", "{task_type} {edits}: {:#}", fix_events[0]);
            assert_eq!(fix_events[0]["detail"]["stage"], want, "{task_type} {edits}: {:#}", fix_events[0]);
            assert_eq!(w["task_status"], "reject", "{w}");
            assert_eq!(w["pending_fix"], false, "{w}");
        }
    }
}

/// RFIX-2 AC6 (1): on a docs or review task, an S or M fix is applied; an L
/// or unsized finding, more than 40 changed lines, more than 3 files, a
/// changed test line, a check input and a check-word file are refused at
/// the code task's stages. Each case is a task of its own.
fn sizes_and_bounds_on(task_type: &'static str) {
    let one = small_edit;
    std::thread::scope(|scope| {
        scope.spawn(|| fix_case(task_type, Some("S"), one(), |_| {}, None));
        scope.spawn(|| fix_case(task_type, Some("M"), one(), |_| {}, None));
        scope.spawn(|| fix_case(task_type, Some("L"), one(), |_| {}, Some("size")));
        scope.spawn(|| fix_case(task_type, None, one(), |_| {}, Some("size")));
        scope.spawn(|| {
            fix_case(task_type, Some("S"), json!([edit("src/greet.rs", &lines("line", 21), &lines("LINE", 21), "AC1")]), |_| {}, Some("bounds"))
        });
        scope.spawn(|| {
            let four = json!([
                edit("src/greet.rs", "line 1\n", "g\n", "AC1"),
                edit("src/other.rs", "other 1\n", "o\n", "AC1"),
                edit("src/third.rs", "third 1\n", "t\n", "AC1"),
                edit("src/fourth.rs", "fourth 1\n", "f\n", "AC1")
            ]);
            fix_case(task_type, Some("S"), four, |_| {}, Some("bounds"))
        });
        scope.spawn(|| {
            fix_case(task_type, Some("S"), json!([edit("tests/greet_check.rs", "let x = 1;", "let x = 2;", "AC1")]), |_| {}, Some("tests"))
        });
        scope.spawn(|| {
            fix_case(
                task_type,
                Some("S"),
                json!([edit("config/check_in.txt", "input\n", "changed\n", "AC1")]),
                |f| add_check(f, "reads-input", Some("config/check_in.txt")),
                Some("protected"),
            )
        });
        scope.spawn(|| {
            fix_case(
                task_type,
                Some("S"),
                json!([edit("docs/git.md", "", "v\n", "AC1")]),
                |f| {
                    add_check(f, "green", None);
                    approve_all(f);
                },
                Some("protected"),
            )
        });
    });
}

#[test]
fn rfix2_a_docs_task_fix_is_held_to_the_sizes_and_bounds() {
    sizes_and_bounds_on("docs");
}

#[test]
fn rfix2_a_review_task_fix_is_held_to_the_sizes_and_bounds() {
    sizes_and_bounds_on("review");
}

/// RFIX-2 AC6 (2) and (4): an applied docs or review fix is refused a
/// confirm until `thoth verify EDIT_RUN` ran on it in the fix worktree; the
/// edit review's own model and effort is refused as the confirmer; another
/// model's APPROVE passes the task on the fix branch.
fn confirm_waits_for_verify_on(task_type: &str) {
    let st = setup_typed(task_type);
    add_green_check(&st);
    let (edit_run, fix) = applied_fix(&st, &reject(&st, false, json!([]), small_edit()), &[]);
    let err = confirm_refused(&st);
    assert!(err.contains(&format!("thoth verify {edit_run}")), "{err}");
    assert_eq!(s(&ok(&st.f, &["status", &st.task, "--json"], &[])["status"]), "in_review");

    let out = exec(&st.f, &["verify", &edit_run, "--json"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let v = out.json();
    assert_eq!(v["head_commit"], show(&st)["fix"]["fix_commit"], "{v:#}");
    assert_eq!(v["head_commit"], fix.as_str());
    let fix_dir = format!("{}-a1-fix", st.task);
    for r in v["results"].as_array().unwrap() {
        assert!(s(&r["cwd"]).contains(&fix_dir), "verify runs in the fix worktree: {r:#}");
    }

    let before = run_count(&st.f, &st.task);
    let same = exec(
        &st.f,
        &["review", "confirm", &st.task, "--profile", "claude-b", "--model", "opus", "--effort", "medium", "--reason", REASON, "--json"],
        &confirm_env(&st.task),
    );
    assert_eq!(same.code, 1, "{}", same.stdout);
    assert!(same.stderr.contains("edit review's own"), "{}", same.stderr);
    assert_eq!(run_count(&st.f, &st.task), before);

    confirm_passes(&st);
    let shown = show(&st);
    assert_eq!(shown["fix"]["verify"]["ok"], true, "{shown}");
    assert_eq!(shown["merge_branch"], format!("thoth/{}/a1-fix", st.task), "{shown}");
    assert_eq!(s(&ok(&st.f, &["status", &st.task, "--json"], &[])["status"]), "pass");
}

#[test]
fn rfix2_a_docs_task_fix_needs_verify_and_another_confirmer() {
    confirm_waits_for_verify_on("docs");
}

#[test]
fn rfix2_a_review_task_fix_needs_verify_and_another_confirmer() {
    confirm_waits_for_verify_on("review");
}

/// RFIX-2 AC6 (3): a docs or review fix that adds a line to an existing test
/// is listed in the review packet's protected_changes and waits for
/// `checks approve --run EDIT_RUN` before the confirm.
fn protected_fix_waits_for_approval_on(task_type: &str) {
    let st = setup_typed(task_type);
    add_green_check(&st);
    let added = edit("tests/greet_check.rs", "fn helper() {}\n", "fn helper() {}\nfn more() {}\n", "AC1");
    let (edit_run, _) = applied_fix(&st, &reject(&st, false, json!([]), json!([added])), &[]);
    verify_green(&st, &edit_run);
    let packet = ok(&st.f, &["review", "packet", &st.task, "--json"], &[]);
    let listed = packet["protected_changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["run_id"] == edit_run.as_str() && c["path"] == "tests/greet_check.rs" && c["kind"] == "test");
    assert!(listed, "{:#}", packet["protected_changes"]);
    let err = confirm_refused(&st);
    assert!(err.contains("changed protected files that nobody has approved yet"), "{err}");
    assert!(err.contains(&format!("thoth checks approve --run {edit_run}")), "{err}");
    assert_eq!(s(&ok(&st.f, &["status", &st.task, "--json"], &[])["status"]), "in_review");
    let v = approve(&st, &edit_run).json();
    assert_eq!(v["protected_approved"], json!([entry("tests/greet_check.rs", "test")]), "{v:#}");
    confirm_passes(&st);
}

#[test]
fn rfix2_a_docs_task_fix_with_a_protected_change_needs_approval() {
    protected_fix_waits_for_approval_on("docs");
}

#[test]
fn rfix2_a_review_task_fix_with_a_protected_change_needs_approval() {
    protected_fix_waits_for_approval_on("review");
}

/// RFIX-2 AC6 (4): a completed verify whose check failed, with HEAD and the
/// worktree unchanged, is `fix.verify.ok` false; the confirm still runs and
/// its APPROVE rejects the task without the fix.
fn red_verify_rejects_on(task_type: &str) {
    let st = setup_typed(task_type);
    ok(&st.f, &["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"], &[]);
    ok(&st.f, &["checks", "add", "redcheck", "--cmd", "git no-such-subcommand-rfix2", "--shell", "argv", "--json"], &[]);
    approve_all(&st.f);
    let (edit_run, fix) = applied_fix(&st, &reject(&st, false, json!([]), small_edit()), &[]);
    let out = exec(&st.f, &["verify", &edit_run, "--json"], &[]);
    assert_eq!(out.code, 1, "{}\n{}", out.stdout, out.stderr);
    let v = out.json();
    assert_eq!(v["head_commit"], fix.as_str(), "{v:#}");
    assert_eq!(v["head_commit_after"], fix.as_str(), "{v:#}");
    assert_eq!(v["worktree_clean"], true, "{v:#}");
    assert_eq!(show(&st)["fix"]["verify"]["ok"], false);
    let c = ok(&st.f, &confirm_args(&st.task), &confirm_env(&st.task));
    assert_eq!(c["fix_verify"]["ok"], false, "{c}");
    let w = wait_run(&st.f, &s(&c["run_id"]));
    assert_eq!(w["timed_out"], false, "{w}");
    assert_eq!(w["task_status"], "reject", "{w}\n{}", st.f.explain_task(&st.task));
    assert_eq!(show(&st)["merge_branch"], Value::Null, "the fix is not adopted");
}

#[test]
fn rfix2_a_red_verify_rejects_a_docs_task_fix() {
    red_verify_rejects_on("docs");
}

#[test]
fn rfix2_a_red_verify_rejects_a_review_task_fix() {
    red_verify_rejects_on("review");
}

/// RFIX-2 AC7 (a): a docs or review fix that changes a Thoth spec, turns a
/// file into one or adds one is refused at spec_file, like a code task's.
fn spec_files_refused_on(task_type: &'static str) {
    let spec_line = format!("{SPEC}\n");
    let cases = [
        edit("plans/spec.json", "keep the greeting right", "keep it right", "AC1"),
        edit("config/data.json", "{\"name\": \"x\"}\n", &spec_line, "AC1"),
        edit("plans/new.json", "", &spec_line, "AC1"),
    ];
    std::thread::scope(|scope| {
        for e in cases {
            scope.spawn(move || fix_case(task_type, Some("S"), json!([e]), |_| {}, Some("spec_file")));
        }
    });
}

#[test]
fn rfix2_a_docs_task_fix_touching_a_thoth_spec_is_refused() {
    spec_files_refused_on("docs");
}

#[test]
fn rfix2_a_review_task_fix_touching_a_thoth_spec_is_refused() {
    spec_files_refused_on("review");
}

/// A docs task with `checks` (raw JSON, `None` for none) whose catalogue
/// holds `approved` checks and, unapproved, `pending` ones, all added
/// before the task; its applied fix: (setup, edit run, fix commit).
fn docs_fix_with_checks(checks: Option<&str>, approved: &'static [&'static str], pending: &'static [&'static str]) -> (Setup, String, String) {
    let st = setup_full(&typed_spec("docs", checks), "ok", &[], |f| {
        for id in approved {
            add_check(f, id, None);
        }
        if !approved.is_empty() {
            approve_all(f);
        }
        for id in pending {
            add_check(f, id, None);
        }
    });
    let (edit_run, fix) = applied_fix(&st, &reject(&st, false, json!([]), small_edit()), &[]);
    (st, edit_run, fix)
}

/// `verify EDIT_RUN --json`: (exit code, document).
fn verify_doc(st: &Setup, run: &str) -> (i32, Value) {
    let out = exec(&st.f, &["verify", run, "--json"], &[]);
    (out.code, out.json())
}

fn check_ids(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = v["results"].as_array().unwrap().iter().map(|r| s(&r["check_id"])).collect();
    ids.sort();
    ids
}

/// RFIX-2 AC7 (d): a non-empty `checks` list on a docs task selects only
/// those ids (source `approved checks (spec)`), whatever the task type.
#[test]
fn rfix2_verify_of_a_docs_fix_runs_only_the_listed_checks() {
    let (st, edit_run, _) = docs_fix_with_checks(Some("[\"green\"]"), &["green", "green2"], &[]);
    let (code, v) = verify_doc(&st, &edit_run);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["source"], "approved checks (spec)", "{v:#}");
    assert_eq!(check_ids(&v), ["green"], "{v:#}");
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    assert_eq!(show(&st)["fix"]["verify"]["ok"], true);
}

/// RFIX-2 AC7 (d): no `checks`, or an empty list, selects the whole
/// catalogue (source `approved checks (all)`), an unapproved check included
/// as a refused row (exit_code null, error naming the approval), so the
/// fix's verify is not ok.
#[test]
fn rfix2_verify_of_a_docs_fix_without_or_with_empty_checks_runs_the_whole_list() {
    std::thread::scope(|scope| {
        for checks in [None, Some("[]")] {
            scope.spawn(move || {
                let (st, edit_run, _) = docs_fix_with_checks(checks, &["green", "green2"], &["pending"]);
                let (code, v) = verify_doc(&st, &edit_run);
                assert_eq!(code, 1, "{checks:?}: {v:#}");
                assert_eq!(v["source"], "approved checks (all)", "{checks:?}: {v:#}");
                assert_eq!(check_ids(&v), ["green", "green2", "pending"], "{checks:?}: {v:#}");
                for r in v["results"].as_array().unwrap() {
                    if r["check_id"] == "pending" {
                        assert_eq!(r["exit_code"], Value::Null, "{r:#}");
                        assert!(s(&r["error"]).contains("is not approved"), "{r:#}");
                    } else {
                        assert_eq!(r["exit_code"], 0, "{r:#}");
                    }
                }
                assert_eq!(show(&st)["fix"]["verify"]["ok"], false);
            });
        }
    });
}

/// RFIX-2 AC7 (d): a listed check that is not approved is a refused row,
/// `fix.verify.ok` is false and the confirm does not adopt the fix.
#[test]
fn rfix2_a_listed_unapproved_check_keeps_a_docs_fix_from_being_adopted() {
    let (st, edit_run, _) = docs_fix_with_checks(Some("[\"green\",\"pending\"]"), &["green"], &["pending"]);
    let (code, v) = verify_doc(&st, &edit_run);
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(v["source"], "approved checks (spec)", "{v:#}");
    assert_eq!(check_ids(&v), ["green", "pending"], "{v:#}");
    let pending = v["results"].as_array().unwrap().iter().find(|r| r["check_id"] == "pending").unwrap();
    assert_eq!(pending["exit_code"], Value::Null, "{pending:#}");
    assert!(s(&pending["error"]).contains("is not approved"), "{pending:#}");
    assert_eq!(show(&st)["fix"]["verify"]["ok"], false);
    let c = ok(&st.f, &confirm_args(&st.task), &confirm_env(&st.task));
    assert_eq!(c["fix_verify"]["ok"], false, "{c}");
    let w = wait_run(&st.f, &s(&c["run_id"]));
    assert_eq!(w["task_status"], "reject", "{w}\n{}", st.f.explain_task(&st.task));
    assert_eq!(show(&st)["merge_branch"], Value::Null, "the fix is not adopted");
}

/// RFIX-2 AC7 (d): when every listed check is gone from the catalogue, the
/// first verify of the fix is `no_approved_checks`, records no verify_ran,
/// leaves `fix.verify` null, and the confirm is refused as unverified.
#[test]
fn rfix2_verify_of_a_docs_fix_whose_listed_checks_are_all_gone_records_nothing() {
    let (st, edit_run, _) = docs_fix_with_checks(Some("[\"gone\"]"), &["gone"], &[]);
    ok(&st.f, &["checks", "remove", "gone", "--json"], &[]);
    let verifies = events(&st.f, &st.task, "verify_ran").len();
    let (code, v) = verify_doc(&st, &edit_run);
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(v["ok"], false, "{v:#}");
    assert_eq!(v["reason"], "no_approved_checks", "{v:#}");
    assert_eq!(events(&st.f, &st.task, "verify_ran").len(), verifies, "no verify_ran");
    assert_eq!(show(&st)["fix"]["verify"], Value::Null);
    let err = confirm_refused(&st);
    assert!(err.contains(&format!("thoth verify {edit_run}")), "{err}");
}

// --- GATE-1 (2): an unreadable diff recorded again is not a failed recording ---------

/// The refusal for a diff that is still unreadable after it was recorded
/// again: it names the run and `has no readable protected_changes`, does not
/// say the recording failed, and the row was recorded one more time.
fn assert_unreadable_again(out: &Out, run: &str, rows_before: usize, st: &Setup) {
    assert_eq!(out.code, 1, "stdout {}\nstderr {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("has no readable protected_changes"), "{}", out.stderr);
    assert!(out.stderr.contains(run), "{}", out.stderr);
    assert!(!out.stderr.contains("recording it again failed"), "{}", out.stderr);
    assert_eq!(diff_rows(&st.f, &st.task, run).len(), rows_before + 1, "the diff was recorded again");
}

#[test]
fn an_unreadable_diff_recorded_again_is_not_called_a_failed_recording() {
    // (a) review start on a pre-LEAN candidate diff that is recorded pre-LEAN again
    let (st, _code) = pre_lean_implementer();
    let before = diff_rows(&st.f, &st.task, &st.implementer).len();
    let env = [("FAKE_SCENARIO", "review_pass"), ("FAKE_TASK_ID", st.task.as_str()), PRE_LEAN];
    let out = exec(&st.f, &review_args(&st.task, false), &env);
    assert_unreadable_again(&out, &st.implementer, before, &st);

    // (b) review confirm on a pre-LEAN fix diff that is recorded pre-LEAN again
    let st = setup();
    add_green_check(&st);
    let fix_edit = edit("src/guarded.rs", "plain line\n", "plain line, fixed\n", "AC1");
    let (edit_run, _fix) = applied_fix(&st, &reject(&st, false, json!([]), json!([fix_edit])), &[PRE_LEAN]);
    verify_green(&st, &edit_run);
    let before = diff_rows(&st.f, &st.task, &edit_run).len();
    let env = [("FAKE_SCENARIO", "fix_confirm_approve"), ("FAKE_TASK_ID", st.task.as_str()), PRE_LEAN];
    let out = exec(&st.f, &confirm_args(&st.task), &env);
    assert_unreadable_again(&out, &edit_run, before, &st);
}

// --- REV-1: the verdict events of an edit review and a confirm -----------------------

/// The `verdict` events of `task` of `mode`, oldest first, as `detail`.
fn verdicts_of_mode(f: &Fixture, task: &str, mode: &str) -> Vec<Value> {
    events(f, task, "verdict").into_iter().map(|e| e["detail"].clone()).filter(|d| d["mode"] == mode).collect()
}

/// REV-1 (AC5): the `verdict` events of edit reviews and a confirm say
/// whether their findings held a blocking one (read from `thoth events`
/// only). An edit review whose fix is applied records no verdict while the
/// fix is pending (docs/adapter-usage.md, `review_fix_applied`), so the edit
/// reviews that write one are a REJECT whose fix is refused (a) and a REJECT
/// that attaches no edit (b); the applied fix's verdict comes with the
/// confirm (c), whose REJECT record carries no category.
#[test]
fn edit_review_and_confirm_verdict_events_record_has_blocking_findings() {
    // (a) AC1 unmet (spec_mismatch) with an edit bound to it, and a blocking
    // extra finding no edit covers: the fix is refused at `size`
    let st = setup();
    let document = reject(
        &st,
        false,
        json!([labelled(extra("major", "unbound", "S"), "error_path")]),
        json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")]),
    );
    let (_, w) = edit_review(&st, &document, &[]);
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    let edits = verdicts_of_mode(&st.f, &st.task, "edit");
    assert_eq!(edits.len(), 1, "{edits:#?}");
    assert_eq!(edits[0]["verdict"], "REJECT", "{edits:#?}");
    assert_eq!(edits[0]["fix"]["applied"], false, "{edits:#?}");
    assert_eq!(edits[0]["has_blocking_findings"], true, "{edits:#?}");

    // (a2) SMALL-16: only AC1 unmet, labelled spec_mismatch, no edit and no
    // extra finding: an unmet blocking criterion alone is blocking
    let alone = setup();
    let (_, w) = edit_review(&alone, &reject(&alone, false, json!([]), json!([])), &[]);
    assert_eq!(w["verdict"], "REJECT", "{}", alone.f.explain_task(&alone.task));
    let edits = verdicts_of_mode(&alone.f, &alone.task, "edit");
    assert_eq!(edits.len(), 1, "{edits:#?}");
    assert_eq!(edits[0]["verdict"], "REJECT", "{edits:#?}");
    assert_eq!(edits[0]["has_blocking_findings"], true, "{edits:#?}");

    // (b) every finding advisory (AC1 unmet labelled docs, a docs extra
    // finding), no edit attached
    let other = setup();
    let mut advisory = advisory_doc(&other, Some("docs"), None);
    advisory["findings"]["criteria"][0]["category"] = json!("docs");
    advisory["edits"] = json!([]);
    let (_, w) = edit_review(&other, &advisory, &[]);
    assert_eq!(w["verdict"], "REJECT", "{}", other.f.explain_task(&other.task));
    let edits = verdicts_of_mode(&other.f, &other.task, "edit");
    assert_eq!(edits.len(), 1, "{edits:#?}");
    assert_eq!(edits[0]["verdict"], "REJECT", "{edits:#?}");
    assert_eq!(edits[0]["fix"], Value::Null, "{edits:#?}");
    assert_eq!(edits[0]["has_blocking_findings"], false, "{edits:#?}");

    // (c) the applied fix: no verdict yet; verified, a confirm that rejects
    // writes one with no category on its findings, so it blocks
    let third = setup();
    add_green_check(&third);
    let (edit_run, _) = applied_fix(&third, &reject(&third, false, json!([]), json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")])), &[]);
    assert_eq!(events(&third.f, &third.task, "verdict").len(), 0, "an applied fix records no verdict while it is pending");
    verify_green(&third, &edit_run);
    let c = ok(&third.f, &confirm_args(&third.task), &[("FAKE_SCENARIO", "fix_confirm_revise"), ("FAKE_TASK_ID", third.task.as_str())]);
    assert_eq!(c["mode"], "confirm", "{c}");
    let w = wait_run(&third.f, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "REJECT", "{}", third.f.explain_task(&third.task));
    let confirms = verdicts_of_mode(&third.f, &third.task, "confirm");
    assert_eq!(confirms.len(), 1, "{confirms:#?}");
    assert_eq!(confirms[0]["verdict"], "REJECT", "{confirms:#?}");
    assert_eq!(confirms[0]["has_blocking_findings"], true, "{confirms:#?}");
    assert_eq!(show(&third)["has_blocking_findings"], true, "{}", show(&third));
}

// --- BRIEF-1: an edit review and a confirm lead with their own brief -----------------

#[test]
fn edit_review_and_confirm_runs_lead_with_their_own_brief() {
    let st = setup();
    add_green_check(&st);
    let (edit_run, _) = applied_fix(&st, &reject(&st, false, json!([]), json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")])), &[]);
    let w = wait_run(&st.f, &edit_run);
    assert_eq!(w["brief_source"], "review", "{w}");
    let brief = s(&w["brief"]);
    assert!(brief.starts_with("REJECT: 1/2 criteria met; AC1 unmet"), "{brief}");

    verify_green(&st, &edit_run);
    let c = ok(&st.f, &confirm_args(&st.task), &confirm_env(&st.task));
    let w = wait_run(&st.f, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["brief_source"], "review", "{w}");
    let brief = s(&w["brief"]);
    assert!(brief.starts_with("PASS: "), "{brief}");
    assert_eq!(brief, s(&show(&st)["brief"]), "the confirm run's brief is the one review show gives");
}
