//! VSCOPE-1: a check registered with `--scope cargo-test-targets` runs only
//! the task's test targets. Everything goes through the CLI in a temporary
//! THOTH_HOME: fake-claude is the worker (its `hooks` scenario plus an edit
//! plan), fake-check's `args` mode is the scoped check, so the argv verify
//! ran is visible in `verify --json` and in `evidence list --json`.

mod common;

use common::Fixture;
use serde_json::{json, Value};

const REASON: &str = "VSCOPE-1 fixture: fake workers in a temporary repo";
const SKIP_SPEC_REVIEW: &str = "VSCOPE-1 fixture: the spec review gate is covered elsewhere";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");
const SCOPED: &str = "test-scope";
const PLAIN: &str = "plain";

/// The two files the fake worker's `hooks` scenario always writes, with the
/// same bytes, so committing them to the base keeps them out of the diff.
const FAKE_SRC: &str = "pub fn added() -> u8 { 7 }\n";
const FAKE_TEST: &str =
    "#[test]\nfn added_is_seven() { assert_eq!(super_seven(), 7); }\nfn super_seven() -> u8 { 7 }\n";

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

/// `checks list --json`'s row for `id`.
fn listed(f: &Fixture, id: &str) -> Value {
    let l = ok(f, &["checks", "list", "--json"], &[]);
    l["checks"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["check_id"] == id))
        .cloned()
        .unwrap_or_else(|| panic!("no {id} in checks list: {l}"))
}

/// `checks add ID --cmd "<fake-check> MODE"`, scoped or not.
fn add_check(f: &Fixture, id: &str, mode: &str, scoped: bool) -> Value {
    let cmd = format!("\"{FAKE_CHECK}\" {mode}");
    let mut args = vec!["checks", "add", id, "--cmd", &cmd, "--shell", "argv"];
    if scoped {
        args.extend(["--scope", "cargo-test-targets"]);
    }
    args.push("--json");
    ok(f, &args, &[])
}

fn approve(f: &Fixture) -> Value {
    ok(f, &["checks", "approve", "--reason", REASON, "--json"], &[])
}

/// A repo whose base holds the fake worker's two files and `base_files`,
/// with profiles registered.
fn repo_with(base_files: &[&str]) -> Fixture {
    let f = common::fixture();
    let repo = f.repo_path();
    let write = |path: &str, body: &str| {
        let p = repo.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write("src/added.rs", FAKE_SRC);
    write("tests/added_test.rs", FAKE_TEST);
    for p in base_files {
        write(p, "#[test]\nfn base() {}\n");
    }
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-m", "base files"]);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f
}

fn spec(checks: &[&str], text: &str) -> String {
    json!({"goal": "say hello twice", "acceptance_criteria": ["AC1: prints hello", "AC2: exits zero"],
        "constraints": [text], "non_goals": [], "excluded_alternatives": [], "risk": "normal",
        "checks": checks})
    .to_string()
}

/// A task with `spec_json`, whose first implementer run (the `hooks`
/// scenario with `plan` applied) is `in_review`: (task, run).
fn task_in_review(f: &Fixture, spec_json: &str, plan: &Value) -> (String, String) {
    let (task, _) = f.create_task(spec_json);
    let plan_path = f.home_path().join(format!("plan-{task}.json"));
    std::fs::write(&plan_path, plan.to_string()).unwrap();
    let plan_path = plan_path.display().to_string();
    let d = ok(
        f,
        &[
            "dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON,
            "--skip-spec-review", SKIP_SPEC_REVIEW, "--json",
        ],
        &[("FAKE_SCENARIO", "hooks"), ("FAKE_TASK_ID", &task), ("FAKE_EDIT_PLAN", &plan_path)],
    );
    let run = d["run_id"].as_str().unwrap_or_else(|| panic!("dispatch gave no run_id: {d}")).to_string();
    let w = ok(f, &["wait", "--run", &run, "--timeout", "180s", "--json"], &[]);
    assert_eq!(w["task_status"], "in_review", "{w}\n{}", f.explain_task(&task));
    (task, run)
}

/// `verify RUN [extra] --json` (exit 0 or 1); its JSON document.
fn verify(f: &Fixture, run: &str, extra: &[&str]) -> Value {
    let mut args = vec!["verify", run];
    args.extend_from_slice(extra);
    args.push("--json");
    let out = exec(f, &args, &[]);
    assert!(out.code == 0 || out.code == 1, "verify exited {}: {}\n{}", out.code, out.stdout, out.stderr);
    serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("verify printed no JSON ({e}): {}\n{}", out.stdout, out.stderr))
}

fn row<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["results"]
        .as_array()
        .and_then(|r| r.iter().find(|x| x["check_id"] == id))
        .unwrap_or_else(|| panic!("no {id} row in {v}"))
}

/// The argv of the newest control-plane evidence row for check `id`, from
/// `evidence list TASK --json`.
fn evidence_argv(f: &Fixture, task: &str, id: &str) -> Vec<String> {
    let rows = f.evidence_rows(task);
    let r = rows
        .iter()
        .rfind(|e| e["collected_by"] == "control_plane" && e["detail"]["check_id"] == id)
        .unwrap_or_else(|| panic!("no evidence row for {id}: {rows:?}"));
    r["detail"]["argv"]
        .as_array()
        .unwrap_or_else(|| panic!("no argv in {r}"))
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect()
}

/// The arguments after the program and its mode word `args`.
fn appended(argv: &[String]) -> Vec<String> {
    let at = argv.iter().position(|a| a == "args").unwrap_or_else(|| panic!("no mode word in {argv:?}"));
    argv[at + 1..].to_vec()
}

fn names(scope: &Value) -> Vec<String> {
    scope["targets"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

fn target<'a>(scope: &'a Value, name: &str) -> &'a Value {
    scope["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no target {name} in {scope}"))
}

/// AC1: `--scope cargo-test-targets` is stored and listed, pending until
/// approved; any other value is refused with the catalogue unchanged; an
/// unscoped check lists `scope: null`.
#[test]
fn checks_add_records_the_scope_and_refuses_an_unknown_one() {
    let f = repo_with(&[]);
    let added = add_check(&f, SCOPED, "args", true);
    assert_eq!(added["scope"], "cargo-test-targets", "{added}");
    let row = listed(&f, SCOPED);
    assert_eq!(row["scope"], "cargo-test-targets", "{row}");
    assert_eq!(row["status"], "pending", "{row}");

    add_check(&f, PLAIN, "args", false);
    assert_eq!(listed(&f, PLAIN)["scope"], Value::Null);

    let a = approve(&f);
    let approved: Vec<&str> = a["approved"].as_array().unwrap().iter().map(|x| x["check_id"].as_str().unwrap()).collect();
    assert!(approved.contains(&SCOPED) && approved.contains(&PLAIN), "{a}");
    assert_eq!(listed(&f, SCOPED)["status"], "approved");
    assert_eq!(listed(&f, PLAIN)["status"], "approved");

    let before = ok(&f, &["checks", "list", "--json"], &[]);
    let cmd = format!("\"{FAKE_CHECK}\" args");
    for bad in ["all", "cargo-test", ""] {
        let out = exec(&f, &["checks", "add", "other", "--cmd", &cmd, "--scope", bad, "--json"], &[]);
        assert_ne!(out.code, 0, "--scope {bad:?} was taken: {}", out.stdout);
        assert!(out.stderr.contains("--scope"), "{}", out.stderr);
    }
    // replacing an existing check with a bad scope leaves it as it was too
    let out = exec(&f, &["checks", "add", SCOPED, "--cmd", &cmd, "--scope", "bogus"], &[]);
    assert_ne!(out.code, 0);
    assert_eq!(ok(&f, &["checks", "list", "--json"], &[]), before);
}

/// AC1: the scope is part of the approval - the same command with and
/// without a scope pins different fingerprints, so switching it needs a new
/// approval.
#[test]
fn the_scope_is_part_of_the_fingerprint() {
    let f = repo_with(&[]);
    add_check(&f, SCOPED, "args", true);
    add_check(&f, PLAIN, "args", false);
    approve(&f);
    let scoped = listed(&f, SCOPED)["approved_fingerprint"].clone();
    let plain = listed(&f, PLAIN)["approved_fingerprint"].clone();
    assert!(scoped.is_string() && plain.is_string());
    assert_ne!(scoped, plain, "same command, different scope: the fingerprints must differ");
}

/// AC3 (1), (5), (6) and AC4: a spec-named test, an added one and a
/// modified one become three targets with their sources; the argv ends in
/// three `--test`; an unscoped check's row has no `scope` and its argv is
/// the approved command; `--dry-run` rows carry `scope` too. The review
/// packet's `test_scope` and `## Test scope` show the same.
#[test]
fn verify_appends_one_test_flag_per_target_and_the_packet_shows_them() {
    let f = repo_with(&["tests/spec_named.rs", "tests/to_modify.rs"]);
    add_check(&f, SCOPED, "args", true);
    add_check(&f, PLAIN, "args", false);
    approve(&f);
    let plan = json!({"write": [
        {"path": "tests/new_one.rs", "text": "#[test]\nfn n() {}\n"},
        {"path": "tests/to_modify.rs", "text": "#[test]\nfn changed() {}\n"},
    ]});
    let (task, run) = task_in_review(&f, &spec(&[SCOPED, PLAIN], "the tests live in tests/spec_named.rs"), &plan);

    // (6) a dry run lists the scope and runs nothing
    let dry = verify(&f, &run, &["--dry-run"]);
    let d = row(&dry, SCOPED);
    assert_eq!(d["dry_run"], true, "{dry}");
    assert_eq!(names(&d["scope"]), ["new_one", "spec_named", "to_modify"], "{dry}");
    assert!(row(&dry, PLAIN).get("scope").is_none(), "{dry}");

    let v = verify(&f, &run, &[]);
    assert_eq!(v["ok"], true, "{v}");
    let scope = &row(&v, SCOPED)["scope"];
    assert_eq!(scope["mode"], "targets", "{scope}");
    assert_eq!(names(scope), ["new_one", "spec_named", "to_modify"], "{scope}");
    assert_eq!(target(scope, "new_one")["sources"], json!(["diff_added"]));
    assert_eq!(target(scope, "new_one")["path"], "tests/new_one.rs");
    assert_eq!(target(scope, "spec_named")["sources"], json!(["spec"]));
    assert_eq!(target(scope, "to_modify")["sources"], json!(["diff_modified"]));
    assert_eq!(scope["all_tests_because"], json!([]));
    assert_eq!(scope["dropped"], json!([]));
    let expected = ["--test", "new_one", "--test", "spec_named", "--test", "to_modify"];
    assert_eq!(scope["appended_args"], json!(expected));
    assert_eq!(appended(&evidence_argv(&f, &task, SCOPED)), expected);

    // (5) the unscoped check: no scope key, the approved argv
    assert!(row(&v, PLAIN).get("scope").is_none(), "{v}");
    assert_eq!(appended(&evidence_argv(&f, &task, PLAIN)), Vec::<String>::new());

    // `--at-base`: the same list, less the file the base does not have
    let base = verify(&f, &run, &["--at-base"]);
    let at = &row(&base, SCOPED)["scope"];
    assert_eq!(names(at), ["spec_named", "to_modify"], "{base}");
    assert_eq!(at["dropped"][0]["name"], "new_one", "{base}");
    assert_eq!(at["dropped"][0]["reason"], "missing_at_base", "{base}");
    assert_eq!(at["appended_args"], json!(["--test", "spec_named", "--test", "to_modify"]));

    // AC4: the packet
    let p = ok(&f, &["review", "packet", &task, "--json"], &[]);
    let ts = p["test_scope"].as_array().unwrap_or_else(|| panic!("no test_scope in {p}"));
    assert_eq!(ts.len(), 1, "{p}");
    assert_eq!(ts[0]["check_id"], SCOPED);
    assert_eq!(ts[0]["mode"], "targets");
    assert_eq!(ts[0]["targets"], scope["targets"]);
    assert_eq!(ts[0]["appended_args"], json!(expected));
    let md = exec(&f, &["review", "packet", &task], &[]).stdout;
    let start = md.find("## Test scope\n").unwrap_or_else(|| panic!("no section in:\n{md}"));
    let not_run = md.find("## Required checks not run\n").unwrap();
    let diffs = md.find("## Diffs\n").unwrap();
    assert!(not_run < start && start < diffs, "section order:\n{md}");
    let section = &md[start..diffs];
    assert!(section.contains("only in the whole suite"), "{section}");
    assert!(
        section.contains(&format!(
            "{SCOPED}: targets new_one (diff_added), spec_named (spec), to_modify (diff_modified)"
        )),
        "{section}"
    );
}

/// AC3 (2): a change to a shared test helper runs every test target.
#[test]
fn a_changed_shared_test_helper_runs_all_tests() {
    let f = repo_with(&["tests/common/mod.rs"]);
    add_check(&f, SCOPED, "args", true);
    approve(&f);
    let plan = json!({"write": [{"path": "tests/common/mod.rs", "text": "pub fn helper() {}\n"}]});
    let (task, run) = task_in_review(&f, &spec(&[SCOPED], "tests/named.rs"), &plan);
    let v = verify(&f, &run, &[]);
    let scope = &row(&v, SCOPED)["scope"];
    assert_eq!(scope["mode"], "all_tests", "{scope}");
    assert_eq!(scope["all_tests_because"], json!(["tests/common/mod.rs"]));
    assert_eq!(scope["targets"], json!([]));
    assert_eq!(appended(&evidence_argv(&f, &task, SCOPED)), ["--tests"]);
    let md = exec(&f, &["review", "packet", &task], &[]).stdout;
    assert!(md.contains(&format!("{SCOPED}: all tests (tests/common/mod.rs)")), "{md}");
}

/// AC3 (3) and AC7: only src changed and the spec names no test: no
/// target, the approved argv unchanged. With only the scoped check in the
/// spec's `checks`, nothing is missing and a PASS is taken.
#[test]
fn a_src_only_change_appends_nothing_and_a_pass_is_taken() {
    let f = repo_with(&[]);
    add_check(&f, SCOPED, "args", true);
    approve(&f);
    let plan = json!({"write": [{"path": "src/other.rs", "text": "pub fn other() {}\n"}]});
    let (task, run) = task_in_review(&f, &spec(&[SCOPED], "change the parser"), &plan);
    let v = verify(&f, &run, &[]);
    assert_eq!(v["ok"], true, "{v}");
    let r = row(&v, SCOPED);
    assert_eq!(r["exit_code"], 0, "{v}");
    assert_eq!(r["scope"]["mode"], "targets");
    assert_eq!(r["scope"]["targets"], json!([]));
    assert_eq!(r["scope"]["appended_args"], json!([]));
    assert_eq!(appended(&evidence_argv(&f, &task, SCOPED)), Vec::<String>::new());

    let p = ok(&f, &["review", "packet", &task, "--json"], &[]);
    assert_eq!(p["checks_not_run"], json!([]), "{p}");
    let met = |id: &str| json!({"id": id, "verdict": "met", "evidence": ["E001"]});
    let doc = json!({"verdict": "PASS", "findings": {"criteria": [met("AC1"), met("AC2")], "extra_findings": []}});
    let file = f.home_path().join("review-pass.json");
    std::fs::write(&file, doc.to_string()).unwrap();
    let out = exec(&f, &["review", "record", &task, "-f", &file.display().to_string(), "--json"], &[]);
    assert_eq!(out.code, 0, "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    let taken: Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(taken["task_status"], "pass", "{taken}");
}

/// AC3 (4) and AC7: a spec-named test the diff deletes is dropped and not in
/// the argv. The full `test` check, listed in the spec but not approved, is
/// the only check REV-3 reports as not run - the scoped check's row never
/// counts as `test`.
#[test]
fn a_deleted_spec_named_test_is_dropped_and_the_full_test_is_still_required() {
    let f = repo_with(&["tests/doomed.rs", "tests/kept.rs"]);
    add_check(&f, SCOPED, "args", true);
    approve(&f);
    // added after the approval, so it stays pending
    add_check(&f, "test", "exit 0", false);
    let plan = json!({"delete": ["tests/doomed.rs"], "write": [{"path": "src/other.rs", "text": "pub fn o() {}\n"}]});
    let (task, run) = task_in_review(&f, &spec(&[SCOPED, "test"], "see tests/doomed.rs and tests/kept.rs"), &plan);
    let v = verify(&f, &run, &[]);
    let scope = &row(&v, SCOPED)["scope"];
    assert_eq!(names(scope), ["kept"], "{scope}");
    let dropped = scope["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 1, "{scope}");
    assert_eq!(dropped[0]["name"], "doomed");
    assert_eq!(dropped[0]["path"], "tests/doomed.rs");
    assert_eq!(dropped[0]["reason"], "deleted_in_diff");
    let argv = appended(&evidence_argv(&f, &task, SCOPED));
    assert_eq!(argv, ["--test", "kept"]);
    assert!(!argv.iter().any(|a| a == "doomed"));
    // the refused `test` row carries no scope
    assert!(row(&v, "test").get("scope").is_none(), "{v}");
    assert!(row(&v, "test")["error"].is_string(), "{v}");

    let p = ok(&f, &["review", "packet", &task, "--json"], &[]);
    assert_eq!(p["checks_not_run"], json!([{"check_id": "test", "reason": "did_not_complete"}]), "{p}");
    let md = exec(&f, &["review", "packet", &task], &[]).stdout;
    assert!(md.contains(&format!("{SCOPED}: targets kept (spec); dropped doomed (deleted_in_diff)")), "{md}");
}

/// AC4: no verify yet: `test_scope` is `[]` and the section says `(none)`.
#[test]
fn the_packet_has_an_empty_test_scope_before_any_verify() {
    let f = repo_with(&[]);
    add_check(&f, SCOPED, "args", true);
    approve(&f);
    let plan = json!({"write": [{"path": "src/other.rs", "text": "pub fn other() {}\n"}]});
    let (task, _run) = task_in_review(&f, &spec(&[SCOPED], "x"), &plan);
    let p = ok(&f, &["review", "packet", &task, "--json"], &[]);
    assert_eq!(p["test_scope"], json!([]), "{p}");
    let md = exec(&f, &["review", "packet", &task], &[]).stdout;
    let start = md.find("## Test scope\n").unwrap();
    let end = md.find("## Diffs\n").unwrap();
    assert!(md[start..end].contains("\n(none)\n"), "{md}");
}
