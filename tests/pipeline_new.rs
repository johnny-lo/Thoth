//! ONE-1: `thoth pipeline new -f SPEC` refuses before anything is created
//! every input `task create` or `pipeline start` would refuse, then creates
//! the task and starts the pipeline on it in one command. Everything is
//! observed through the CLI (`--json` output, exit codes, `status`,
//! `events`, `pipeline show`) and MCP tool calls; fake workers only
//! (fake-claude implements, fake-codex reviews), temporary homes and
//! repositories. No test opens a database.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const REASON: &str = "pipeline new fixture: fake workers in a temporary repo";
const IMPL_A: &str = "claude-a:sonnet:medium";
const IMPL_B: &str = "claude-b:sonnet:medium";
const REVIEW: &str = "codex-a:gpt-5.6-luna:medium";
const REVIEW_2: &str = "codex-a:gpt-5.6-luna:high";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");
/// Debug-build-only seam of `pipeline new`: the pipeline start after the
/// task is created is refused.
const START_REFUSE_SEAM: &str = "THOTH_TEST_PIPELINE_NEW_START_REFUSE";
/// `pipeline start`'s seam: the background process cannot be started.
const SPAWN_FAIL_SEAM: &str = "THOTH_TEST_PIPELINE_SPAWN_FAIL";

// --- running thoth ------------------------------------------------------------

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names only are read), and without the two benchmark
/// keys. The fake implementer reads the task id from its prompt, since the
/// task does not exist before the command runs.
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
    c.env_remove("THOTH_PIPELINE_ID");
    c.env_remove(START_REFUSE_SEAM).env_remove(SPAWN_FAIL_SEAM);
    c.env("FAKE_CHECK_DIR", check_dir(f));
    c.env("FAKE_TASK_ID", "from-prompt");
    c.env("FAKE_CLAUDE_SCENARIO", "hooks").env("FAKE_CODEX_SCENARIO", "review_pass");
    c
}

fn check_dir(f: &Fixture) -> PathBuf {
    let d = f.home_path().join("check-gates");
    std::fs::create_dir_all(&d).unwrap();
    d
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
    /// The `--json` error document's `error` (the last stderr line).
    fn error(&self) -> String {
        let last = self.stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        let v: Value = serde_json::from_str(last)
            .unwrap_or_else(|e| panic!("stderr's last line is not JSON ({e}): {}", self.stderr));
        assert_eq!(v["ok"], false, "{v}");
        v["error"].as_str().unwrap_or_else(|| panic!("no error string: {v}")).to_string()
    }
}

fn exec_in(f: &Fixture, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let mut c = cmd(f);
    c.current_dir(dir);
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

fn exec(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    exec_in(f, &f.repo_path(), args, envs)
}

fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let o = exec(f, args, envs);
    assert_eq!(o.code, 0, "thoth {args:?} exited {}\nstdout: {}\nstderr: {}", o.code, o.stdout, o.stderr);
    o.json()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

// --- fixtures ---------------------------------------------------------------------

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let dir = cfg.display().to_string();
    ok(f, &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &dir, "--json"], &[]);
}

/// An initialised repository with claude-a, claude-b, codex-a and one
/// approved green check.
fn setup() -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    ok(&f, &["init", "--json"], &[]);
    for id in ["claude-a", "claude-b"] {
        add_profile(&f, id, "anthropic", "claude-code");
    }
    add_profile(&f, "codex-a", "openai", "codex");
    let check = format!("\"{FAKE_CHECK}\" exit 0");
    ok(&f, &["checks", "add", "green", "--cmd", &check, "--json"], &[]);
    ok(&f, &["checks", "approve", "--reason", "fixture checks", "--json"], &[]);
    f
}

fn spec_with(risk: &str, extra: &str) -> String {
    format!(
        r#"{{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"{risk}"{extra}}}"#
    )
}

fn spec_of(risk: &str) -> String {
    spec_with(risk, "")
}

/// A spec with a lint error: the same criterion twice.
const SPEC_LINT_ERROR: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello","AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;

/// Writes `spec` to a new file in the home and returns its path.
fn spec_file(f: &Fixture, spec: &str) -> String {
    let file = f.home_path().join(format!("spec-{}.json", rand_name()));
    std::fs::write(&file, spec).unwrap();
    file.display().to_string()
}

fn rand_name() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn set_defaults(f: &Fixture, flags: &[&str], reason: &str) {
    let mut args = vec!["pipeline", "defaults", "set"];
    args.extend_from_slice(flags);
    args.extend(["--reason", reason, "--json"]);
    ok(f, &args, &[]);
}

fn entry(text: &str) -> Value {
    let p: Vec<&str> = text.split(':').collect();
    json!({"profile": p[0], "model": p[1], "effort": p.get(2)})
}

fn entries(list: &[&str]) -> Value {
    Value::Array(list.iter().map(|e| entry(e)).collect())
}

/// `pipeline new -f FILE --title T` + `flags` + `--json`.
fn new_args<'a>(file: &'a str, flags: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["pipeline", "new", "-f", file, "--title", "one command"];
    a.extend_from_slice(flags);
    a.push("--json");
    a
}

fn task_ids(f: &Fixture) -> Vec<String> {
    let v = ok(f, &["status", "--json"], &[]);
    let tasks = v["tasks"].as_array().unwrap_or_else(|| panic!("no tasks array: {v}"));
    tasks.iter().map(|t| s(&t["task_id"])).collect()
}

fn all_event_types(f: &Fixture) -> Vec<String> {
    ok(f, &["events", "--since", "0", "--json"], &[])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| s(&e["type"]))
        .collect()
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[])["events"].as_array().unwrap().clone()
}

/// Runs `args`, asserts exit 1 with a `--json` error holding every needle,
/// and that no task and no task event came of it.
fn refused_nothing_created(f: &Fixture, args: &[&str], envs: &[(&str, &str)], needles: &[&str]) -> String {
    let tasks_before = task_ids(f);
    let events_before = all_event_types(f);
    let o = exec(f, args, envs);
    assert_eq!(o.code, 1, "{args:?} was not refused\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    assert!(o.stdout.trim().is_empty(), "a refusal printed a document: {}", o.stdout);
    let err = o.error();
    for n in needles {
        assert!(err.contains(n), "{args:?}: error lacks {n:?}: {err}");
    }
    assert_eq!(task_ids(f), tasks_before, "{args:?} created a task");
    let events_after = all_event_types(f);
    assert_eq!(events_after, events_before, "{args:?} wrote events");
    err
}

fn log_of(doc: &Value) -> String {
    doc["dir"]
        .as_str()
        .and_then(|d| std::fs::read_to_string(Path::new(d).join("pipeline.log")).ok())
        .unwrap_or_default()
}

fn wait_stopped(f: &Fixture, task: &str) -> Value {
    let o = exec(f, &["pipeline", "show", task, "--wait", "240s", "--json"], &[]);
    let v = o.json();
    assert_eq!(o.code, 0, "still running after 240s: {v:#}\n{}", log_of(&v));
    assert_eq!(v["status"], "stopped", "{v:#}");
    v
}

/// Asserts the AC5 keys and equalities of a `pipeline new --json` document.
fn assert_new_doc(v: &Value) {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["ok", "pipeline_id", "pipeline_show", "pipeline_start", "task_create", "task_id", "warnings"], "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["task_id"], v["task_create"]["task_id"], "{v}");
    assert_eq!(v["task_id"], v["pipeline_start"]["task_id"], "{v}");
    assert_eq!(v["pipeline_id"], v["pipeline_start"]["pipeline_id"], "{v}");
    assert_eq!(v["task_create"]["status"], "queued", "{v}");
    assert_eq!(v["pipeline_start"]["status"], "running", "{v}");
    let mut warnings: Vec<Value> = v["task_create"]["warnings"].as_array().unwrap().clone();
    warnings.extend(v["pipeline_start"]["warnings"].as_array().unwrap().iter().cloned());
    assert_eq!(v["warnings"], Value::Array(warnings), "{v}");
}

// --- (1) a spec lint error creates nothing -----------------------------------------

#[test]
fn a_spec_with_a_lint_error_is_refused_and_creates_no_task() {
    let f = setup();
    let file = spec_file(&f, SPEC_LINT_ERROR);
    let err = refused_nothing_created(
        &f,
        &new_args(&file, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]),
        &[],
        &[],
    );
    assert!(err.starts_with("spec lint:"), "{err}");
    assert!(task_ids(&f).is_empty());
    assert!(all_event_types(&f).iter().all(|t| t != "task_created"));
}

// --- (2) refusals that need no task -------------------------------------------------

#[test]
fn refusals_before_creation_each_name_their_check_and_leave_nothing() {
    let f = setup();
    let good = spec_file(&f, &spec_of("normal"));
    // no list flag and nothing stored
    let err = refused_nothing_created(&f, &new_args(&good, &["--reason", REASON]), &[], &["the implementer list is empty"]);
    assert!(err.contains("thoth pipeline defaults set"), "{err}");
    // an invalid --implementer entry
    refused_nothing_created(
        &f,
        &new_args(&good, &["--implementer", "nobody:sonnet:medium", "--reviewer", REVIEW, "--reason", REASON]),
        &[],
        &["invalid --implementer entry 1 of 1 ('nobody:sonnet:medium'): ", "no such profile: nobody"],
    );
    // a missing --reason
    refused_nothing_created(
        &f,
        &new_args(&good, &["--implementer", IMPL_A, "--reviewer", REVIEW]),
        &[],
        &["--reason TEXT is required"],
    );
    // a docs task
    let docs = spec_file(&f, &spec_with("normal", r#","task_type":"docs""#));
    let err = refused_nothing_created(
        &f,
        &new_args(&docs, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]),
        &[],
        &["code tasks only", "thoth task create", "thoth task accept"],
    );
    assert!(err.contains(&docs), "the error names the spec file: {err}");
    // an invalid --wait comes after pipeline start's own argument checks
    refused_nothing_created(
        &f,
        &new_args(&good, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON, "--wait", "soon"]),
        &[],
        &["invalid --wait 'soon'"],
    );
    refused_nothing_created(
        &f,
        &new_args(&good, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON, "--wait", "soon", "--timeout", "0"]),
        &[],
        &["invalid --timeout '0'"],
    );
    assert!(task_ids(&f).is_empty());
}

// --- (3) both lists from the stored defaults, run to PASS ------------------------------

#[test]
fn a_normal_spec_with_stored_lists_is_created_started_and_runs_to_pass() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "light path pair");
    let file = spec_file(&f, &spec_of("normal"));
    let v = ok(&f, &new_args(&file, &["--reason", REASON]), &[]);
    assert_new_doc(&v);
    assert_eq!(v["pipeline_show"], Value::Null);
    let task = s(&v["task_id"]);
    assert_eq!(v["task_create"]["title"], "one command");
    let st = &v["pipeline_start"]["settings"];
    assert_eq!(st["implementers"], entries(&[IMPL_A]));
    assert_eq!(st["reviewers"], entries(&[REVIEW]));
    assert_eq!(st["implementers_source"], "default");
    assert_eq!(st["reviewers_source"], "default");
    assert_eq!(st["reason"], REASON);
    // the task is the one task create would have made
    let shown = ok(&f, &["task", "show", &task, "--json"], &[]);
    assert_eq!(shown["title"], "one command");
    assert_eq!(shown["risk"], "normal");
    assert_eq!(shown["base_commit"], v["task_create"]["base_commit"]);

    let doc = wait_stopped(&f, &task);
    assert_eq!(doc["stop"]["reason"], "pass", "{doc:#}\n{}", log_of(&doc));
    assert_eq!(doc["pipeline_id"], v["pipeline_id"]);

    let ev = events(&f, &task);
    let types: Vec<String> = ev.iter().map(|e| s(&e["type"])).collect();
    assert!(!types.iter().any(|t| t == "spec_review" || t == "spec_review_skipped"), "{types:?}");
    let pos = |pred: &dyn Fn(&Value) -> bool, after: usize, what: &str| -> usize {
        ev.iter()
            .enumerate()
            .skip(after)
            .find(|(_, e)| pred(e))
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("no {what} after index {after}: {types:?}"))
    };
    let created = pos(&|e| e["type"] == "task_created", 0, "task_created");
    assert_eq!(created, 0, "task_created comes first: {types:?}");
    let started = pos(&|e| e["type"] == "pipeline_started", created + 1, "pipeline_started");
    assert_eq!(ev[started]["detail"]["pipeline_id"], v["pipeline_id"]);
    assert_eq!(ev[started]["detail"]["settings"], *st);
    let run = pos(&|e| e["type"] == "run_started", started + 1, "run_started");
    let verify = pos(&|e| e["type"] == "verify_ran", run + 1, "verify_ran");
    let review = pos(&|e| e["type"] == "review_started", verify + 1, "review_started");
    let verdict = pos(&|e| e["type"] == "verdict" && e["detail"]["verdict"] == "PASS", review + 1, "verdict PASS");
    let stopped = pos(&|e| e["type"] == "pipeline_stopped", verdict + 1, "pipeline_stopped");
    assert_eq!(ev[stopped]["detail"]["stop_reason"], "pass");
    let seqs: Vec<i64> = [created, started, run, verify, review, verdict, stopped]
        .iter()
        .map(|i| ev[*i]["seq"].as_i64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}

// --- (4) an explicit flag beats the stored default ------------------------------------

#[test]
fn an_explicit_reviewer_flag_replaces_only_the_stored_reviewer_list() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "stored pair");
    let file = spec_file(&f, &spec_of("low"));
    let v = ok(&f, &new_args(&file, &["--reviewer", REVIEW_2, "--reviewer", REVIEW, "--reason", REASON]), &[]);
    assert_new_doc(&v);
    let st = &v["pipeline_start"]["settings"];
    assert_eq!(st["reviewers"], entries(&[REVIEW_2, REVIEW]));
    assert_eq!(st["reviewers_source"], "flag");
    assert_eq!(st["reviewers_default"], Value::Null);
    assert_eq!(st["implementers"], entries(&[IMPL_A]));
    assert_eq!(st["implementers_source"], "default");
    assert_eq!(st["implementers_default"]["reason"], "stored pair");
    wait_stopped(&f, &s(&v["task_id"]));
}

// --- (5) important risk: LIGHT-1 and the spec review gate --------------------------------

#[test]
fn an_important_spec_is_refused_with_the_light_rule_or_the_heavy_path_commands() {
    let f = setup();
    let file = spec_file(&f, &spec_of("important"));
    // The command runs in the repository, so a path relative to it is one
    // plain word on every platform; a Windows temp path holds `\`, which the
    // heavy-path commands quote for the shell, so it would not appear as given.
    std::fs::copy(&file, f.repo_path().join("important-spec.json")).unwrap();
    let file = "important-spec.json".to_string();
    // without a stored list, the empty-list refusal comes first
    refused_nothing_created(
        &f,
        &new_args(&file, &["--implementer", IMPL_B, "--reason", REASON]),
        &[],
        &["the reviewer list is empty"],
    );
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "stored pair");
    let err = refused_nothing_created(
        &f,
        &new_args(&file, &["--implementer", IMPL_B, "--reason", REASON]),
        &[],
        &["important risk", "--reviewer", "low or normal risk"],
    );
    assert!(err.contains(&file), "the error names the spec file: {err}");
    let err = refused_nothing_created(&f, &new_args(&file, &["--reason", REASON]), &[], &["--implementer and --reviewer"]);
    assert!(err.contains("important"), "{err}");

    // both flags: the spec review gate, with the heavy-path commands in order
    let err = refused_nothing_created(
        &f,
        &new_args(&file, &["--implementer", IMPL_B, "--reviewer", REVIEW, "--reason", REASON, "--max-attempts", "2", "--wait", "9m"]),
        &[],
        &["important", "nothing was created", "spec review start", "--edit", "spec review confirm", "pipeline start"],
    );
    assert!(!err.contains("--skip-spec-review"), "{err}");
    let order = [
        format!("thoth task create -f {file} --title \"one command\" --json"),
        "thoth spec review start TASK --edit --profile PROFILE --model MODEL --effort LEVEL --reason TEXT --json".to_string(),
        "thoth wait --run RUN --json".to_string(),
        "thoth spec review confirm TASK --profile PROFILE --model MODEL --effort LEVEL --reason TEXT --json".to_string(),
        format!("thoth pipeline start TASK --implementer {IMPL_B} --reviewer {REVIEW} --reason \"{REASON}\" --max-attempts 2"),
        "thoth pipeline show TASK --wait 9m --json".to_string(),
    ];
    let mut at = 0;
    for step in &order {
        let found = err[at..].find(step.as_str()).unwrap_or_else(|| panic!("missing or out of order: {step}\n{err}"));
        at += found + step.len();
    }
    assert!(task_ids(&f).is_empty());
}

// --- (6) the background process cannot be started ----------------------------------------

#[test]
fn a_spawn_failure_leaves_the_task_queued_with_a_stopped_pipeline() {
    let f = setup();
    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(&f, &new_args(&file, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]), &[(SPAWN_FAIL_SEAM, "1")]);
    assert_eq!(o.code, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    let ids = task_ids(&f);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let task = &ids[0];
    assert!(err.contains(task.as_str()) && err.contains("thoth pipeline start"), "{err}");
    let st = ok(&f, &["status", task, "--json"], &[]);
    assert_eq!(st["status"], "queued", "{st}");
    assert_eq!(st["pipeline"]["stop_reason"], "spawn_failed", "{st}");
    assert!(err.contains(&s(&st["pipeline"]["pipeline_id"])), "the error names the pipeline: {err}");
}

// --- (7) the start after creation is refused ----------------------------------------------

#[test]
fn a_start_refused_after_creation_closes_the_new_task() {
    let f = setup();
    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(&f, &new_args(&file, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]), &[(START_REFUSE_SEAM, "1")]);
    assert_eq!(o.code, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    let ids = task_ids(&f);
    // status lists open tasks only; the closed one is found through events
    assert!(ids.is_empty(), "{ids:?}");
    let all = ok(&f, &["events", "--since", "0", "--json"], &[])["events"].as_array().unwrap().clone();
    let created = all.iter().find(|e| e["type"] == "task_created").expect("a task_created event");
    let task = s(&created["task_id"]);
    assert!(err.contains(&task) && err.contains("closed"), "{err}");
    let shown = ok(&f, &["task", "show", &task, "--json"], &[]);
    assert_eq!(shown["status"], "cancelled", "{shown}");
    let ev = events(&f, &task);
    let closed: Vec<&Value> = ev.iter().filter(|e| e["type"] == "task_closed").collect();
    assert_eq!(closed.len(), 1, "{ev:?}");
    assert!(s(&closed[0]["detail"]["reason"]).contains("thoth pipeline new"), "{}", closed[0]);
    assert!(!ev.iter().any(|e| e["type"] == "pipeline_started"), "{ev:?}");
}

// --- (8) --wait ----------------------------------------------------------------------------

#[test]
fn wait_returns_the_stopped_pipeline_or_exits_3_while_it_still_runs() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "stored pair");
    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(&f, &new_args(&file, &["--reason", REASON, "--wait", "240s"]), &[]);
    assert_eq!(o.code, 0, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let v = o.json();
    assert_new_doc(&v);
    let show = &v["pipeline_show"];
    assert_eq!(show["status"], "stopped", "{show:#}");
    assert_eq!(show["stop"]["reason"], "pass", "{show:#}\n{}", log_of(show));
    assert_eq!(show["pipeline_id"], v["pipeline_id"]);
    assert_eq!(show["task_id"], v["task_id"]);
    for key in ["summary", "next", "evidence"] {
        assert!(show["stop"].get(key).is_some(), "stop lacks {key}: {show}");
    }

    // a slow implementer keeps the pipeline running past D
    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(&f, &new_args(&file, &["--reason", REASON, "--wait", "3s"]), &[("FAKE_CLAUDE_SCENARIO", "slow")]);
    assert_eq!(o.code, 3, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let v = o.json();
    assert_new_doc(&v);
    assert_eq!(v["pipeline_show"]["status"], "running", "{v:#}");
    assert_eq!(v["pipeline_show"]["stop"], Value::Null);
    let task = s(&v["task_id"]);
    ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    // cancel the implementer run once it exists
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let run = loop {
        let st = ok(&f, &["status", &task, "--json"], &[]);
        if let Some(r) = st["runs"].as_array().and_then(|r| r.first()) {
            break s(&r["run_id"]);
        }
        assert!(std::time::Instant::now() < deadline, "no run appeared: {st}");
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    ok(&f, &["cancel", &run, "--json"], &[]);
    wait_stopped(&f, &task);
}

// --- (9) inside a run ----------------------------------------------------------------------

#[test]
fn thoth_run_id_set_is_refused_first() {
    let f = setup();
    let file = spec_file(&f, SPEC_LINT_ERROR);
    refused_nothing_created(&f, &new_args(&file, &[]), &[("THOTH_RUN_ID", "01FAKERUN")], &["pipeline new", "THOTH_RUN_ID"]);
}

// --- (10) MCP ------------------------------------------------------------------------------

struct McpServer {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl McpServer {
    fn start(f: &Fixture) -> Self {
        use std::process::Stdio;
        let mut c = common::scrubbed(std::process::Command::new(env!("CARGO_BIN_EXE_thoth")));
        c.arg("mcp").arg("--default-repo").arg(f.repo_path()).arg("--max-wait").arg("5s").current_dir(f.repo_path());
        for name in std::env::vars_os().map(|(name, _)| name) {
            let upper = name.to_string_lossy().to_uppercase();
            // GITENV-1: with GIT_CONFIG_KEY_n the count and values go too, so git never sees a count without its keys
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) || upper == "GIT_CONFIG_COUNT" || upper.starts_with("GIT_CONFIG_VALUE_") {
                c.env_remove(&name);
            }
        }
        for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME", "CLAUDE_PLUGIN_ROOT", "THOTH_PIPELINE_ID", "AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY", START_REFUSE_SEAM, SPAWN_FAIL_SEAM] {
            c.env_remove(var);
        }
        c.env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .env("FAKE_CHECK_DIR", check_dir(f))
            .env("FAKE_TASK_ID", "from-prompt")
            .env("FAKE_CLAUDE_SCENARIO", "hooks")
            .env("FAKE_CODEX_SCENARIO", "review_pass");
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn request(&mut self, v: Value) -> Value {
        use std::io::{BufRead, Write};
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}}))
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_pipeline_new_creates_and_starts_or_refuses_creating_nothing() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "mcp pair");
    let mut server = McpServer::start(&f);
    let list = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let matching: Vec<&Value> = tools.iter().filter(|t| t["name"] == "thoth_pipeline_new").collect();
    assert_eq!(matching.len(), 1, "listed once");
    let tool = matching[0];
    assert_eq!(tool["inputSchema"]["required"], json!(["reason"]));
    let props = tool["inputSchema"]["properties"].as_object().unwrap();
    assert!(props.contains_key("spec") && props.contains_key("file") && !props.contains_key("wait_seconds"), "{tool}");
    assert_eq!(tools.last().unwrap()["name"], "thoth_guide");

    // a lint-failing spec: a tool error, nothing created
    let bad: Value = serde_json::from_str(SPEC_LINT_ERROR).unwrap();
    let r = server.call(2, "thoth_pipeline_new", json!({"spec": bad, "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(r["result"]["structuredContent"]["exit_code"], 1, "{r}");
    assert!(s(&r["result"]["structuredContent"]["error"]).starts_with("spec lint:"), "{r}");
    assert!(task_ids(&f).is_empty());

    // both spec and file: a tool error, nothing created
    let good: Value = serde_json::from_str(&spec_of("normal")).unwrap();
    let file = spec_file(&f, &spec_of("normal"));
    let r = server.call(3, "thoth_pipeline_new", json!({"spec": good, "file": file, "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(task_ids(&f).is_empty());

    // a spec object with the stored lists: created and started at once
    let r = server.call(4, "thoth_pipeline_new", json!({"spec": good, "title": "via mcp", "reason": REASON}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let doc = &r["result"]["structuredContent"];
    assert_new_doc(doc);
    assert_eq!(doc["pipeline_show"], Value::Null);
    assert_eq!(doc["task_create"]["title"], "via mcp");
    assert_eq!(doc["pipeline_start"]["settings"]["implementers_source"], "default");
    let task = s(&doc["task_id"]);
    assert_eq!(task_ids(&f), std::slice::from_ref(&task));
    let shown = wait_stopped(&f, &task);
    assert_eq!(shown["stop"]["reason"], "pass", "{shown:#}\n{}", log_of(&shown));
}

// --- TESTGAP-1 (ONE-1 gaps) -----------------------------------------------------------------

/// The ids of every `task_created` event, oldest first (`status` lists open tasks only).
fn created_task_ids(f: &Fixture) -> Vec<String> {
    ok(f, &["events", "--since", "0", "--json"], &[])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "task_created")
        .map(|e| s(&e["task_id"]))
        .collect()
}

/// A normal-risk spec with `goal` and an extra raw JSON fragment (`"checks":[...]` or empty).
fn spec_with_goal(goal: &str, extra: &str) -> String {
    let goal = serde_json::to_string(goal).unwrap();
    format!(
        r#"{{"goal":{goal},"acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"{extra}}}"#
    )
}

#[test]
fn a_pipeline_folder_that_cannot_be_created_leaves_the_task_queued_with_a_recorded_pipeline() {
    let f = setup();
    // the folder `pipeline show`'s `dir` lives in is a plain file
    let blocker = f.home_path().join("repos").join(f.repo_id()).join("pipelines");
    assert!(!blocker.exists(), "{}", blocker.display());
    std::fs::write(&blocker, "not a folder").unwrap();

    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(&f, &new_args(&file, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]), &[]);
    assert_eq!(o.code, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    let ids = task_ids(&f);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let task = &ids[0];
    assert!(err.contains(task.as_str()), "the error names the task: {err}");

    let st = ok(&f, &["status", task, "--json"], &[]);
    assert_eq!(st["status"], "queued", "{st}");
    assert_eq!(st["pipeline"]["status"], "running", "{st}");
    assert_eq!(st["pipeline"]["step"], "starting", "{st}");
    let pipeline_id = s(&st["pipeline"]["pipeline_id"]);
    assert!(err.contains(&pipeline_id), "the error names the pipeline {pipeline_id}: {err}");
    assert!(err.contains("was recorded"), "{err}");
    assert!(err.contains(&format!("thoth pipeline start {task}")) && err.contains("again"), "{err}");
    assert_eq!(st["runs"], json!([]), "{st}");

    let ev = events(&f, task);
    let count = |t: &str| ev.iter().filter(|e| e["type"] == t).count();
    assert_eq!(count("pipeline_started"), 1, "{ev:?}");
    assert_eq!(count("task_closed"), 0, "{ev:?}");
    assert!(!ev.iter().any(|e| s(&e["type"]).starts_with("run_")), "a run event exists: {ev:?}");
    assert_eq!(ev.iter().find(|e| e["type"] == "pipeline_started").unwrap()["detail"]["pipeline_id"], pipeline_id.as_str());
}

#[test]
fn mcp_pipeline_new_with_neither_spec_nor_file_is_a_tool_error_creating_nothing() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "mcp pair");
    let mut server = McpServer::start(&f);
    let r = server.call(1, "thoth_pipeline_new", json!({"reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(s(&r["result"]["content"][0]["text"]).contains("exactly one is required"), "{r}");
    assert!(s(&r["result"]["structuredContent"]["error"]).contains("exactly one is required"), "{r}");
    assert!(task_ids(&f).is_empty());
    assert!(all_event_types(&f).iter().all(|t| t != "task_created"));
}

#[test]
fn mcp_pipeline_new_with_a_file_that_cannot_be_read_is_a_tool_error_creating_nothing() {
    let f = setup();
    set_defaults(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "mcp pair");
    // a relative path that exists in the repository
    std::fs::write(f.repo_path().join("rel-spec.json"), spec_of("normal")).unwrap();
    let missing = f.home_path().join("no-such-spec.json");
    let folder = f.home_path().join("a-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let mut server = McpServer::start(&f);
    for (id, path) in [(1u64, "rel-spec.json".to_string()), (2, missing.display().to_string()), (3, folder.display().to_string())] {
        let r = server.call(id, "thoth_pipeline_new", json!({"file": path, "reason": REASON}));
        assert_eq!(r["result"]["isError"], true, "{path}: {r}");
        assert!(s(&r["result"]["content"][0]["text"]).contains("absolute path of an existing file"), "{path}: {r}");
        assert!(task_ids(&f).is_empty(), "{path} created a task");
        assert!(all_event_types(&f).iter().all(|t| t != "task_created"), "{path} wrote task_created");
    }
    // an absolute file that holds text that is not JSON passes the tool's own check; the CLI refuses it
    let garbage = f.home_path().join("garbage-spec.json");
    std::fs::write(&garbage, "this is not json").unwrap();
    let r = server.call(4, "thoth_pipeline_new", json!({"file": garbage.display().to_string(), "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(r["result"]["structuredContent"]["exit_code"], 1, "{r}");
    assert!(!s(&r["result"]["structuredContent"]["error"]).trim().is_empty(), "{r}");
    assert!(task_ids(&f).is_empty());
    assert!(all_event_types(&f).iter().all(|t| t != "task_created"));
}

#[test]
fn without_a_title_the_task_takes_the_first_line_of_the_goal() {
    let f = setup();
    // 68 ASCII characters, then multi-byte ones: byte 69 falls inside the 69th character
    let prefix: String = "say hello to each visitor of the shop and wish them a good day, then close the door gently. ".chars().take(68).collect();
    let long_first_line = format!("{prefix}中文字符号");
    assert!(long_first_line.chars().count() > 72);
    let expected_long = format!("{prefix}中...");
    let cases = [
        ("  Say hello politely  \nthe second line is not the title".to_string(), "Say hello politely".to_string()),
        (format!("{long_first_line}\nsecond line"), expected_long),
    ];
    for (goal, expected) in cases {
        let file = spec_file(&f, &spec_with_goal(&goal, ""));
        // the title `task create -f` prints for the same file
        let created = ok(&f, &["task", "create", "-f", &file, "--json"], &[]);
        assert_eq!(created["title"], expected.as_str(), "task create: {created}");

        let args = ["pipeline", "new", "-f", &file, "--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON, "--json"];
        let v = ok(&f, &args, &[]);
        assert_new_doc(&v);
        assert_eq!(v["task_create"]["title"], created["title"], "{v}");
        assert_eq!(v["task_create"]["title"], expected.as_str(), "{v}");
        let task = s(&v["task_id"]);
        let shown = ok(&f, &["task", "show", &task, "--json"], &[]);
        assert_eq!(shown["title"], created["title"], "{shown}");
        wait_stopped(&f, &task);
    }
}

#[test]
fn without_json_the_two_human_lines_and_the_warnings_are_printed() {
    let f = setup();
    // a check that exists in the catalogue but is not approved
    let check = format!("\"{FAKE_CHECK}\" exit 0");
    ok(&f, &["checks", "add", "extra", "--cmd", &check, "--json"], &[]);
    let file = spec_file(&f, &spec_with_goal("say hello", r#","checks":["extra"]"#));
    let flags = ["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON];

    // the line `task create` prints, for another task
    let created = exec(&f, &["task", "create", "-f", &file, "--title", "human lines"], &[]);
    assert_eq!(created.code, 0, "{}", created.stderr);
    let reference_task = created_task_ids(&f).pop().unwrap();
    let reference = created.stdout.trim_end().to_string();
    assert_eq!(reference.lines().count(), 1, "{reference}");
    assert!(reference.contains(&reference_task), "{reference}");

    for (round, wait) in [("first", None), ("second", Some("240s"))] {
        let seen = created_task_ids(&f);
        let mut args = vec!["pipeline", "new", "-f", file.as_str(), "--title", "human lines"];
        args.extend_from_slice(&flags);
        if let Some(w) = wait {
            args.extend(["--wait", w]);
        }
        let o = exec(&f, &args, &[]);
        assert_eq!(o.code, 0, "[{round}] stdout: {}\nstderr: {}", o.stdout, o.stderr);
        let task = created_task_ids(&f).into_iter().find(|t| !seen.contains(t)).expect("a new task");
        let st = ok(&f, &["status", &task, "--json"], &[]);
        let pipeline_id = s(&st["pipeline"]["pipeline_id"]);

        let lines: Vec<&str> = o.stdout.lines().collect();
        if wait.is_none() {
            assert_eq!(lines.len(), 2, "[{round}] stdout is exactly two lines: {}", o.stdout);
        } else {
            assert!(lines.len() > 2, "[{round}] the pipeline show text follows: {}", o.stdout);
        }
        assert_eq!(lines[0], reference.replace(&reference_task, &task), "[{round}] first line");
        assert!(
            lines[1].contains(&pipeline_id) && lines[1].contains(&format!("started for task {task}")) && lines[1].contains(&format!("thoth pipeline show {task}")),
            "[{round}] second line: {}",
            lines[1]
        );
        let warning = o
            .stderr
            .lines()
            .find(|l| l.starts_with("warning: check"))
            .unwrap_or_else(|| panic!("[{round}] no `warning: check` line on stderr: {}", o.stderr));
        assert!(warning.contains("extra") && warning.contains("not approved"), "[{round}] {warning}");
        if wait.is_some() {
            assert!(lines[2..].join("\n").contains("stopped"), "[{round}] {}", o.stdout);
        }
        wait_stopped(&f, &task);
    }
}

// --- SMALL-12 (4): the close after a refused start fails --------------------------------------

/// Debug-build-only seam of `pipeline new`: the close of the new task after a
/// refused start fails.
const CLOSE_FAIL_SEAM: &str = "THOTH_TEST_PIPELINE_NEW_CLOSE_FAIL";

#[test]
fn a_close_that_fails_after_a_refused_start_leaves_the_task_queued_and_says_so() {
    let f = setup();
    let file = spec_file(&f, &spec_of("normal"));
    let o = exec(
        &f,
        &new_args(&file, &["--implementer", IMPL_A, "--reviewer", REVIEW, "--reason", REASON]),
        &[(START_REFUSE_SEAM, "1"), (CLOSE_FAIL_SEAM, "1")],
    );
    assert_eq!(o.code, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    // the task stays open, so status lists it
    let ids = task_ids(&f);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let task = &ids[0];
    assert!(err.contains(task.as_str()), "{err}");
    assert!(err.contains("still queued without a pipeline"), "{err}");
    assert!(err.contains(&format!("The pipeline start error: {START_REFUSE_SEAM} is set (test seam)")), "{err}");
    assert!(err.contains(&format!("The close error: {CLOSE_FAIL_SEAM} is set (test seam)")), "{err}");
    let st = ok(&f, &["status", task, "--json"], &[]);
    assert_eq!(st["status"], "queued", "{st}");
    assert_eq!(st["pipeline"], Value::Null, "{st}");
    let ev = events(&f, task);
    assert!(!ev.iter().any(|e| e["type"] == "task_closed" || e["type"] == "pipeline_started"), "{ev:?}");
}
