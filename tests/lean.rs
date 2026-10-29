//! LEAN-1: Thoth works a delivery's protected changes out from its diff (no
//! per-file declaration, no rule 6 refusal), finds changed inline Rust test
//! regions (`inline_test`), keeps the commander's approval and the review
//! gate, slims the worker prompt, and refuses low/normal specs over 20,480
//! bytes. Everything is observed through the CLI: `--json` output, events,
//! evidence and the files a run leaves (prompt.txt). Fake workers only, in a
//! temporary THOTH_HOME and repo.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const REASON: &str = "lean fixture: fake worker in a temporary repo";
const SPEC_NORMAL: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;

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

fn write_home_file(f: &Fixture, name: &str, body: &[u8]) -> String {
    let p = f.home_path().join(name);
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

// --- fixtures ---------------------------------------------------------------------

/// An initialised repo whose line ends git keeps as written, with two Claude
/// profiles.
fn initialised() -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    let mut c = cmd(&f);
    c.arg("init");
    f.run_ok(c, "init");
    for id in ["claude-b", "claude-a"] {
        add_profile(&f, id, "anthropic", "claude-code");
    }
    f
}

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) -> PathBuf {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let mut c = cmd(f);
    c.args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"])
        .arg(&cfg);
    f.run_ok(c, "profile add");
    cfg
}

/// Writes `files` into the main repo and commits them.
fn commit_files(f: &Fixture, files: &[(&str, &[u8])], msg: &str) {
    let repo = f.repo_path();
    for (path, body) in files {
        let p = repo.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        common::git(&repo, &["add", "--", path]);
    }
    common::git(&repo, &["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", msg]);
}

/// `task create` of `spec`: (task, base commit).
fn create(f: &Fixture, spec: &str) -> (String, String) {
    let file = write_home_file(f, "spec.json", spec.as_bytes());
    let v = ok(f, &["task", "create", "--title", "lean", "-f", &file, "--json"], &[]);
    (s(&v["task_id"]), s(&v["base_commit"]))
}

/// The plan file `FAKE_EDIT_PLAN` names: `write` (`text` or `hex`),
/// `delete`, `gitlink`.
fn plan_file(f: &Fixture, name: &str, plan: &Value) -> String {
    write_home_file(f, name, plan.to_string().as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `dispatch` of the fake Claude worker's `scenario`, with the plan, if any.
fn dispatch(f: &Fixture, task: &str, scenario: &str, plan: Option<&str>) -> Value {
    let mut envs = vec![("FAKE_SCENARIO", scenario), ("FAKE_TASK_ID", task)];
    if let Some(p) = plan {
        envs.push(("FAKE_EDIT_PLAN", p));
    }
    ok(
        f,
        &["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &envs,
    )
}

/// `wait TASK --json`'s status.
fn wait(f: &Fixture, task: &str) -> String {
    s(&ok(f, &["wait", task, "--timeout", "180s", "--json"], &[])["status"])
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[])["events"].as_array().unwrap().clone()
}

fn all_events(f: &Fixture) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--json"], &[])["events"].as_array().unwrap().clone()
}

fn run_events(f: &Fixture, task: &str, run: &str, ty: &str) -> Vec<Value> {
    events(f, task).into_iter().filter(|e| e["run_id"] == run && e["type"] == ty).collect()
}

/// The `detail` of a run's newest diff evidence, from `evidence list`.
fn diff_detail(f: &Fixture, task: &str, run: &str) -> Value {
    ok(f, &["evidence", "list", task, "--json"], &[])["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run)
        .unwrap_or_else(|| panic!("run {run} has no diff evidence\n{}", f.explain_task(task)))["detail"]
        .clone()
}

fn entries(list: &[(&str, &str, &str)]) -> Value {
    let mut sorted = list.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    Value::Array(sorted.iter().map(|(p, st, k)| json!({"path": p, "status": st, "kind": k})).collect())
}

fn runs(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["status", task, "--json"], &[])["runs"].as_array().unwrap().clone()
}

/// prompt.txt of a run, found through `status TASK --json`'s `runs[].run_dir`.
fn prompt_of(f: &Fixture, task: &str, run: &str) -> String {
    let row = runs(f, task)
        .into_iter()
        .find(|r| r["run_id"] == run)
        .unwrap_or_else(|| panic!("status {task} lists no run {run}"));
    std::fs::read_to_string(Path::new(&s(&row["run_dir"])).join("prompt.txt")).unwrap()
}

fn first_command_code(f: &Fixture, task: &str) -> String {
    s(&ok(f, &["evidence", "list", task, "--json"], &[])["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "command")
        .expect("a command evidence row")["code"])
}

// --- AC1 and AC3: the list without declarations, and the commander's approval ---

const KEEP_TEST: &str = "#[test]\nfn keep() { assert!(true); }\n";
const GONE_TEST: &str = "#[test]\nfn gone() { assert!(true); }\n";
const INLINE_BASE: &str = "pub fn one() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn one_is_one() {\n        assert_eq!(super::one(), 1);\n    }\n}\n";

/// A normal-risk task whose fake worker modifies an existing test, deletes
/// another, modifies a check input, adds a check-word file and changes an
/// inline test region, and declares none of it. Returns (task, run).
fn undeclared_candidate(f: &Fixture) -> (String, String) {
    commit_files(
        f,
        &[
            ("tests/keep_test.rs", KEEP_TEST.as_bytes()),
            ("tests/gone_test.rs", GONE_TEST.as_bytes()),
            ("src/inline.rs", INLINE_BASE.as_bytes()),
        ],
        "base for lean",
    );
    ok(f, &["checks", "add", "data", "--cmd", "cargo test", "--input", "b.txt", "--json"], &[]);
    let (task, _) = create(f, SPEC_NORMAL);
    let plan = plan_file(
        f,
        "plan-ac1.json",
        &json!({
            "write": [
                {"path": "tests/keep_test.rs", "text": "#[test]\nfn keep() { assert_eq!(1, 1); }\n"},
                {"path": "b.txt", "text": "a changed check input\n"},
                {"path": "cargo.bat", "text": "@echo off\n"},
                {"path": "src/inline.rs", "text": INLINE_BASE.replace("assert_eq!(super::one(), 1)", "assert!(super::one() > 0)")},
            ],
            "delete": ["tests/gone_test.rs"],
        }),
    );
    let d = dispatch(f, &task, "hooks", Some(&plan));
    (task, s(&d["run_id"]))
}

fn ac1_expected() -> Value {
    entries(&[
        ("b.txt", "modified", "check_input"),
        ("cargo.bat", "added", "check_word"),
        ("src/inline.rs", "modified", "inline_test"),
        ("tests/gone_test.rs", "deleted", "test"),
        ("tests/keep_test.rs", "modified", "test"),
    ])
}

/// AC1 (dispatch), AC3: in_review with no declaration; the list is complete
/// and sorted; the gate and the approval work on it, inline_test included.
#[test]
fn an_undeclared_delivery_is_in_review_and_waits_for_the_commanders_approval() {
    let f = initialised();
    let (task, run) = undeclared_candidate(&f);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));

    // AC1: the diff evidence lists every attributed path, sorted, kinds kept
    let detail = diff_detail(&f, &task, &run);
    assert_eq!(detail["protected_changes"], ac1_expected(), "{detail:#}");
    assert_eq!(detail["inline_test_errors"], json!([]));
    assert_eq!(run_events(&f, &task, &run, "result_ingested").len(), 1);
    assert!(run_events(&f, &task, &run, "result_invalid").is_empty());
    assert!(
        !events(&f, &task).iter().any(|e| e["type"] == "result_invalid" && e["detail"]["rule"] == 6),
        "no rule 6"
    );

    // AC1: no claims were made up for the worker
    let result = ok(&f, &["task", "result", &task, "--run", &run, "--json"], &[]);
    let kinds: Vec<String> = result["claims"].as_array().unwrap().iter().map(|c| s(&c["kind"])).collect();
    assert_eq!(kinds, ["test_result", "completion"], "{result:#}");

    // AC3: the packet, both formats, without any claim
    let packet = ok(&f, &["review", "packet", &task, "--json"], &[]);
    let listed: Vec<Value> = packet["protected_changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["run_id"] == run.as_str())
        .map(|c| json!({"path": c["path"], "status": c["status"], "kind": c["kind"]}))
        .collect();
    assert_eq!(Value::Array(listed), ac1_expected());
    let md = String::from_utf8(
        cmd(&f).args(["review", "packet", &task]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let section = &md[md.find("## Protected files changed").unwrap()..md.find("## Diffs").unwrap()];
    for line in [
        "b.txt (modified, check_input)",
        "cargo.bat (added, check_word)",
        "src/inline.rs (modified, inline_test)",
        "tests/gone_test.rs (deleted, test)",
        "tests/keep_test.rs (modified, test)",
    ] {
        assert!(section.contains(line), "{line}\n{section}");
    }

    // AC3: review start and a PASS record wait for the commander
    let review_args = [
        "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium",
        "--reason", REASON, "--json",
    ];
    let review_env = [("FAKE_SCENARIO", "review_pass"), ("FAKE_TASK_ID", task.as_str())];
    let refused = exec(&f, &review_args, &review_env);
    assert_eq!(refused.code, 1, "{}", refused.stdout);
    for part in [
        "cargo.bat (added, check_word)",
        "src/inline.rs (modified, inline_test)",
        "tests/gone_test.rs (deleted, test)",
        "tests/keep_test.rs (modified, test)",
    ] {
        assert!(refused.stderr.contains(part), "{part}\n{}", refused.stderr);
    }
    assert!(!refused.stderr.contains("b.txt ("), "check inputs go through fingerprints: {}", refused.stderr);
    assert!(runs(&f, &task).iter().all(|r| r["role"] != "reviewer"));
    let code = first_command_code(&f, &task);
    let pass = json!({"verdict": "PASS", "findings": {"criteria": [{"id": "AC1", "verdict": "met", "evidence": [code]}], "extra_findings": []}});
    let pass = write_home_file(&f, "review-pass.json", pass.to_string().as_bytes());
    let o = exec(&f, &["review", "record", &task, "-f", &pass, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.stderr.contains("refusing to record a PASS"), "{}", o.stderr);
    assert!(o.stderr.contains("src/inline.rs (modified, inline_test)"), "{}", o.stderr);
    assert_eq!(s(&ok(&f, &["status", &task, "--json"], &[])["status"]), "in_review");

    // AC3: the approval takes the inline test with the others
    let v = ok(&f, &["checks", "approve", "--run", &run, "--reason", "R-lean", "--json"], &[]);
    assert_eq!(
        v["protected_approved"],
        entries(&[
            ("cargo.bat", "added", "check_word"),
            ("src/inline.rs", "modified", "inline_test"),
            ("tests/gone_test.rs", "deleted", "test"),
            ("tests/keep_test.rs", "modified", "test"),
        ]),
        "{v:#}"
    );
    let approved: Vec<Value> = events(&f, &task).into_iter().filter(|e| e["type"] == "protected_change_approved").collect();
    assert_eq!(approved.len(), 1, "{approved:?}");
    assert!(
        approved[0]["detail"]["changes"].as_array().unwrap().iter().any(|c| c["path"] == "src/inline.rs"),
        "{approved:?}"
    );

    // AC3: the retry passes the gate; the reviewer's prompt has the same list
    let started = ok(&f, &review_args, &review_env);
    let reviewer = s(&started["run_id"]);
    ok(&f, &["wait", "--run", &reviewer, "--timeout", "180s", "--json"], &[]);
    let prompt = prompt_of(&f, &task, &reviewer);
    for line in [
        "- b.txt (modified, check_input)",
        "- cargo.bat (added, check_word)",
        "- src/inline.rs (modified, inline_test)",
        "- tests/gone_test.rs (deleted, test)",
        "- tests/keep_test.rs (modified, test)",
    ] {
        assert!(prompt.contains(line), "{line}\n{prompt}");
    }
    assert_eq!(f.wait_for_task_status(&task, &["pass", "reject", "unverified"], 60), "pass");
}

/// AC1 (submit), AC4's reference-only key: `submit` of a finished run takes
/// a delivery without per-file claims, keeps an optional, incomplete one as
/// written, and still refuses a citation of evidence that does not exist.
#[test]
fn submit_takes_a_delivery_without_per_file_claims_and_keeps_other_rules() {
    let f = initialised();
    let (task, run) = undeclared_candidate(&f);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let code = first_command_code(&f, &task);

    let plain = json!({
        "task_id": task, "status": "completed", "summary": "resubmitted by hand",
        "claims": [{"kind": "test_result", "text": "suite green again", "evidence": [code]}],
        "verification_requests": [{"cmd": "echo kept for reference", "cwd": "."}],
    });
    let file = write_home_file(&f, "result-plain.json", plain.to_string().as_bytes());
    let v = ok(&f, &["submit", "-f", &file, "--run", &run, "--json"], &[]);
    assert_eq!((v["ok"].clone(), v["task_status"].clone()), (json!(true), json!("in_review")), "{v}");
    let result = ok(&f, &["task", "result", &task, "--run", &run, "--json"], &[]);
    assert_eq!(result["claims"], json!([{"kind": "test_result", "text": "suite green again", "evidence": [code], "sources": []}]));

    // an optional claim naming one of the five files: accepted and kept
    let partial = json!({
        "task_id": task, "status": "completed", "summary": "with one optional note",
        "claims": [
            {"kind": "test_result", "text": "suite green again", "evidence": [code]},
            {"kind": "protected_change", "text": "tests/keep_test.rs: AC1 needed the stricter assertion", "evidence": []},
        ],
    });
    let file = write_home_file(&f, "result-partial.json", partial.to_string().as_bytes());
    let v = ok(&f, &["submit", "-f", &file, "--run", &run, "--json"], &[]);
    assert_eq!(v["task_status"], "in_review", "{v}");
    let result = ok(&f, &["task", "result", &task, "--run", &run, "--json"], &[]);
    let texts: Vec<String> = result["claims"].as_array().unwrap().iter().map(|c| s(&c["text"])).collect();
    assert_eq!(texts, ["suite green again", "tests/keep_test.rs: AC1 needed the stricter assertion"]);

    // other validation is unchanged: a code that is not evidence is rule 3
    let bad = json!({
        "task_id": task, "status": "completed", "summary": "cites nothing real",
        "claims": [{"kind": "test_result", "text": "green", "evidence": ["E999"]}],
    });
    let file = write_home_file(&f, "result-bad.json", bad.to_string().as_bytes());
    let o = exec(&f, &["submit", "-f", &file, "--run", &run, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!((o.json()["ok"].clone(), o.json()["task_status"].clone()), (json!(false), json!("unverified")));
    let invalid = run_events(&f, &task, &run, "result_invalid");
    assert_eq!(invalid.last().unwrap()["detail"]["rule"], 3, "{invalid:?}");
    assert!(!invalid.iter().any(|e| e["detail"]["rule"] == 6), "{invalid:?}");
}

// --- AC2: inline test regions ---------------------------------------------------

const REGION: &str = "pub fn a() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn a_is_one() {\n        assert_eq!(super::a(), 1);\n    }\n}\n";
const TWO_TESTS: &str = "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn first() {\n        assert!(1 < 2);\n    }\n\n    #[test]\n    fn second() {\n        assert!(2 < 3);\n    }\n}\n";
const TWO_TESTS_SWAPPED: &str = "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn second() {\n        assert!(2 < 3);\n    }\n\n    #[test]\n    fn first() {\n        assert!(1 < 2);\n    }\n}\n";
const TWO_TESTS_ONE_LEFT: &str = "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn first() {\n        assert!(1 < 2);\n    }\n}\n";
const CFG_ALL: &str = "pub fn w() {}\n\n#[cfg(all(test, windows))]\nmod win_tests {\n    #[test]\n    fn w() {\n        super::w();\n    }\n}\n";
const BARE_ATTR: &str = "pub fn c() -> bool {\n    true\n}\n\n#[test]\nfn c_is_true() {\n    assert!(super_c());\n}\n\nfn super_c() -> bool {\n    c()\n}\n";
const TOKIO: &str = "pub async fn d() {}\n\nmod async_tests {\n    #[tokio::test]\n    async fn d_runs() {\n        super::d().await;\n    }\n}\n";
const INDENTED: &str = "pub mod outer {\n    pub fn e() {}\n\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn e() {\n            super::e();\n        }\n    }\n}\n";
const NEAR_MISSES: &str = "#[latest]\nfn a() {}\n\n#[attest]\nfn b() {}\n\n#[cfg(latest)]\nmod m {\n    fn c() {}\n}\n";
const TRAILING: &str = "#[inline] // only the test calls this\nfn a() {}\n\n#[derive(Debug)] // test helper\nstruct S;\n";
const CFG_FN: &str = "#[cfg(test)]\nfn helper() -> u8 {\n    1\n}\n";
const ADDED_TEST: &str = "    #[test]\n    fn added() {\n        assert!(true);\n    }\n";

/// `REGION` with an extra test appended inside its module.
fn region_with_extra(base: &str) -> String {
    let at = base.rfind("}\n").unwrap();
    format!("{}{ADDED_TEST}{}", &base[..at], &base[at..])
}

fn crlf(text: &str) -> String {
    text.replace('\n', "\r\n")
}

/// Non-UTF-8 bytes in the production half and inside the region.
fn legacy_bytes(test_line: &str) -> Vec<u8> {
    let mut b: Vec<u8> = b"pub fn g() {}\n// caf\xe9\n\n#[cfg(test)]\nmod tests {\n    // \xff legacy\n".to_vec();
    b.extend_from_slice(test_line.as_bytes());
    b.extend_from_slice(b"}\n");
    b
}

/// AC2: every inline case the spec names, in one candidate.
#[test]
fn inline_test_regions_are_listed_only_when_existing_test_lines_change() {
    let f = initialised();
    let legacy = legacy_bytes("    #[test]\n    fn g() {\n        super::g();\n    }\n");
    let base: Vec<(&str, Vec<u8>)> = vec![
        // not listed
        ("src/prod_edit.rs", REGION.into()),
        ("src/add_tests.rs", REGION.into()),
        ("src/first_region.rs", "pub fn b() {}\n".into()),
        ("src/near_misses.rs", NEAR_MISSES.into()),
        ("src/trailing.rs", TRAILING.into()),
        ("src/cfg_fn.rs", CFG_FN.into()),
        ("src/crlf_insert.rs", crlf(REGION).into_bytes()),
        ("src/legacy_insert.rs", legacy.clone()),
        // listed
        ("src/change_line.rs", REGION.into()),
        ("src/delete_one.rs", TWO_TESTS.into()),
        ("src/reorder.rs", TWO_TESTS.into()),
        ("src/delete_region.rs", REGION.into()),
        ("src/delete_file.rs", REGION.into()),
        ("src/cfg_all.rs", CFG_ALL.into()),
        ("src/bare_attr.rs", BARE_ATTR.into()),
        ("src/tokio_case.rs", TOKIO.into()),
        ("src/indented.rs", INDENTED.into()),
        ("src/crlf_changed.rs", crlf(REGION).into_bytes()),
        ("src/legacy_changed.rs", legacy.clone()),
    ];
    let files: Vec<(&str, &[u8])> = base.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    commit_files(&f, &files, "inline fixtures");
    let (task, _) = create(&f, SPEC_NORMAL);

    let changed_assert = REGION.replace("assert_eq!(super::a(), 1);", "assert_eq!(super::a(), 2);");
    let plan = json!({
        "write": [
            {"path": "src/prod_edit.rs", "text": REGION.replacen("    1\n", "    2\n", 1)},
            {"path": "src/add_tests.rs", "text": region_with_extra(REGION)},
            {"path": "src/first_region.rs", "text": format!("pub fn b() {{}}\n\n#[cfg(test)]\nmod tests {{\n{ADDED_TEST}}}\n")},
            {"path": "src/new_file.rs", "text": REGION},
            {"path": "src/near_misses.rs", "text": NEAR_MISSES.replace("fn a() {}", "fn a() { let _ = 1; }").replace("fn c() {}", "fn c() { let _ = 2; }")},
            {"path": "src/trailing.rs", "text": TRAILING.replace("fn a() {}", "fn a() { let _ = 1; }").replace("struct S;", "struct S(u8);")},
            {"path": "src/cfg_fn.rs", "text": CFG_FN.replace("    1\n", "    2\n")},
            {"path": "src/crlf_insert.rs", "text": region_with_extra(REGION)},
            {"path": "src/legacy_insert.rs", "hex": hex(&legacy_bytes(&format!("{ADDED_TEST}    #[test]\n    fn g() {{\n        super::g();\n    }}\n")))},
            {"path": "src/change_line.rs", "text": changed_assert},
            {"path": "src/delete_one.rs", "text": TWO_TESTS_ONE_LEFT},
            {"path": "src/reorder.rs", "text": TWO_TESTS_SWAPPED},
            {"path": "src/delete_region.rs", "text": "pub fn a() -> u8 {\n    1\n}\n"},
            {"path": "src/cfg_all.rs", "text": CFG_ALL.replace("        super::w();\n", "        let _ = 0;\n")},
            {"path": "src/bare_attr.rs", "text": BARE_ATTR.replace("assert!(super_c());", "assert!(!super_c() || true);")},
            {"path": "src/tokio_case.rs", "text": TOKIO.replace("super::d().await;", "let _ = 0;")},
            {"path": "src/indented.rs", "text": INDENTED.replace("            super::e();\n", "")},
            {"path": "src/crlf_changed.rs", "text": changed_assert},
            {"path": "src/legacy_changed.rs", "hex": hex(&legacy_bytes("    #[test]\n    fn g() {}\n"))},
        ],
        "delete": ["src/delete_file.rs"],
    });
    let plan = plan_file(&f, "plan-ac2.json", &plan);
    let run = s(&dispatch(&f, &task, "hooks", Some(&plan))["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));

    let detail = diff_detail(&f, &task, &run);
    assert_eq!(
        detail["protected_changes"],
        entries(&[
            ("src/bare_attr.rs", "modified", "inline_test"),
            ("src/cfg_all.rs", "modified", "inline_test"),
            ("src/change_line.rs", "modified", "inline_test"),
            ("src/crlf_changed.rs", "modified", "inline_test"),
            ("src/delete_file.rs", "deleted", "inline_test"),
            ("src/delete_one.rs", "modified", "inline_test"),
            ("src/delete_region.rs", "modified", "inline_test"),
            ("src/indented.rs", "modified", "inline_test"),
            ("src/legacy_changed.rs", "modified", "inline_test"),
            ("src/reorder.rs", "modified", "inline_test"),
            ("src/tokio_case.rs", "modified", "inline_test"),
        ]),
        "{detail:#}"
    );
    assert_eq!(detail["inline_test_errors"], json!([]), "{detail:#}");
    // every file of the plan was really in the diff, so the misses are misses
    let changed: Vec<String> = detail["files_changed"].as_array().unwrap().iter().map(s).collect();
    for p in ["src/prod_edit.rs", "src/add_tests.rs", "src/first_region.rs", "src/new_file.rs", "src/near_misses.rs", "src/trailing.rs", "src/cfg_fn.rs", "src/crlf_insert.rs", "src/legacy_insert.rs"] {
        assert!(changed.iter().any(|c| c == p), "{p} not in the diff: {changed:?}");
    }
}

/// AC2: a file whose content cannot be read at the candidate (it became a
/// gitlink) is listed as inline_test and explained in inline_test_errors.
#[test]
fn an_unreadable_rust_file_is_listed_and_explained() {
    let f = initialised();
    commit_files(&f, &[("src/linked.rs", REGION.as_bytes())], "a file to replace with a gitlink");
    let (task, base) = create(&f, SPEC_NORMAL);
    let plan = plan_file(&f, "plan-link.json", &json!({"gitlink": [{"path": "src/linked.rs", "sha": base}]}));
    let run = s(&dispatch(&f, &task, "hooks", Some(&plan))["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let detail = diff_detail(&f, &task, &run);
    assert_eq!(detail["protected_changes"], entries(&[("src/linked.rs", "type_changed", "inline_test")]), "{detail:#}");
    let errors = detail["inline_test_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{detail:#}");
    assert_eq!(errors[0]["path"], "src/linked.rs");
    assert!(s(&errors[0]["error"]).contains("at the candidate"), "{errors:?}");
}

/// AC3: across a reroute, the list is counted from the task's base: an
/// inline change an earlier run made still counts, a file the task added
/// stays new, and a change main brought in through a merge is not the
/// task's; `integration_tip` names the tip used.
#[test]
fn inline_changes_follow_the_tasks_base_across_a_reroute_and_a_merge_of_main() {
    let f = initialised();
    commit_files(
        &f,
        &[("src/x.rs", REGION.as_bytes()), ("src/tipped.rs", REGION.as_bytes())],
        "inline fixtures",
    );
    let (task, base) = create(&f, SPEC_NORMAL);
    let changed_assert = REGION.replace("assert_eq!(super::a(), 1);", "assert_eq!(super::a(), 2);");

    // run 1 changes src/x.rs's test and adds src/fresh.rs; its result cites a
    // code that does not exist, so it is unverified
    let plan1 = plan_file(
        &f,
        "plan-r1.json",
        &json!({"write": [{"path": "src/x.rs", "text": changed_assert}, {"path": "src/fresh.rs", "text": REGION}]}),
    );
    let d1 = dispatch(&f, &task, "hooks_bad_ref", Some(&plan1));
    let run1 = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "unverified");
    assert_eq!(diff_detail(&f, &task, &run1)["protected_changes"], entries(&[("src/x.rs", "modified", "inline_test")]));

    // main changes src/tipped.rs's test; the worktree merges main
    commit_files(&f, &[("src/tipped.rs", changed_assert.as_bytes())], "main changes a test");
    let tip = common::git(&f.repo_path(), &["rev-parse", "HEAD"]);
    let wt = PathBuf::from(s(&d1["worktree"]));
    common::git(&wt, &["-c", "user.email=t@example.com", "-c", "user.name=t", "merge", "--no-edit", "main"]);

    // run 2, the reroute, changes the test of the file the task added
    let plan2 = plan_file(&f, "plan-r2.json", &json!({"write": [{"path": "src/fresh.rs", "text": changed_assert}]}));
    let d2 = dispatch(&f, &task, "hooks", Some(&plan2));
    assert_eq!(d2["worktree_reused"], true, "{d2}");
    let run2 = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let detail = diff_detail(&f, &task, &run2);
    assert_eq!(detail["protected_base"], base.as_str());
    assert_eq!(detail["integration_tip"], tip.as_str());
    assert_eq!(detail["protected_changes"], entries(&[("src/x.rs", "modified", "inline_test")]), "{detail:#}");
    // src/tipped.rs differs from the base, so leaving it out was the tip's doing
    let changed: Vec<String> = detail["files_changed"].as_array().unwrap().iter().map(s).collect();
    assert!(changed.iter().any(|c| c == "src/tipped.rs"), "{changed:?}");
}

// --- AC4: the worker prompt -------------------------------------------------------

fn assert_lean_worker_prompt(who: &str, p: &str) {
    let at = p.find("\n## Protected files\n").unwrap_or_else(|| panic!("{who}: no section\n{p}"));
    let section = &p[at..];
    assert!(section.contains("Thoth records protected changes automatically"), "{who}: {section}");
    assert!(section.contains("You do not need to declare these files"), "{who}: {section}");
    assert!(section.contains("a claim of kind protected_change is optional"), "{who}: {section}");
    assert!(section.contains("The commander reads the recorded list and approves"), "{who}: {section}");
    assert!(section.contains("inline test region"), "{who}: {section}");
    assert!(!p.contains("per file"), "{who}: {p}");
    for gone in ["declare them again", "is not accepted", "must be declared", "undeclared"] {
        assert!(!section.contains(gone), "{who}: the section still says '{gone}'\n{section}");
    }
    let shape = p
        .lines()
        .find(|l| l.starts_with("{\"task_id\":\""))
        .unwrap_or_else(|| panic!("{who}: no result.json shape\n{p}"));
    assert!(!shape.contains("verification_requests"), "{who}: {shape}");
    assert!(shape.contains("\"claims\":[{\"kind\":\"test_result\""), "{who}: {shape}");
    assert!(
        p.contains("In the summary, say what you intended, its limitations and the decisions you made. Do not reproduce file lists, command logs, exit codes or usage: Thoth already records them."),
        "{who}: {p}"
    );
    // kept: the evidence contract and the checks section
    assert!(p.contains("MUST cite at least one code from that file"), "{who}");
    assert!(p.contains("## Checks the control plane will re-run"), "{who}");
}

/// AC4: the prompt.txt of a fake Claude, codex and antigravity worker.
#[test]
fn every_worker_prompt_drops_the_per_file_duty_and_explains_the_recording() {
    let f = initialised();
    add_profile(&f, "codex-a", "openai", "codex");
    let agy_cfg = add_profile(&f, "agy-a", "google", "antigravity");
    let agy_cfg = agy_cfg.display().to_string();
    for (profile, model, extra) in [
        ("claude-b", "sonnet", vec![]),
        ("codex-a", "gpt-5.6-luna", vec![]),
        ("agy-a", "gemini-3.8-flash-low", vec![("FAKE_AGY_DATA_DIR", agy_cfg.as_str())]),
    ] {
        let (task, _) = create(&f, SPEC_NORMAL);
        let mut envs = vec![("FAKE_SCENARIO", "ok"), ("FAKE_TASK_ID", task.as_str())];
        envs.extend(extra);
        let d = ok(
            &f,
            &["dispatch", &task, "--profile", profile, "--model", model, "--effort", "medium", "--reason", REASON, "--json"],
            &envs,
        );
        let run = s(&d["run_id"]);
        wait(&f, &task);
        assert_lean_worker_prompt(profile, &prompt_of(&f, &task, &run));
    }

    // a research task keeps its source instructions
    let research = r#"{"goal":"find out","acceptance_criteria":["AC1: says where"],"risk":"normal","task_type":"research"}"#;
    let (task, _) = create(&f, research);
    let d = ok(
        &f,
        &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "research_ok"), ("FAKE_TASK_ID", task.as_str())],
    );
    wait(&f, &task);
    let p = prompt_of(&f, &task, &s(&d["run_id"]));
    assert!(p.contains("## Research claims") && p.contains("\"sources\""), "{p}");
    assert_lean_worker_prompt("claude research", &p);
}

// --- AC5: the risk-sensitive size limit -------------------------------------------

/// A valid spec of exactly `bytes` bytes, with `risk` when given.
fn sized(bytes: usize, risk: Option<&str>) -> String {
    let make = |pad: &str| {
        let mut v = json!({"goal": "g", "acceptance_criteria": ["AC1: x"], "constraints": [pad]});
        if let Some(r) = risk {
            v["risk"] = json!(r);
        }
        v.to_string()
    };
    let empty = make("").len();
    let spec = make(&"x".repeat(bytes - empty));
    assert_eq!(spec.len(), bytes);
    spec
}

fn rules(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().unwrap().iter().map(|e| s(&e["rule"])).collect()
}

fn lint(f: &Fixture, body: &str) -> (i32, Value) {
    let file = write_home_file(f, "lint.json", body.as_bytes());
    let o = exec(f, &["spec", "lint", &file, "--json"], &[]);
    (o.code, o.json())
}

/// AC5: `spec lint` at the four sizes, every risk, and invalid JSON.
#[test]
fn spec_lint_size_rules_follow_the_risk() {
    let f = initialised();
    for risk in [None, Some("low"), Some("normal"), Some("important"), Some("high")] {
        let small = matches!(risk, None | Some("low") | Some("normal"));
        for bytes in [20_480, 20_481, 40_960, 40_961] {
            let (code, v) = lint(&f, &sized(bytes, risk));
            let label = format!("risk {risk:?}, {bytes} bytes: {v}");
            let (errors, warnings) = (rules(&v, "errors"), rules(&v, "warnings"));
            match bytes {
                20_480 => {
                    assert_eq!((code, errors.len(), warnings.len()), (0, 0, 0), "{label}");
                }
                40_961 => {
                    assert_eq!(errors, ["too_large"], "{label}");
                    assert!(warnings.is_empty(), "{label}");
                    assert_eq!(code, 1, "{label}");
                }
                _ if small => {
                    assert_eq!(errors, ["large"], "{label}");
                    assert!(warnings.is_empty(), "{label}");
                    assert_eq!(code, 1, "{label}");
                    let message = s(&v["errors"][0]["message"]);
                    for part in ["20480-byte limit", "skip the mandatory spec review", "shrink the spec or split the task"] {
                        assert!(message.contains(part), "{part}: {message}");
                    }
                }
                _ => {
                    assert!(errors.is_empty(), "{label}");
                    assert_eq!(warnings, ["large"], "{label}");
                    assert_eq!(code, 0, "{label}");
                }
            }
        }
    }
    // invalid JSON: no readable risk, so normal
    for (bytes, expected) in [
        (20_480, vec!["invalid_spec"]),
        (20_481, vec!["large", "invalid_spec"]),
        (40_960, vec!["large", "invalid_spec"]),
        (40_961, vec!["too_large", "invalid_spec"]),
    ] {
        let junk = format!("{{{}", "x".repeat(bytes - 1));
        let (code, v) = lint(&f, &junk);
        assert_eq!(rules(&v, "errors"), expected, "{bytes}: {v}");
        assert!(rules(&v, "warnings").is_empty(), "{bytes}: {v}");
        assert_eq!(code, 1);
    }
}

/// AC5: `task create` and `task update-spec` refuse a 21KB normal spec and
/// write nothing; the same content at important risk is created, with the
/// warning.
#[test]
fn task_create_and_update_spec_refuse_a_large_normal_spec() {
    let f = initialised();
    let big_normal = sized(21_000, Some("normal"));
    let tasks_before = ok(&f, &["status", "--json"], &[])["tasks"].clone();
    let events_before = all_events(&f);
    let file = write_home_file(&f, "big-normal.json", big_normal.as_bytes());
    let o = exec(&f, &["task", "create", "--title", "big", "-f", &file, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.stderr.contains("large"), "{}", o.stderr);
    assert_eq!(ok(&f, &["status", "--json"], &[])["tasks"], tasks_before);
    assert_eq!(all_events(&f), events_before);

    let big_important = sized(21_000, Some("important"));
    let file = write_home_file(&f, "big-important.json", big_important.as_bytes());
    let v = ok(&f, &["task", "create", "--title", "big", "-f", &file, "--json"], &[]);
    assert!(
        v["warnings"].as_array().unwrap().iter().any(|w| s(w).starts_with("spec lint: large")),
        "{v}"
    );

    let (task, _) = create(&f, SPEC_NORMAL);
    let shown_before = ok(&f, &["task", "show", &task, "--json"], &[]);
    let task_events_before = events(&f, &task);
    let file = write_home_file(&f, "big-update.json", big_normal.as_bytes());
    let o = exec(&f, &["task", "update-spec", &task, "-f", &file, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.stderr.contains("large"), "{}", o.stderr);
    assert_eq!(ok(&f, &["task", "show", &task, "--json"], &[]), shown_before);
    assert_eq!(events(&f, &task), task_events_before);
}

/// Stands in for a spec stored before LEAN-1 (when 21KB at normal risk was
/// only warned about): no command stores one any more, so the fixture writes
/// the task's row. Everything afterwards is read through the CLI.
fn store_spec_as_before(f: &Fixture, task: &str, spec: &str) {
    f.state_conn()
        .execute("UPDATE tasks SET spec = ?1 WHERE task_id = ?2", [spec, task])
        .unwrap();
}

/// The same for a proposed spec an edit review stored before LEAN-1.
fn store_proposed_as_before(f: &Fixture, task: &str, spec: &str) {
    f.state_conn()
        .execute(
            "UPDATE events SET detail = json_set(detail, '$.proposed_spec', ?1) \
             WHERE task_id = ?2 AND type = 'spec_review'",
            [spec, task],
        )
        .unwrap();
}

/// What a refused launch must leave unchanged.
fn snapshot(f: &Fixture, task: &str) -> (Value, Vec<Value>, Vec<Value>) {
    (ok(f, &["task", "show", task, "--json"], &[]), events(f, task), runs(f, task))
}

/// AC5: spec review start (with and without --edit) lints the stored spec,
/// and confirm lints the proposed one; a blocking `large` starts no run and
/// replaces no spec.
#[test]
fn spec_review_start_and_confirm_refuse_a_stored_large_normal_spec() {
    let f = initialised();
    let big = sized(21_000, Some("normal"));
    let launch = |verb: &str, task: &str, extra: &[&str]| {
        let mut args = vec![
            "spec", "review", verb, task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium",
            "--reason", REASON, "--json",
        ];
        args.extend_from_slice(extra);
        args.into_iter().map(str::to_string).collect::<Vec<String>>()
    };

    let (task, _) = create(&f, SPEC_NORMAL);
    store_spec_as_before(&f, &task, &big);
    let before = snapshot(&f, &task);
    assert_eq!(before.0["spec"], serde_json::from_str::<Value>(&big).unwrap());
    for extra in [&[][..], &["--edit"][..]] {
        let args = launch("start", &task, extra);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = exec(&f, &args, &[("FAKE_SCENARIO", "spec_review_approve"), ("FAKE_TASK_ID", task.as_str())]);
        assert_eq!(o.code, 1, "{extra:?}: {}", o.stdout);
        assert!(o.stderr.contains("spec lint") && o.stderr.contains("large"), "{extra:?}: {}", o.stderr);
        assert_eq!(snapshot(&f, &task), before, "{extra:?}");
    }

    // confirm: an edit review of a small normal spec, whose proposed spec is
    // then one stored before LEAN-1
    let (task, _) = create(&f, r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend"],"constraints":["no new crate"],"non_goals":["rewriting the transport"],"excluded_alternatives":[],"risk":"normal"}"#);
    let args = launch("start", &task, &["--edit"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let r = ok(&f, &args, &[("FAKE_SCENARIO", "spec_review_edit"), ("FAKE_TASK_ID", task.as_str())]);
    ok(&f, &["wait", "--run", &s(&r["run_id"]), "--timeout", "180s", "--json"], &[]);
    assert_eq!(ok(&f, &["spec", "review", "show", &task, "--json"], &[])["pending_edits"], true);
    store_proposed_as_before(&f, &task, &big);
    let before = snapshot(&f, &task);
    let args = launch("confirm", &task, &[]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = exec(&f, &args, &[("FAKE_SCENARIO", "spec_confirm_approve"), ("FAKE_TASK_ID", task.as_str())]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.stderr.contains("spec lint") && o.stderr.contains("large"), "{}", o.stderr);
    assert_eq!(snapshot(&f, &task), before);
}
