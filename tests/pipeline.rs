//! AUTO-1a: `thoth pipeline start | show | stop` runs a task to a short
//! conclusion in the background. Everything is observed through the CLI
//! (`--json` output, `events --task`, `status`, `review show`, `evidence`),
//! the user-visible pipeline folder and the verify slot files; fake workers
//! only (fake-claude implements, fake-codex reviews), a temporary THOTH_HOME
//! and temporary repositories. No test opens a database.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REASON: &str = "pipeline fixture: fake workers in a temporary repo";
const IMPL: &str = "claude-b:sonnet:medium";
const REVIEW: &str = "codex-a:gpt-5.6-luna:medium";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");

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
    c.env_remove("THOTH_PIPELINE_ID");
    c.env("FAKE_CHECK_DIR", check_dir(f));
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

/// Runs `args` in `dir`, which must exit 0, and returns stdout as JSON.
fn ok_in(f: &Fixture, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let o = exec_in(f, dir, args, envs);
    assert_eq!(o.code, 0, "thoth {args:?} exited {}\nstdout: {}\nstderr: {}", o.code, o.stdout, o.stderr);
    o.json()
}

fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    ok_in(f, &f.repo_path(), args, envs)
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

// --- fixtures ---------------------------------------------------------------------

/// A git repository with two commits whose line ends git keeps as written.
fn new_repo(dir: &Path) {
    common::git(dir, &["init", "-q", "--initial-branch=main"]);
    common::git(dir, &["config", "user.email", "test@example.com"]);
    common::git(dir, &["config", "user.name", "thoth test"]);
    common::git(dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    common::git(dir, &["add", "a.txt"]);
    common::git(dir, &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    common::git(dir, &["add", "b.txt"]);
    common::git(dir, &["commit", "-q", "-m", "second"]);
}

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let dir = cfg.display().to_string();
    ok(f, &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &dir, "--json"], &[]);
}

/// The check line that runs fake-check in `mode`.
fn check_cmd(mode: &str) -> String {
    format!("\"{FAKE_CHECK}\" {mode}")
}

/// Adds check `id` running fake-check `mode` in `dir`'s repository.
fn add_check_in(f: &Fixture, dir: &Path, id: &str, mode: &str) {
    ok_in(f, dir, &["checks", "add", id, "--cmd", &check_cmd(mode), "--json"], &[]);
}

fn approve_in(f: &Fixture, dir: &Path) {
    ok_in(f, dir, &["checks", "approve", "--reason", "fixture checks", "--json"], &[]);
}

/// An initialised repository with Claude and Codex profiles and one approved
/// green check.
fn setup() -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    ok(&f, &["init", "--json"], &[]);
    add_profile(&f, "claude-b", "anthropic", "claude-code");
    add_profile(&f, "claude-a", "anthropic", "claude-code");
    add_profile(&f, "codex-a", "openai", "codex");
    add_check_in(&f, &f.repo_path(), "green", "exit 0");
    approve_in(&f, &f.repo_path());
    f
}

fn create_in(f: &Fixture, dir: &Path, spec: &str) -> String {
    let file = f.home_path().join(format!("spec-{}.json", rand_name()));
    std::fs::write(&file, spec).unwrap();
    let path = file.display().to_string();
    s(&ok_in(f, dir, &["task", "create", "--title", "pipeline", "-f", &path, "--json"], &[])["task_id"])
}

fn create(f: &Fixture) -> String {
    create_in(f, &f.repo_path(), Fixture::SPEC)
}

fn rand_name() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

/// The environment a pipeline's workers read: the fake Claude implementer's
/// scenario (or `FAKE_SCENARIO_SEQUENCE` when `seq` is true) and the fake
/// Codex reviewer's.
fn worker_env<'a>(task: &'a str, implement: &'a str, review: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("FAKE_TASK_ID", task),
        ("FAKE_CLAUDE_SCENARIO", implement),
        ("FAKE_CODEX_SCENARIO", review),
    ]
}

fn start_args<'a>(task: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["pipeline", "start", task, "--implementer", IMPL, "--reviewer", REVIEW, "--reason", REASON];
    a.extend_from_slice(extra);
    a.push("--json");
    a
}

fn start_in(f: &Fixture, dir: &Path, task: &str, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let v = ok_in(f, dir, &start_args(task, extra), envs);
    assert_eq!(v["status"], "running", "{v}");
    assert_eq!(v["step"], "starting", "{v}");
    v
}

fn start(f: &Fixture, task: &str, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    start_in(f, &f.repo_path(), task, extra, envs)
}

fn show_in(f: &Fixture, dir: &Path, task: &str) -> Value {
    ok_in(f, dir, &["pipeline", "show", task, "--json"], &[])
}

fn show(f: &Fixture, task: &str) -> Value {
    show_in(f, &f.repo_path(), task)
}

/// `pipeline show --wait` until the pipeline stops; panics with the
/// pipeline log when it does not.
fn wait_stopped_in(f: &Fixture, dir: &Path, task: &str) -> Value {
    let o = exec_in(f, dir, &["pipeline", "show", task, "--wait", "240s", "--json"], &[]);
    let v = o.json();
    assert_eq!(o.code, 0, "still running after 240s: {v:#}\n{}", log_of(&v));
    assert_eq!(v["status"], "stopped", "{v:#}");
    v
}

fn wait_stopped(f: &Fixture, task: &str) -> Value {
    wait_stopped_in(f, &f.repo_path(), task)
}

/// Polls `pipeline show --json` until `pred` holds.
fn wait_show_in(f: &Fixture, dir: &Path, task: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let v = show_in(f, dir, task);
        if pred(&v) {
            return v;
        }
        if v["status"] == "stopped" || Instant::now() >= deadline {
            panic!("never saw {what}: {v:#}\n{}", log_of(&v));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_show(f: &Fixture, task: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    wait_show_in(f, &f.repo_path(), task, what, pred)
}

fn log_of(doc: &Value) -> String {
    doc["dir"]
        .as_str()
        .and_then(|d| std::fs::read_to_string(Path::new(d).join("pipeline.log")).ok())
        .unwrap_or_default()
}

fn events_in(f: &Fixture, dir: &Path, task: &str) -> Vec<Value> {
    ok_in(f, dir, &["events", "--since", "0", "--task", task, "--json"], &[])["events"].as_array().unwrap().clone()
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    events_in(f, &f.repo_path(), task)
}

fn of_type(events: &[Value], ty: &str) -> Vec<Value> {
    events.iter().filter(|e| e["type"] == ty).cloned().collect()
}

/// The steps of a show document of one kind, in order.
fn steps(doc: &Value, kind: &str) -> Vec<Value> {
    doc["steps"].as_array().unwrap().iter().filter(|s| s["kind"] == kind).cloned().collect()
}

fn stop_reason(doc: &Value) -> String {
    s(&doc["stop"]["reason"])
}

fn status(f: &Fixture, task: &str) -> Value {
    ok(f, &["status", task, "--json"], &[])
}

// --- AC2, AC4, AC7, AC8: the green path ---------------------------------------------

/// AC2/AC4/AC8: queued -> dispatch -> wait -> protected -> verify -> review ->
/// PASS, with the merge targets review show names; one terminal event;
/// every launch carries the supplied choice and the combined reason.
#[test]
fn a_green_task_runs_to_pass_and_names_the_merge_target() {
    let f = setup();
    let task = create(&f);
    let started = start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let id = s(&started["pipeline_id"]);
    assert_eq!(started["settings"]["implementers"], json!([{"profile": "claude-b", "model": "sonnet", "effort": "medium"}]));
    assert_eq!(started["settings"]["reviewers"], json!([{"profile": "codex-a", "model": "gpt-5.6-luna", "effort": "medium"}]));
    assert_eq!(started["settings"]["timeout_seconds"], Value::Null);
    assert_eq!(started["settings"]["verify_timeout_seconds"], 7200);
    assert_eq!(started["settings"]["verify_wait_seconds"], 7200);
    assert_eq!(started["settings"]["max_attempts"], 2);

    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let review = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(doc["stop"]["evidence"]["merge_branch"], review["merge_branch"]);
    assert_eq!(doc["stop"]["evidence"]["merge_commit"], review["merge_commit"]);
    assert!(review["merge_branch"].is_string(), "{review}");
    assert_eq!(doc["task_status"], "pass");
    assert_eq!(doc["pipeline_id"], id.as_str());

    let kinds: Vec<String> = doc["steps"].as_array().unwrap().iter().map(|st| s(&st["kind"])).collect();
    for k in ["dispatch", "verify", "review_start"] {
        assert!(kinds.contains(&k.to_string()), "{kinds:?}");
    }
    let d = &steps(&doc, "dispatch")[0];
    assert_eq!(d["ok"], true);
    assert_eq!(d["result"]["attempt"], 1);
    let ev = events(&f, &task);
    let step_events = of_type(&ev, "pipeline_step");
    // AUTO-1b: the dispatch is selected by kind, since recommend preflight
    // steps now come before it; the reason names the dispatch's own index.
    let dispatch_ev = step_events.iter().find(|e| e["detail"]["kind"] == "dispatch").unwrap();
    let argv: Vec<String> = dispatch_ev["detail"]["argv"].as_array().unwrap().iter().map(s).collect();
    let reason = format!("pipeline {id} step {}: {REASON}", dispatch_ev["detail"]["index"]);
    assert_eq!(
        argv,
        ["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", &reason, "--json"]
    );
    let choice = of_type(&ev, "launch_choice");
    assert_eq!(choice[0]["detail"]["reason"], reason.as_str());
    assert_eq!(of_type(&ev, "pipeline_started").len(), 1);
    assert_eq!(of_type(&ev, "pipeline_stopped").len(), 1);
    let stopped = &of_type(&ev, "pipeline_stopped")[0]["detail"];
    assert_eq!(stopped["stop_reason"], "pass");
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["steps"], doc["steps"].as_array().unwrap().len());
    assert_eq!(stopped["task_status"], "pass");
}

// --- AC1: refusals before anything is written --------------------------------------

/// Every folder under THOTH_HOME/repos/*/worktrees.
fn worktree_dirs(f: &Fixture) -> Vec<String> {
    let mut out = Vec::new();
    for repo in std::fs::read_dir(f.home_path().join("repos")).into_iter().flatten().flatten() {
        for wt in std::fs::read_dir(repo.path().join("worktrees")).into_iter().flatten().flatten() {
            out.push(wt.path().display().to_string());
        }
    }
    out.sort();
    out
}

/// What a refused start must leave as it was: every event, the task's
/// status document, the worktree folders, and `pipeline show`.
#[derive(Debug)]
struct Snapshot {
    events: Value,
    status: Option<Value>,
    worktrees: Vec<String>,
    show: (i32, Option<String>),
}

fn snapshot(f: &Fixture, task: Option<&str>) -> Snapshot {
    let shown = task.map(|t| exec(f, &["pipeline", "show", t, "--json"], &[]));
    Snapshot {
        events: ok(f, &["events", "--since", "0", "--json"], &[])["events"].clone(),
        status: task.map(|t| status(f, t)),
        worktrees: worktree_dirs(f),
        show: match shown {
            Some(o) if o.code == 0 => (0, o.json()["pipeline_id"].as_str().map(str::to_string)),
            Some(o) => (o.code, None),
            None => (1, None),
        },
    }
}

fn assert_unchanged(f: &Fixture, task: Option<&str>, before: &Snapshot, case: &str) {
    let after = snapshot(f, task);
    assert_eq!(after.events, before.events, "{case}: events changed");
    assert_eq!(after.status, before.status, "{case}: status changed");
    assert_eq!(after.worktrees, before.worktrees, "{case}: a worktree appeared");
    assert_eq!(after.show, before.show, "{case}: pipeline show changed");
}

/// Runs `args` (a `pipeline start ... --json`) and asserts the refusal: exit
/// 1, a stderr JSON error containing every one of `needles`, nothing changed.
fn refused(f: &Fixture, task: Option<&str>, args: &[&str], envs: &[(&str, &str)], needles: &[&str]) {
    let before = snapshot(f, task);
    let o = exec(f, args, envs);
    assert_eq!(o.code, 1, "{args:?} was not refused\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    for n in needles {
        assert!(err.contains(n), "{args:?}: error lacks {n:?}: {err}");
    }
    assert_unchanged(f, task, &before, &format!("{args:?}"));
}

/// `pipeline start TASK` + `pre` + `rest` + `--json`.
fn start_with(task: &str, pre: &[&str], rest: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = ["pipeline", "start", task].iter().map(|x| x.to_string()).collect();
    a.extend(pre.iter().map(|x| x.to_string()));
    a.extend(rest.iter().map(|x| x.to_string()));
    a.push("--json".into());
    a
}

#[test]
fn every_refusal_happens_before_a_pipeline_event_run_or_worktree_exists() {
    let f = setup();
    let task = create(&f);
    let t = task.as_str();
    let run = |args: Vec<String>, envs: &[(&str, &str)], needles: &[&str]| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        refused(&f, Some(t), &args, envs, needles);
    };
    let good = ["--implementer", IMPL, "--reviewer", REVIEW, "--reason", REASON];
    run(start_with(t, &good, &[]), &[("THOTH_RUN_ID", "01FAKERUN")], &["THOTH_RUN_ID"]);
    run(start_with(t, &["--reviewer", REVIEW, "--reason", REASON], &[]), &[], &["implementer list is empty"]);
    run(start_with(t, &["--implementer", IMPL, "--reason", REASON], &[]), &[], &["reviewer list is empty"]);
    for (bad, why) in [
        ("claude-b", "two or three parts"),
        ("claude-b:sonnet:medium:extra", "two or three parts"),
        ("claude-b::medium", "none of them empty"),
        ("nobody:sonnet:medium", "no such profile: nobody"),
        ("claude-b:auto:medium", "auto was removed"),
        ("auto:sonnet:medium", "auto was removed"),
        ("claude-b:sonnet:auto", "auto was removed"),
        ("claude-b:sonnet:turbo", "invalid effort level"),
        ("claude-b:sonnet", "needs an effort"),
    ] {
        run(
            start_with(t, &["--implementer", bad, "--reviewer", REVIEW, "--reason", REASON], &[]),
            &[],
            &["--implementer entry 1 of 1", why],
        );
    }
    // an invalid entry later in a list is refused although it is never used
    run(
        start_with(t, &["--implementer", IMPL, "--implementer", "claude-b:sonnet:bogus", "--reviewer", REVIEW, "--reason", REASON], &[]),
        &[],
        &["--implementer entry 2 of 2", "invalid effort level"],
    );
    run(
        start_with(t, &["--implementer", IMPL, "--reviewer", REVIEW, "--reviewer", "codex-a", "--reason", REASON], &[]),
        &[],
        &["--reviewer entry 2 of 2"],
    );
    for n in ["0", "-1", "two"] {
        run(start_with(t, &good, &["--max-attempts", n]), &[], &["--max-attempts"]);
    }
    run(start_with(t, &["--implementer", IMPL, "--reviewer", REVIEW], &[]), &[], &["--reason TEXT is required"]);
    run(start_with(t, &["--implementer", IMPL, "--reviewer", REVIEW, "--reason", "   "], &[]), &[], &["--reason TEXT is required"]);
    for (flag, value) in [("--timeout", "5x"), ("--review-timeout", "abc"), ("--verify-timeout", "-3"), ("--verify-wait", "0")] {
        run(start_with(t, &good, &[flag, value]), &[], &[&format!("invalid {flag}")]);
    }
    // unknown task, a task in another state, a document task
    refused(&f, None, &start_args("01NOSUCHTASK", &[]), &[], &["01NOSUCHTASK"]);
    let closed = create(&f);
    ok(&f, &["task", "close", &closed, "--reason", "not needed", "--json"], &[]);
    // AUTO-1b: the accepted entry states grew; cancelled is still refused
    refused(
        &f,
        Some(&closed),
        &start_args(&closed, &[]),
        &[],
        &["is 'cancelled'", "needs queued, in_review, reject, unverified, interrupted or failed"],
    );
    let docs = create_in(&f, &f.repo_path(), r#"{"goal":"write notes","acceptance_criteria":["AC1: notes exist"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","task_type":"docs"}"#);
    refused(&f, Some(&docs), &start_args(&docs, &[]), &[], &["document task", "thoth task accept"]);

    // a repository that is not initialised
    let fresh = tempfile::tempdir().unwrap();
    new_repo(fresh.path());
    let repos = || -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(f.home_path().join("repos")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    };
    let repos_before = repos();
    let o = exec_in(&f, fresh.path(), &start_args("01ANYTASK", &[]), &[]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.error().contains("is not initialised"), "{}", o.stderr);
    assert_eq!(repos(), repos_before);

    // another live pipeline for the task: the first one keeps running
    let gate = f.home_path().join("gate-ac1");
    let gate_s = gate.display().to_string();
    let mut env = worker_env(t, "hooks", "review_pass");
    env.push(("FAKE_WORKER_GATE", &gate_s));
    let first = start(&f, t, &[], &env);
    let id = s(&first["pipeline_id"]);
    wait_show(&f, t, "wait_run", |v| v["step"] == "wait_run");
    let before = snapshot(&f, Some(t));
    let o = exec(&f, &start_args(t, &[]), &env);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains(&id) && err.contains("thoth pipeline stop"), "{err}");
    let after = snapshot(&f, Some(t));
    assert_eq!(after.show, (0, Some(id.clone())));
    assert_eq!(after.worktrees, before.worktrees);
    assert_eq!(
        of_type(after.events.as_array().unwrap(), "pipeline_started").len(),
        of_type(before.events.as_array().unwrap(), "pipeline_started").len()
    );
    std::fs::write(&gate, "go").unwrap();
    assert_eq!(stop_reason(&wait_stopped(&f, t)), "pass");
}

// --- AC2: persistence, spawn failure, unexpected errors -------------------------------

fn has_pair(argv: &Value, flag: &str, value: &str) -> bool {
    argv.as_array().unwrap().windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn step_event(f: &Fixture, task: &str, kind: &str) -> Value {
    of_type(&events(f, task), "pipeline_step")
        .into_iter()
        .find(|e| e["detail"]["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} step event"))["detail"]
        .clone()
}

#[test]
fn a_started_pipeline_reads_back_the_same_after_the_cli_exits() {
    let f = setup();
    let task = create(&f);
    // a repository that never ran a pipeline reports none
    assert_eq!(exec(&f, &["pipeline", "show", &task, "--json"], &[]).code, 1);
    let listed = ok(&f, &["status", "--json"], &[]);
    let row = listed["tasks"].as_array().unwrap().iter().find(|t| t["task_id"] == task.as_str()).unwrap().clone();
    assert_eq!(row["pipeline"], Value::Null, "{row}");
    assert_eq!(status(&f, &task)["pipeline"], Value::Null);

    let gate = f.home_path().join("gate-ac2");
    let gate_s = gate.display().to_string();
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_WORKER_GATE", &gate_s));
    let started = start(
        &f,
        &task,
        &["--timeout", "30m", "--review-timeout", "45m", "--verify-timeout", "90m", "--verify-wait", "1h", "--max-attempts", "3"],
        &env,
    );
    let shown = show(&f, &task);
    assert_eq!(shown["pipeline_id"], started["pipeline_id"]);
    assert_eq!(shown["settings"], started["settings"]);
    assert_eq!(shown["settings"]["timeout_seconds"], 1800);
    assert_eq!(shown["settings"]["review_timeout_seconds"], 2700);
    assert_eq!(shown["settings"]["verify_timeout_seconds"], 5400);
    assert_eq!(shown["settings"]["verify_wait_seconds"], 3600);
    assert_eq!(shown["settings"]["max_attempts"], 3);
    assert_eq!(shown["settings"]["reason"], REASON);
    assert_eq!(shown["implementer_index"], 0);
    assert_eq!(shown["reviewer_index"], 0);
    assert_eq!(shown["stop"], Value::Null);
    let ev = events(&f, &task);
    let started_ev = &of_type(&ev, "pipeline_started")[0]["detail"];
    assert_eq!(started_ev["pipeline_id"], started["pipeline_id"]);
    assert_eq!(started_ev["settings"], started["settings"]);
    assert_eq!(started_ev["entry_status"], "queued");
    assert_eq!(started_ev["reason"], REASON);
    assert_eq!(started_ev["require_reason"], false);
    let progressed = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    // AUTO-1b: counted by kind; the recommend preflight is a step too
    assert_eq!(steps(&progressed, "dispatch").len(), 1);
    assert_eq!(steps(&progressed, "recommend").len(), 1);
    assert!(has_pair(&step_event(&f, &task, "dispatch")["argv"], "--timeout", "1800s"));
    std::fs::write(&gate, "go").unwrap();
    let done = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&done), "pass", "{done:#}");
    let review = &steps(&done, "review_start")[0];
    assert_eq!(review["ok"], true);
    assert_eq!(review["result"]["attempt"], 1);
    assert!(review["result"]["target_run_id"].is_string(), "{review}");
    assert!(has_pair(&step_event(&f, &task, "review_start")["argv"], "--timeout", "2700s"));
    let verify = step_event(&f, &task, "verify");
    assert!(has_pair(&verify["argv"], "--timeout", "5400s"));
    assert_eq!(verify["result"]["ok"], true);
    assert_eq!(verify["result"]["failures"], 0);
    assert!(verify["result"]["missing_since_last_verify"].is_array(), "{verify}");
    for key in ["pipeline_id", "index", "kind", "argv", "exit_code", "duration_ms", "ok", "run_id", "error", "stdout_bytes", "waited_for_slot_ms", "result"] {
        assert!(verify.get(key).is_some(), "{key} missing from {verify}");
    }
    let dispatch = step_event(&f, &task, "dispatch");
    assert_eq!(dispatch["waited_for_slot_ms"], Value::Null);
    assert_eq!(dispatch["result"]["rework"], false);
    assert_eq!(dispatch["result"]["folded_findings"], 0);
    assert_eq!(dispatch["result"]["handoff"], Value::Null);
    let st = status(&f, &task);
    assert_eq!(st["pipeline"]["pipeline_id"], started["pipeline_id"]);
    assert_eq!(st["pipeline"]["stop_reason"], "pass");
    assert_eq!(st["pipeline"]["status"], "stopped");
    assert_eq!(st["pipeline"]["stale"], false);
}

#[test]
fn a_background_that_cannot_start_is_recorded_as_spawn_failed() {
    let f = setup();
    let task = create(&f);
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("THOTH_TEST_PIPELINE_SPAWN_FAIL", "1"));
    let o = exec(&f, &start_args(&task, &[]), &env);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.error().contains("spawn_failed"), "{}", o.stderr);
    let doc = show(&f, &task);
    assert_eq!(doc["status"], "stopped");
    assert_eq!(stop_reason(&doc), "spawn_failed");
    assert_eq!(doc["steps"], json!([]));
    let ev = events(&f, &task);
    assert_eq!(of_type(&ev, "pipeline_started").len(), 1);
    let stopped = of_type(&ev, "pipeline_stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["detail"]["stop_reason"], "spawn_failed");
    assert_eq!(stopped[0]["detail"]["steps"], 0);
    assert_eq!(status(&f, &task)["status"], "queued");
}

#[test]
fn an_unexpected_background_error_settles_once_as_error() {
    let f = setup();
    let task = create(&f);
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("THOTH_TEST_PIPELINE_FAIL_AT", "verify_wait"));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "error", "{doc:#}");
    assert!(s(&doc["stop"]["summary"]).contains("verify_wait"), "{doc:#}");
    assert_eq!(doc["step"], "verify_wait");
    assert_eq!(of_type(&events(&f, &task), "pipeline_stopped").len(), 1);
    assert!(log_of(&doc).contains("error: injected failure at step verify_wait"), "{}", log_of(&doc));
    // the task is left where it was for the commander
    assert_eq!(status(&f, &task)["status"], "in_review");
}

// --- AC12: the fake's scenario sequence ---------------------------------------------

#[test]
fn the_fake_scenario_sequence_follows_the_launch_count_and_skips_login_probes() {
    let f = setup();
    let task = create(&f);
    let attempts = f.home_path().join("attempts-seq.txt");
    let attempts_s = attempts.display().to_string();
    let env = [
        ("FAKE_TASK_ID", task.as_str()),
        ("FAKE_ATTEMPTS_FILE", attempts_s.as_str()),
        ("FAKE_SCENARIO_SEQUENCE", "bad_result,hooks"),
        // all three are overridden by the sequence
        ("FAKE_FIRST_SCENARIO", "crash"),
        ("FAKE_CLAUDE_SCENARIO", "crash"),
        ("FAKE_SCENARIO", "crash"),
    ];
    let dispatch = ["dispatch", task.as_str(), "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"];
    let d1 = ok(&f, &dispatch, &env);
    let w1 = ok(&f, &["wait", "--run", &s(&d1["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(w1["task_status"], "unverified", "{w1}");
    let d2 = ok(&f, &dispatch, &env);
    let w2 = ok(&f, &["wait", "--run", &s(&d2["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(w2["task_status"], "in_review", "{w2}");
    // two dispatches each checked the login first, yet only the two worker
    // launches are in the record
    let lines = std::fs::read_to_string(&attempts).unwrap();
    assert_eq!(lines.lines().filter(|l| !l.trim().is_empty()).count(), 2, "{lines}");
    // past its end the list keeps its last entry: launch 3 runs `hooks`
    let d3 = ok(&f, &dispatch, &env);
    let w3 = ok(&f, &["wait", "--run", &s(&d3["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(w3["task_status"], "in_review", "{w3}");
    assert_eq!(std::fs::read_to_string(&attempts).unwrap().lines().filter(|l| !l.trim().is_empty()).count(), 3);
}

// --- AC4: dispatch, reroute and worker stops --------------------------------------------

#[test]
fn an_unverified_delivery_is_dispatched_once_more_in_the_same_attempt() {
    let f = setup();
    let task = create(&f);
    let attempts = f.home_path().join("attempts-reroute.txt");
    let attempts_s = attempts.display().to_string();
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_ATTEMPTS_FILE", &attempts_s));
    // AUTO-1b (AC10): fake-codex follows the shared sequence too, so the
    // reviewer's launch (the third) needs its own entry
    env.push(("FAKE_SCENARIO_SEQUENCE", "bad_result,hooks,review_pass"));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2, "{doc:#}");
    assert_eq!(d[0]["result"]["attempt"], 1);
    assert_eq!(d[1]["result"]["attempt"], 1);
    assert_ne!(d[0]["run_id"], d[1]["run_id"]);
    assert_eq!(of_type(&events(&f, &task), "result_invalid").len(), 1);
}

#[test]
fn a_delivery_unverified_twice_stops_with_the_result_invalid_reason() {
    let f = setup();
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "bad_result", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "unverified_twice", "{doc:#}");
    assert_eq!(steps(&doc, "dispatch").len(), 2);
    let invalid = of_type(&events(&f, &task), "result_invalid");
    let reason = invalid.last().unwrap()["detail"]["reason"].clone();
    assert!(reason.is_string(), "{invalid:?}");
    assert_eq!(doc["stop"]["evidence"]["result_invalid_reason"], reason);
    assert!(s(&doc["stop"]["summary"]).contains(reason.as_str().unwrap()), "{doc:#}");
    assert_eq!(status(&f, &task)["status"], "unverified");
}

fn run_row(f: &Fixture, task: &str, run: &str) -> Value {
    status(f, task)["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run).cloned().unwrap()
}

#[test]
fn quota_interruption_failure_cancellation_and_refusal_each_stop_with_their_reason() {
    let f = setup();
    // quota
    // AUTO-1b replaces AUTO-1a's run_interrupted stop here: a quota stop
    // hands over to the next implementer entry, and with a one-entry list
    // there is none, so the pipeline stops implementers_exhausted.
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "quota", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "implementers_exhausted", "{doc:#}");
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    let row = run_row(&f, &task, &run);
    assert_eq!(row["interrupted_reason"], "quota");
    // the exhaustion stop still says what AUTO-1a's run_interrupted stop said
    // about the run: its diagnostic and its own next command
    for key in ["interrupted_reason", "error_text", "next_step"] {
        assert_eq!(doc["stop"]["evidence"][key], row[key], "{key}");
    }
    assert_eq!(doc["stop"]["evidence"]["run_id"], run.as_str());
    if let Some(next) = row["next_step"].as_str() {
        assert!(doc["stop"]["next"].as_array().unwrap().iter().any(|n| n == next), "{doc:#}");
    }
    let entries = doc["stop"]["evidence"]["entries"].as_array().unwrap().clone();
    assert!(
        entries.iter().any(|e| e["run_id"] == run.as_str() && s(&e["outcome"]).contains("(quota)")),
        "{doc:#}"
    );
    assert_eq!(steps(&doc, "dispatch").len(), 1, "a one-entry list has nothing to hand over to");

    // a crashed worker: one resume, then (crashing again) nothing left. A
    // fresh home from here on: the quota stop above marked claude-b
    // rate_limited, and AUTO-1b's preflight skips a profile recommend calls
    // ineligible.
    let f = setup();
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "crash", "review_pass"));
    let doc = wait_stopped(&f, &task);
    let row = run_row(&f, &task, &s(&steps(&doc, "dispatch")[0]["run_id"]));
    if row["status"] == "failed" {
        assert_eq!(stop_reason(&doc), "run_failed", "{doc:#}\n{row}");
    } else {
        assert_eq!(stop_reason(&doc), "implementers_exhausted", "{doc:#}\n{row}");
        assert_eq!(steps(&doc, "resume").len(), 1, "{doc:#}");
        assert_eq!(row["interrupted_reason"], "crash", "{row}");
        assert_eq!(doc["stop"]["evidence"]["interrupted_reason"], row["interrupted_reason"]);
    }
    // either way the stop keeps the run's own diagnostic
    assert_eq!(doc["stop"]["evidence"]["error_text"], row["error_text"]);

    // a cancelled task
    let task = create(&f);
    let gate = f.home_path().join("gate-cancel");
    let gate_s = gate.display().to_string();
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_WORKER_GATE", &gate_s));
    start(&f, &task, &[], &env);
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let run = s(&steps(&at, "dispatch")[0]["run_id"]);
    ok(&f, &["cancel", &run, "--json"], &[]);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "task_cancelled", "{doc:#}");
    assert_eq!(status(&f, &task)["status"], "cancelled");

    // a refused dispatch: the profile's login check fails. AUTO-1b: an auth
    // refusal consumes the entry instead of stopping dispatch_refused; with
    // one entry the list is then used up.
    let task = create(&f);
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_LOGIN_BY_FILE", "1"));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "implementers_exhausted", "{doc:#}");
    let d = &steps(&doc, "dispatch")[0];
    assert_eq!(d["ok"], false);
    assert_eq!(d["run_id"], Value::Null);
    assert!(s(&d["error"]).contains("failed its auth check"), "{d}");
    assert!(s(&doc["stop"]["summary"]).contains("failed its auth check"), "{doc:#}");
    assert_eq!(status(&f, &task)["status"], "queued");
}

// --- AC3: the background lifecycle --------------------------------------------------

/// A pipeline whose fake implementer waits at a gate file; returns the gate.
fn gated_start(f: &Fixture, task: &str, extra_env: &[(&str, &str)]) -> (PathBuf, Value) {
    let gate = f.home_path().join(format!("gate-{}", rand_name()));
    let gate_s = gate.display().to_string();
    let mut env = worker_env(task, "hooks", "review_pass");
    env.push(("FAKE_WORKER_GATE", &gate_s));
    env.extend_from_slice(extra_env);
    let v = start(f, task, &[], &env);
    (gate, v)
}

#[test]
fn the_pipeline_keeps_working_and_heartbeating_after_its_shell_exits() {
    let f = setup();
    let task = create(&f);
    let env_log = f.home_path().join("worker-env.jsonl");
    let env_log_s = env_log.display().to_string();
    let (gate, started) = gated_start(&f, &task, &[("FAKE_ENV_LOG", &env_log_s), ("THOTH_VIA_MCP", "1")]);
    let first = show(&f, &task);
    assert!(s(&first["heartbeat_at"]) >= s(&first["started_at"]), "{first:#}");
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    assert_eq!(at["pipeline_pid"], started["pipeline_pid"]);
    let h1 = s(&at["heartbeat_at"]);
    let moved = wait_show(&f, &task, "a newer heartbeat", |v| v["heartbeat_at"].as_str().is_some_and(|h| h > h1.as_str()));
    assert_eq!(moved["step"], "wait_run");
    assert_eq!(moved["stale"], false);
    // the user-visible folder
    let dir = PathBuf::from(s(&moved["dir"]));
    assert!(dir.is_absolute(), "{dir:?}");
    assert!(dir.ends_with(Path::new("pipelines").join(s(&started["pipeline_id"]))), "{dir:?}");
    // AUTO-1b: the dispatch is found by kind (a recommend preflight comes first)
    let log = std::fs::read_to_string(dir.join("pipeline.log")).unwrap();
    let i = steps(&moved, "dispatch")[0]["index"].as_u64().unwrap();
    assert!(log.contains(&format!("step {i} dispatch: ok")), "{log}");
    let out: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("step-{i:03}.stdout.json"))).unwrap()).unwrap();
    assert_eq!(out["run_id"], steps(&moved, "dispatch")[0]["run_id"]);
    assert!(dir.join(format!("step-{i:03}.stderr.txt")).is_file());
    std::fs::write(&gate, "go").unwrap();
    assert_eq!(stop_reason(&wait_stopped(&f, &task)), "pass");
    // children carry THOTH_PIPELINE_ID, never THOTH_VIA_MCP (the worker
    // inherits what its dispatch had)
    let names: Value = serde_json::from_str(std::fs::read_to_string(&env_log).unwrap().lines().next().unwrap()).unwrap();
    let names: Vec<&str> = names["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(names.contains(&"THOTH_PIPELINE_ID"), "{names:?}");
    assert!(!names.contains(&"THOTH_VIA_MCP"), "{names:?}");
}

#[test]
fn a_killed_pipeline_turns_stale_and_a_restart_attaches_without_dispatching_again() {
    let f = setup();
    let task = create(&f);
    let (gate, started) = gated_start(&f, &task, &[]);
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let old = s(&started["pipeline_id"]);
    let pid = at["pipeline_pid"].as_u64().unwrap() as u32;
    common::kill_pid_only(pid);
    // the worker is not the pipeline's: it finishes
    std::fs::write(&gate, "go").unwrap();
    f.wait_for_task_status(&task, &["in_review"], 120);
    let stale = wait_show(&f, &task, "stale", |v| v["stale"] == true);
    assert_eq!(stale["status"], "running");
    assert_eq!(status(&f, &task)["pipeline"]["stale"], true);
    let listed = ok(&f, &["status", "--json"], &[]);
    let row = listed["tasks"].as_array().unwrap().iter().find(|t| t["task_id"] == task.as_str()).unwrap().clone();
    assert_eq!(row["pipeline"]["pipeline_id"], old.as_str());
    // the restart settles the old one as stopped/stale, then continues
    let again = start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let ev = events(&f, &task);
    let stopped = of_type(&ev, "pipeline_stopped");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["detail"]["pipeline_id"], old.as_str());
    assert_eq!(stopped[0]["detail"]["stop_reason"], "stale");
    let doc = wait_stopped(&f, &task);
    assert_eq!(doc["pipeline_id"], again["pipeline_id"]);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    assert!(steps(&doc, "dispatch").is_empty(), "no second dispatch: {doc:#}");
    assert_eq!(of_type(&events(&f, &task), "pipeline_started")[1]["detail"]["entry_status"], "in_review");
}

// SMALL-11 changed this: `thoth status` settles a run whose runner is dead as
// interrupted (reason crash), the pipeline reads status while it waits, so it
// resumes the run once as for any crash instead of stopping `runner_lost`.
#[test]
fn a_lost_runner_is_settled_as_a_crash_and_the_pipeline_resumes_it_once() {
    let f = setup();
    let task = create(&f);
    let (_gate, _) = gated_start(&f, &task, &[]);
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let run = s(&steps(&at, "dispatch")[0]["run_id"]);
    let deadline = Instant::now() + Duration::from_secs(60);
    let runner = loop {
        let row = run_row(&f, &task, &run);
        if let (Some(pid), "running") = (row["runner_pid"].as_u64(), row["status"].as_str().unwrap_or("")) {
            break pid as u32;
        }
        assert!(Instant::now() < deadline, "the run never started: {row}");
        std::thread::sleep(Duration::from_millis(200));
    };
    common::kill_pid_only(runner);
    // the pipeline does not stop: it resumes the crashed run (wait_show panics
    // when the pipeline stops, so a runner_lost stop fails here)
    let doc = wait_show(&f, &task, "a resume step", |v| !steps(v, "resume").is_empty());
    assert_ne!(doc["status"], "stopped", "{doc:#}");
    let resumes = steps(&doc, "resume");
    assert_eq!(resumes.len(), 1, "{doc:#}");
    assert_eq!(resumes[0]["run_id"], run.as_str(), "{doc:#}");
    assert!(s(&resumes[0]["reason"]).contains("crash"), "{doc:#}");
    // `status` settled the dead runner's run as interrupted with reason crash.
    // The resume returns the same run to running, and the pipeline resumes it
    // on its own schedule, so what a test can read afterwards is the event
    // that settlement wrote, not the run's status at that moment.
    let settled: Vec<Value> = events(&f, &task)
        .into_iter()
        .filter(|e| e["type"] == "status_change" && e["run_id"] == run.as_str() && e["detail"]["settled_by"] == "status")
        .collect();
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0]["detail"]["to"], "interrupted", "{settled:?}");
    assert_eq!(settled[0]["detail"]["interrupted_reason"], "crash", "{settled:?}");
    assert_ne!(show(&f, &task)["stop"]["reason"], "runner_lost");
}

// --- AC5: the protected-change gate --------------------------------------------------

const KEEP_TEST: &str = "#[test]\nfn keep() { assert!(true); }\n";
const INLINE_BASE: &str = "pub fn one() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn one_is_one() {\n        assert_eq!(super::one(), 1);\n    }\n}\n";

fn commit_files(f: &Fixture, files: &[(&str, &str)], msg: &str) {
    let repo = f.repo_path();
    for (path, body) in files {
        let p = repo.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        common::git(&repo, &["add", "--", path]);
    }
    common::git(&repo, &["commit", "-q", "-m", msg]);
}

fn plan_file(f: &Fixture, plan: &Value) -> String {
    let p = f.home_path().join(format!("plan-{}.json", rand_name()));
    std::fs::write(&p, plan.to_string()).unwrap();
    p.display().to_string()
}

fn entries(list: &[(&str, &str, &str)]) -> Value {
    let mut sorted = list.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    Value::Array(sorted.iter().map(|(p, st, k)| json!({"path": p, "status": st, "kind": k})).collect())
}

fn newest_diff(f: &Fixture, task: &str, run: &str) -> Value {
    ok(f, &["evidence", "list", task, "--json"], &[])["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run)
        .cloned()
        .unwrap_or_else(|| panic!("run {run} has no diff evidence"))
}

#[test]
fn undeclared_protected_changes_stop_for_approval_and_a_restart_continues_the_candidate() {
    let f = setup();
    commit_files(&f, &[("tests/keep_test.rs", KEEP_TEST), ("src/inline.rs", INLINE_BASE)], "protected base");
    let task = create(&f);
    let plan = plan_file(
        &f,
        &json!({"write": [
            {"path": "tests/keep_test.rs", "text": "#[test]\nfn keep() { assert_eq!(1, 1); }\n"},
            {"path": "src/inline.rs", "text": INLINE_BASE.replace("assert_eq!(super::one(), 1)", "assert!(super::one() > 0)")},
            {"path": "fake-check.cfg", "text": "a check word\n"},
        ]}),
    );
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_EDIT_PLAN", &plan));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "needs_approval", "{doc:#}");
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    let expected = entries(&[
        ("fake-check.cfg", "added", "check_word"),
        ("src/inline.rs", "modified", "inline_test"),
        ("tests/keep_test.rs", "modified", "test"),
    ]);
    assert_eq!(doc["stop"]["evidence"]["changes"], expected, "{doc:#}");
    assert_eq!(doc["stop"]["evidence"]["diff"], newest_diff(&f, &task, &run)["code"]);
    assert_eq!(doc["stop"]["evidence"]["run_id"], run.as_str());
    let summary = s(&doc["stop"]["summary"]);
    for line in ["fake-check.cfg (added, check_word)", "src/inline.rs (modified, inline_test)", "tests/keep_test.rs (modified, test)"] {
        assert!(summary.contains(line), "{line}: {summary}");
    }
    let next: Vec<String> = doc["stop"]["next"].as_array().unwrap().iter().map(s).collect();
    assert!(next.contains(&format!("thoth review packet {task}")), "{next:?}");
    assert!(next.contains(&format!("thoth checks approve --run {run} --reason TEXT")), "{next:?}");
    assert!(steps(&doc, "verify").is_empty() && steps(&doc, "review_start").is_empty(), "{doc:#}");
    // the packet lists the same changes; nothing was approved for the commander
    let packet = ok(&f, &["review", "packet", &task, "--json"], &[]);
    let listed: Vec<Value> = packet["protected_changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["run_id"] == run.as_str() && c["kind"] != "check_input")
        .map(|c| json!({"path": c["path"], "status": c["status"], "kind": c["kind"]}))
        .collect();
    assert_eq!(Value::Array(listed), expected);
    assert!(of_type(&events(&f, &task), "protected_change_approved").is_empty());
    assert_eq!(status(&f, &task)["status"], "in_review");

    let approved = ok(&f, &["checks", "approve", "--run", &run, "--reason", "read the change", "--json"], &[]);
    assert_eq!(approved["protected_approved"], expected, "{approved:#}");
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    assert!(steps(&doc, "dispatch").is_empty());
    assert_eq!(steps(&doc, "review_start")[0]["result"]["target_run_id"], run.as_str());
}

#[test]
fn a_candidate_whose_evidence_cannot_be_recorded_stops_protected_unreadable() {
    let f = setup();
    let task = create(&f);
    // the blob store cannot be written: no diff evidence can be recorded
    let blobs = f.home_path().join("repos").join(f.repo_id()).join("blobs");
    let _ = std::fs::remove_dir_all(&blobs);
    std::fs::write(&blobs, "not a folder").unwrap();
    start(&f, &task, &[], &worker_env(&task, "ok", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "protected_unreadable", "{doc:#}\n{}", log_of(&doc));
    let summary = s(&doc["stop"]["summary"]);
    assert!(summary.contains("no diff evidence") && summary.contains("not known"), "{summary}");
    assert!(doc["stop"]["evidence"]["warnings"].as_array().is_some_and(|w| !w.is_empty()), "{doc:#}");
    assert!(steps(&doc, "verify").is_empty());
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    assert!(
        !ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"].as_array().unwrap().iter().any(|e| e["kind"] == "diff"),
        "no diff evidence was made up"
    );
    // once the store works again, a restart records it and goes on
    std::fs::remove_file(&blobs).unwrap();
    start(&f, &task, &[], &worker_env(&task, "ok", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    assert_eq!(newest_diff(&f, &task, &run)["detail"]["protected_changes"], json!([]));
}

#[test]
fn main_line_changes_the_candidate_merely_carries_are_not_held_for_approval() {
    let f = setup();
    commit_files(&f, &[("tests/keep_test.rs", KEEP_TEST)], "protected base");
    let task = create(&f);
    let merged = "#[test]\nfn keep() { assert_eq!(2, 2); }\n";
    // the main line moves on after the task was created ...
    commit_files(&f, &[("tests/keep_test.rs", merged)], "main line test change");
    // ... and the candidate brings the same content in
    let plan = plan_file(&f, &json!({"write": [{"path": "tests/keep_test.rs", "text": merged}]}));
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_EDIT_PLAN", &plan));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    assert_eq!(newest_diff(&f, &task, &run)["detail"]["protected_changes"], json!([]));
}

// --- AC7: verify ---------------------------------------------------------------------

fn verify_codes(doc: &Value) -> Vec<String> {
    let dir = PathBuf::from(s(&doc["dir"]));
    let mut out = Vec::new();
    for st in steps(doc, "verify") {
        let file = dir.join(format!("step-{:03}.stdout.json", st["index"].as_u64().unwrap()));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        for r in v["results"].as_array().unwrap() {
            out.push(format!("{}={}", r["check_id"].as_str().unwrap(), r["evidence"].as_str().unwrap_or("-")));
        }
    }
    out
}

fn evidence_content(f: &Fixture, task: &str, code: &str) -> String {
    s(&ok(f, &["evidence", "show", code, "--task", task, "--json"], &[])["content"])
}

#[test]
fn verify_children_inherit_the_commanders_build_limits_at_a_lower_priority() {
    let f = setup();
    add_check_in(&f, &f.repo_path(), "env", "env");
    add_check_in(&f, &f.repo_path(), "prio", "priority");
    approve_in(&f, &f.repo_path());
    let task = create(&f);
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("CARGO_BUILD_JOBS", "2"));
    env.push(("RUST_TEST_THREADS", "2"));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    let codes = verify_codes(&doc);
    let code = |id: &str| codes.iter().find_map(|c| c.strip_prefix(&format!("{id}="))).unwrap().to_string();
    let content = evidence_content(&f, &task, &code("env"));
    assert!(content.contains("CARGO_BUILD_JOBS=2"), "{content}");
    assert!(content.contains("RUST_TEST_THREADS=2"), "{content}");
    let prio = evidence_content(&f, &task, &code("prio"));
    if cfg!(windows) {
        assert!(prio.contains("below_normal"), "{prio}");
    }
    assert!(steps(&doc, "verify")[0]["waited_for_slot_ms"].as_i64().unwrap() >= 1);
}

#[test]
fn a_red_verify_is_tried_once_more_and_stops_verify_failed_when_still_red() {
    // red, then green: one more try, then the review
    let f = setup();
    add_check_in(&f, &f.repo_path(), "flaky", "seq flaky 1 0");
    approve_in(&f, &f.repo_path());
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    let v = steps(&doc, "verify");
    assert_eq!(v.len(), 2, "{doc:#}");
    assert_eq!(v[0]["ok"], false);
    assert_eq!(v[0]["result"]["failures"], 1);
    assert_eq!(v[1]["ok"], true);

    // red twice
    let f = setup();
    add_check_in(&f, &f.repo_path(), "red", "exit 3");
    approve_in(&f, &f.repo_path());
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "verify_failed", "{doc:#}");
    let attempts = doc["stop"]["evidence"]["attempts"].as_array().unwrap().clone();
    assert_eq!(attempts.len(), 2);
    let codes = verify_codes(&doc);
    for (i, a) in attempts.iter().enumerate() {
        let failed = a["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{a}");
        assert_eq!(failed[0]["check_id"], "red");
        assert_eq!(failed[0]["exit_code"], 3);
        let red_codes: Vec<&String> = codes.iter().filter(|c| c.starts_with("red=")).collect();
        assert_eq!(format!("red={}", s(&failed[0]["evidence"])), *red_codes[i]);
    }
    assert!(s(&doc["stop"]["summary"]).contains("red (exit 3)"), "{doc:#}");
    assert!(steps(&doc, "review_start").is_empty());
    assert!(!slot_files(&f).iter().any(|_| true), "the slot was released");
}

#[test]
fn a_refused_or_missing_check_blocks_verify_with_its_hint() {
    let f = setup();
    // a pending (unapproved) check is refused by verify
    add_check_in(&f, &f.repo_path(), "pending", "exit 0");
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "verify_blocked", "{doc:#}");
    assert!(s(&doc["stop"]["summary"]).contains("check pending"), "{doc:#}");
    assert!(doc["stop"]["next"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().starts_with("thoth checks approve")));
    assert_eq!(steps(&doc, "verify").len(), 1, "a refusal is not retried");

    // no approved check at all
    ok(&f, &["checks", "remove", "pending", "--json"], &[]);
    ok(&f, &["checks", "remove", "green", "--json"], &[]);
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "verify_blocked", "{doc:#}");
    assert!(s(&doc["stop"]["summary"]).contains("no approved check"), "{doc:#}");
}

/// The slot files under THOTH_HOME/verify-slots, sorted.
fn slot_files(f: &Fixture) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(f.home_path().join("verify-slots"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("slot-"))
        .collect();
    v.sort();
    v
}

// --- AC6: verify capacity --------------------------------------------------------------

#[test]
fn verify_slots_is_a_setting_from_one_to_eight_defaulting_to_one() {
    let f = setup();
    let list = ok(&f, &["config", "list", "--json"], &[]);
    let entry = list["settings"].as_array().unwrap().iter().find(|e| e["key"] == "verify-slots").cloned().unwrap();
    assert_eq!(entry["kind"], "integer", "{entry}");
    assert_eq!(entry["value"], "1");
    assert_eq!(entry["default"], "1");
    assert_eq!(entry["env"], Value::Null);
    assert_eq!(entry["settable"], true);
    assert_eq!(entry["source"], "default");
    ok(&f, &["config", "set", "verify-slots", "1", "--json"], &[]);
    let got = ok(&f, &["config", "get", "verify-slots", "--json"], &[]);
    assert_eq!(got["value"], "1", "{got}");
    ok(&f, &["config", "set", "verify-slots", "8", "--json"], &[]);
    assert_eq!(ok(&f, &["config", "get", "verify-slots", "--json"], &[])["value"], "8");
    for bad in ["0", "9", "x", "2.5"] {
        let o = exec(&f, &["config", "set", "verify-slots", bad, "--json"], &[]);
        assert_eq!(o.code, 1, "{bad}: {}", o.stdout);
        assert!(o.error().contains("1 to 8"), "{bad}: {}", o.stderr);
    }
    assert_eq!(ok(&f, &["config", "get", "verify-slots", "--json"], &[])["value"], "8");
    ok(&f, &["config", "unset", "verify-slots", "--json"], &[]);
    assert_eq!(ok(&f, &["config", "get", "verify-slots", "--json"], &[])["value"], "1");
    let list = ok(&f, &["config", "list", "--json"], &[]);
    let entry = list["settings"].as_array().unwrap().iter().find(|e| e["key"] == "verify-slots").cloned().unwrap();
    assert_eq!(entry["source"], "default", "{entry}");
}

/// A second repository under the same THOTH_HOME, initialised, with a
/// gated check `gated` that waits for `NAME.go`.
fn second_repo(f: &Fixture, name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    new_repo(dir.path());
    ok_in(f, dir.path(), &["init", "--json"], &[]);
    add_check_in(f, dir.path(), "gated", &format!("gate {name}"));
    approve_in(f, dir.path());
    dir
}

/// `start` and `end` lines of fake-check's gate log for `name`: (ms, pid).
/// (milliseconds since the epoch, pid) per line of a gate log.
type GateLines = Vec<(u128, u32)>;

fn gate_log(f: &Fixture, name: &str) -> (GateLines, GateLines) {
    let text = std::fs::read_to_string(check_dir(f).join(format!("{name}.log"))).unwrap_or_default();
    let (mut starts, mut ends) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 4 {
            let entry = (parts[3].parse().unwrap(), parts[2].parse().unwrap());
            if parts[0] == "start" { starts.push(entry) } else { ends.push(entry) }
        }
    }
    (starts, ends)
}

fn open_gate(f: &Fixture, name: &str) {
    std::fs::write(check_dir(f).join(format!("{name}.go")), "go").unwrap();
}

fn slot_owner(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn with_one_slot_a_second_pipeline_waits_until_the_first_verify_releases_it() {
    let f = setup();
    add_check_in(&f, &f.repo_path(), "gated", "gate A");
    approve_in(&f, &f.repo_path());
    let other = second_repo(&f, "B");
    let t1 = create(&f);
    let t2 = create_in(&f, other.path(), Fixture::SPEC);
    let p1 = start(&f, &t1, &[], &worker_env(&t1, "hooks", "review_pass"));
    let at = wait_show(&f, &t1, "verify", |v| v["step"] == "verify" && !gate_log(&f, "A").0.is_empty());
    let p2 = start_in(&f, other.path(), &t2, &[], &worker_env(&t2, "hooks", "review_pass"));
    wait_show_in(&f, other.path(), &t2, "verify_wait", |v| v["step"] == "verify_wait");
    let files = slot_files(&f);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("slot-1.json"), "{files:?}");
    let owner = slot_owner(&files[0]);
    assert_eq!(owner["pipeline_id"], p1["pipeline_id"]);
    assert_eq!(owner["task_id"], t1.as_str());
    assert_eq!(owner["run_id"], steps(&at, "dispatch")[0]["run_id"]);
    assert_eq!(owner["pid"], at["pipeline_pid"]);
    assert!(owner["acquired_at"].is_string(), "{owner}");
    assert!(gate_log(&f, "B").0.is_empty(), "B must not have started");
    open_gate(&f, "A");
    wait_show_in(&f, other.path(), &t2, "B in verify", |v| v["step"] == "verify" && !gate_log(&f, "B").0.is_empty());
    open_gate(&f, "B");
    let d1 = wait_stopped(&f, &t1);
    let d2 = wait_stopped_in(&f, other.path(), &t2);
    assert_eq!(stop_reason(&d1), "pass", "{d1:#}");
    assert_eq!(stop_reason(&d2), "pass", "{d2:#}");
    assert_eq!(d2["pipeline_id"], p2["pipeline_id"]);
    let (a_starts, a_ends) = gate_log(&f, "A");
    let (b_starts, _) = gate_log(&f, "B");
    assert!(a_ends[0].0 <= b_starts[0].0, "B's verify began before A's ended: {a_starts:?} {a_ends:?} {b_starts:?}");
    let waited = steps(&d2, "verify")[0]["waited_for_slot_ms"].as_i64().unwrap();
    let first_waited = steps(&d1, "verify")[0]["waited_for_slot_ms"].as_i64().unwrap();
    assert!(waited > first_waited, "{waited} vs {first_waited}");
    assert!(slot_files(&f).is_empty(), "every exit releases its slot");
}

#[test]
fn two_slots_admit_two_verifies_and_lowering_the_limit_keeps_live_files() {
    let f = setup();
    add_check_in(&f, &f.repo_path(), "gated", "gate C");
    approve_in(&f, &f.repo_path());
    let other = second_repo(&f, "D");
    ok(&f, &["config", "set", "verify-slots", "2", "--json"], &[]);
    let t1 = create(&f);
    let t2 = create_in(&f, other.path(), Fixture::SPEC);
    start(&f, &t1, &[], &worker_env(&t1, "hooks", "review_pass"));
    start_in(&f, other.path(), &t2, &[], &worker_env(&t2, "hooks", "review_pass"));
    wait_show(&f, &t1, "C in verify", |v| v["step"] == "verify" && !gate_log(&f, "C").0.is_empty());
    wait_show_in(&f, other.path(), &t2, "D in verify", |v| v["step"] == "verify" && !gate_log(&f, "D").0.is_empty());
    let files = slot_files(&f);
    assert_eq!(files.len(), 2, "{files:?}");
    ok(&f, &["config", "set", "verify-slots", "1", "--json"], &[]);
    assert_eq!(slot_files(&f), files, "lowering the limit deletes no live ownership file");
    open_gate(&f, "C");
    open_gate(&f, "D");
    assert_eq!(stop_reason(&wait_stopped(&f, &t1)), "pass");
    assert_eq!(stop_reason(&wait_stopped_in(&f, other.path(), &t2)), "pass");
    assert!(slot_files(&f).is_empty());
}

#[test]
fn dead_reused_or_malformed_slot_owners_are_recovered_and_a_timeout_lists_the_holders() {
    let f = setup();
    let dir = f.home_path().join("verify-slots");
    std::fs::create_dir_all(&dir).unwrap();
    // a pid that is gone: a finished child's
    let mut child = std::process::Command::new("git").arg("--version").stdout(std::process::Stdio::null()).spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let owner = |pid: u32| json!({"pid": pid, "pipeline_id": "01OLDPIPELINE", "task_id": "01OLDTASK", "run_id": "01OLDRUN", "acquired_at": "2026-01-01T00:00:00Z"}).to_string();
    for (case, body) in [
        ("dead owner", owner(dead)),
        ("pid now runs another program", owner(std::process::id())),
        ("malformed", "{not json".to_string()),
    ] {
        std::fs::write(dir.join("slot-1.json"), &body).unwrap();
        let task = create(&f);
        start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
        let doc = wait_stopped(&f, &task);
        assert_eq!(stop_reason(&doc), "pass", "{case}: {doc:#}");
        assert!(slot_files(&f).is_empty(), "{case}");
    }

    // a live owner is not taken over: the second pipeline times out
    add_check_in(&f, &f.repo_path(), "gated", "gate E");
    approve_in(&f, &f.repo_path());
    let other = tempfile::tempdir().unwrap();
    new_repo(other.path());
    ok_in(&f, other.path(), &["init", "--json"], &[]);
    add_check_in(&f, other.path(), "green", "exit 0");
    approve_in(&f, other.path());
    let t1 = create(&f);
    let p1 = start(&f, &t1, &[], &worker_env(&t1, "hooks", "review_pass"));
    wait_show(&f, &t1, "E in verify", |v| v["step"] == "verify" && !gate_log(&f, "E").0.is_empty());
    let t2 = create_in(&f, other.path(), Fixture::SPEC);
    // the slot is never freed while t2 waits (gate E stays closed), so 1s
    // is as good a timeout as 3s
    start_in(&f, other.path(), &t2, &["--verify-wait", "1s"], &worker_env(&t2, "hooks", "review_pass"));
    let d2 = wait_stopped_in(&f, other.path(), &t2);
    assert_eq!(stop_reason(&d2), "verify_wait_timeout", "{d2:#}");
    let holders = d2["stop"]["evidence"]["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1, "{d2:#}");
    assert_eq!(holders[0]["pipeline_id"], p1["pipeline_id"]);
    let file = s(&holders[0]["file"]);
    assert!(s(&d2["stop"]["summary"]).contains(&file), "{d2:#}");
    assert!(steps(&d2, "verify").is_empty());
    open_gate(&f, "E");
    assert_eq!(stop_reason(&wait_stopped(&f, &t1)), "pass");
    assert!(slot_files(&f).is_empty());
}

// --- AC8: the review and its verdict ---------------------------------------------------

#[test]
fn reject_and_an_invalid_verdict_stop_with_the_review_brief_first() {
    let f = setup();
    let task = create(&f);
    // AUTO-1b replaces AUTO-1a's `reject` stop: the rework takes the next
    // implementer entry, and a one-entry list has none.
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_reject"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "implementers_exhausted", "{doc:#}");
    let review = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(review["verdict"], "REJECT");
    // the exhaustion stop still leads with the review brief and names the
    // reviewer run, as AUTO-1a's reject stop did
    if let Some(brief) = review["brief"].as_str() {
        assert!(s(&doc["stop"]["summary"]).starts_with(brief.trim()), "{doc:#}");
    }
    assert_eq!(doc["stop"]["evidence"]["reviewer_run_id"], review["reviewer_run_id"]);
    let review_id = s(&review["review_id"]);
    assert_eq!(doc["stop"]["evidence"]["review_id"], review_id.as_str());
    let entries = doc["stop"]["evidence"]["entries"].as_array().unwrap().clone();
    assert!(entries.iter().any(|e| s(&e["outcome"]).contains(&review_id)), "{doc:#}");
    assert_eq!(steps(&doc, "dispatch").len(), 1, "the list holds no entry after the first");
    assert_eq!(status(&f, &task)["status"], "reject");

    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_bad"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "review_unverified", "{doc:#}");
    assert_eq!(status(&f, &task)["status"], "unverified");
}

#[test]
fn a_pass_whose_branch_moved_stops_merge_target_unavailable() {
    let f = setup();
    let task = create(&f);
    let gate = f.home_path().join("review-gate");
    let gate_s = gate.display().to_string();
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_REVIEW_GATE", &gate_s));
    start(&f, &task, &[], &env);
    let at = wait_show(&f, &task, "wait_review", |v| v["step"] == "wait_review");
    let run = s(&steps(&at, "dispatch")[0]["run_id"]);
    let wt = PathBuf::from(s(&run_row(&f, &task, &run)["worktree"]));
    common::git(&wt, &["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "moved after the candidate"]);
    std::fs::write(&gate, "go").unwrap();
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "merge_target_unavailable", "{doc:#}");
    let review = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(review["verdict"], "PASS");
    assert_eq!(review["merge_branch"], Value::Null);
    assert_eq!(status(&f, &task)["status"], "pass");
}

#[test]
fn a_pending_edit_fix_is_handed_back_for_the_commanders_judgement() {
    let f = setup();
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    commit_files(&f, &[("src/greet.rs", &lines)], "greet");
    let task = create(&f);
    let d = ok(
        &f,
        &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_TASK_ID", task.as_str()), ("FAKE_CLAUDE_SCENARIO", "ok")],
    );
    ok(&f, &["wait", "--run", &s(&d["run_id"]), "--timeout", "120s", "--json"], &[]);
    let code = s(&ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"][0]["code"]);
    let plan = plan_file(
        &f,
        &json!({"document": {
            "verdict": "REJECT",
            "findings": {
                "criteria": [{"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"}],
                "extra_findings": [],
            },
            "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}],
        }}),
    );
    let edit = ok(
        &f,
        &["review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--edit", "--reason", REASON, "--json"],
        &[("FAKE_TASK_ID", task.as_str()), ("FAKE_CLAUDE_SCENARIO", "review_fix"), ("FAKE_FIX_PLAN", plan.as_str())],
    );
    let edit_run = s(&edit["run_id"]);
    ok(&f, &["wait", "--run", &edit_run, "--timeout", "120s", "--json"], &[]);
    let before = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(before["pending_fix"], true, "{before:#}");

    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "needs_fix_confirmation", "{doc:#}");
    assert_eq!(doc["steps"], json!([]), "nothing launched");
    let next: Vec<String> = doc["stop"]["next"].as_array().unwrap().iter().map(s).collect();
    assert!(next.contains(&format!("thoth verify {edit_run}")), "{next:?}");
    assert!(next.contains(&format!("thoth review confirm {task} --profile P --model M --effort E --reason TEXT")), "{next:?}");
    assert_eq!(doc["stop"]["evidence"]["reviewer_run_id"], edit_run.as_str());
    assert!(s(&doc["stop"]["summary"]).contains(&edit_run), "{doc:#}");
    // the fix is left exactly as it was
    assert_eq!(ok(&f, &["review", "show", &task, "--json"], &[]), before);
}

/// RFIX-2 AC4: a pipeline started on a fresh candidate (no pending fix)
/// whose fake codex reviewer hands in `document` (E001 is the implementer's
/// first evidence, the only code a plan written up front can cite); the
/// stopped show document and the review_start step's pipeline_step event.
fn rfix2_pipeline_review(document: Value) -> (Fixture, String, Value, Value) {
    let f = setup();
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    commit_files(&f, &[("src/greet.rs", &lines)], "greet");
    let task = create(&f);
    let plan = plan_file(&f, &json!({ "document": document }));
    let mut env = worker_env(&task, "hooks", "review_fix");
    env.push(("FAKE_FIX_PLAN", plan.as_str()));
    start(&f, &task, &[], &env);
    let doc = wait_stopped(&f, &task);
    let codes: Vec<String> =
        ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"].as_array().unwrap().iter().map(|e| s(&e["code"])).collect();
    assert!(codes.contains(&"E001".to_string()), "{codes:?}");
    let step = step_event(&f, &task, "review_start");
    (f, task, doc, step)
}

/// The review_start step as `pipeline show` and its event report it: no
/// mode flag in the argv, and `mode` edit in the result.
fn assert_default_edit_step(doc: &Value, step: &Value) {
    let argv: Vec<String> = step["argv"].as_array().unwrap().iter().map(s).collect();
    assert_eq!(argv[..2], ["review", "start"], "{argv:?}");
    assert!(!argv.iter().any(|a| a == "--edit" || a == "--no-edit"), "{argv:?}");
    let shown = steps(doc, "review_start");
    assert_eq!(shown.len(), 1, "{doc:#}");
    assert_eq!(shown[0]["ok"], true, "{:#}", shown[0]);
    assert_eq!(shown[0]["result"]["mode"], "edit", "{:#}", shown[0]);
    assert_eq!(step["result"]["mode"], "edit", "{step:#}");
}

/// RFIX-2 AC4: the pipeline's review start adds no mode flag, so a code
/// task's review is in edit mode: an applied fix stops at
/// needs_fix_confirmation, a PASS with no edits at pass on the candidate.
#[test]
fn rfix2_the_pipeline_review_takes_the_code_task_default_edit_mode() {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let fix = json!({
                "verdict": "REJECT",
                "findings": {
                    "criteria": [{"id": "AC1", "verdict": "unmet", "evidence": ["E001"], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"}],
                    "extra_findings": [],
                },
                "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}],
            });
            let (f, task, doc, step) = rfix2_pipeline_review(fix);
            assert_eq!(stop_reason(&doc), "needs_fix_confirmation", "{doc:#}\n{}", log_of(&doc));
            assert_default_edit_step(&doc, &step);
            let review = ok(&f, &["review", "show", &task, "--json"], &[]);
            assert_eq!(review["pending_fix"], true, "{review:#}");
            assert_eq!(review["mode"], "edit", "{review:#}");
        });
        scope.spawn(|| {
            let pass = json!({
                "verdict": "PASS",
                "findings": {"criteria": [{"id": "AC1", "verdict": "met", "evidence": ["E001"]}], "extra_findings": []},
                "edits": [],
            });
            let (f, task, doc, step) = rfix2_pipeline_review(pass);
            assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
            assert_default_edit_step(&doc, &step);
            assert_eq!(doc["stop"]["evidence"]["merge_branch"], format!("thoth/{task}/a1"), "{doc:#}");
            assert_eq!(ok(&f, &["review", "show", &task, "--json"], &[])["mode"], "edit");
        });
        scope.spawn(|| {
            // a document task is still refused, before anything is written
            let f = setup();
            let spec = Fixture::SPEC.replacen("\"risk\":\"low\"", "\"risk\":\"low\",\"task_type\":\"docs\"", 1);
            let task = create_in(&f, &f.repo_path(), &spec);
            let out = exec(&f, &start_args(&task, &[]), &worker_env(&task, "hooks", "review_pass"));
            assert_eq!(out.code, 1, "{}", out.stdout);
            assert!(out.error().contains("document task"), "{}", out.stderr);
            assert!(of_type(&events(&f, &task), "pipeline_started").is_empty());
        });
    });
}

// --- AC9: show --wait, stop, status --------------------------------------------------------

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn show_wait_times_out_with_the_same_shape_and_stop_never_cancels_the_worker() {
    let f = setup();
    let task = create(&f);
    let (gate, started) = gated_start(&f, &task, &[]);
    wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let o = exec(&f, &["pipeline", "show", &task, "--wait", "1s", "--json"], &[]);
    assert_eq!(o.code, 3, "{}", o.stderr);
    let waited = o.json();
    assert_eq!(waited["status"], "running");
    assert_eq!(keys(&waited), keys(&show(&f, &task)));
    for key in ["ok", "pipeline_id", "task_id", "task_status", "status", "step", "stale", "stop_requested", "settings", "implementer_index", "reviewer_index", "pipeline_pid", "heartbeat_at", "started_at", "ended_at", "dir", "steps", "stop"] {
        assert!(waited.get(key).is_some(), "{key}");
    }
    let human = String::from_utf8(cmd(&f).args(["pipeline", "show", &task]).output().unwrap().stdout).unwrap();
    assert!(human.lines().next().unwrap().contains("is running: wait_run"), "{human}");

    let stopped = ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    assert_eq!(stopped["pipeline_id"], started["pipeline_id"]);
    assert_eq!(stopped["status"], "running");
    let ev = events(&f, &task);
    let req = of_type(&ev, "pipeline_stop_requested");
    assert_eq!(req.len(), 1);
    assert_eq!(req[0]["detail"], json!({"pipeline_id": started["pipeline_id"]}));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "stopped", "{doc:#}");
    assert_eq!(doc["stop_requested"], true);
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    assert_eq!(doc["stop"]["evidence"]["run_id"], run.as_str());
    let human = String::from_utf8(cmd(&f).args(["pipeline", "show", &task]).output().unwrap().stdout).unwrap();
    assert!(human.lines().next().unwrap().contains("stopped: stopped"), "{human}");
    // the worker was not cancelled
    assert!(of_type(&events(&f, &task), "cancel_requested").is_empty());
    std::fs::write(&gate, "go").unwrap();
    assert_eq!(f.wait_for_task_status(&task, &["in_review", "cancelled"], 120), "in_review");
    assert_eq!(run_row(&f, &task, &run)["status"], "completed");
    // nothing left to stop
    let o = exec(&f, &["pipeline", "stop", &task, "--json"], &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("no running pipeline"), "{}", o.stderr);
    let o = exec(&f, &["pipeline", "stop", &task, "--json"], &[("THOTH_RUN_ID", "01FAKERUN")]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("THOTH_RUN_ID"), "{}", o.stderr);
}

#[test]
fn stop_during_verify_ends_only_verify_and_frees_its_slot() {
    let f = setup();
    add_check_in(&f, &f.repo_path(), "gated", "gate F");
    approve_in(&f, &f.repo_path());
    // an unrelated run that must survive
    let other = create(&f);
    let gate2 = f.home_path().join("gate-unrelated");
    let gate2_s = gate2.display().to_string();
    let d = ok(
        &f,
        &["dispatch", &other, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_TASK_ID", other.as_str()), ("FAKE_CLAUDE_SCENARIO", "hooks"), ("FAKE_WORKER_GATE", gate2_s.as_str())],
    );
    let unrelated = s(&d["run_id"]);
    let task = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_pass"));
    wait_show(&f, &task, "F in verify", |v| v["step"] == "verify" && !gate_log(&f, "F").0.is_empty());
    assert_eq!(slot_files(&f).len(), 1);
    let check_pid = gate_log(&f, "F").0[0].1;
    ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "stopped", "{doc:#}");
    assert_eq!(doc["step"], "verify");
    assert!(slot_files(&f).is_empty(), "the slot was freed");
    assert!(gate_log(&f, "F").1.is_empty(), "the check was ended, not let finish");
    let deadline = Instant::now() + Duration::from_secs(20);
    while common::pid_alive(check_pid, FAKE_CHECK) {
        assert!(Instant::now() < deadline, "the check process {check_pid} survived");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(steps(&doc, "review_start").is_empty());
    assert_eq!(status(&f, &task)["status"], "in_review");
    // the unrelated run is still going, and finishes
    assert_eq!(run_row(&f, &other, &unrelated)["status"], "running");
    std::fs::write(&gate2, "go").unwrap();
    assert_eq!(f.wait_for_task_status(&other, &["in_review", "unverified", "failed"], 120), "in_review");
}

#[test]
fn a_stale_pipeline_is_settled_by_stop_and_no_longer_owns_its_task() {
    let f = setup();
    let task = create(&f);
    let (gate, started) = gated_start(&f, &task, &[]);
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    common::kill_pid_only(at["pipeline_pid"].as_u64().unwrap() as u32);
    wait_show(&f, &task, "stale", |v| v["stale"] == true);
    // stale ownership does not hold a command back: dispatch reaches its own check
    let o = exec(&f, &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"], &[]);
    assert_eq!(o.code, 1);
    assert!(!o.error().contains("being run by pipeline"), "{}", o.stderr);
    let stopped = ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["pipeline_id"], started["pipeline_id"]);
    let ev = events(&f, &task);
    assert!(of_type(&ev, "pipeline_stop_requested").is_empty());
    let settled = of_type(&ev, "pipeline_stopped");
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0]["detail"]["stop_reason"], "stale");
    let doc = show(&f, &task);
    assert_eq!(doc["status"], "stopped");
    assert_eq!(doc["stale"], false);
    std::fs::write(&gate, "go").unwrap();
    f.wait_for_task_status(&task, &["in_review"], 120);
}

#[test]
fn status_lists_each_tasks_pipeline_inside_and_outside_a_repository() {
    let f = setup();
    let task = create(&f);
    let quiet = create(&f);
    start(&f, &task, &[], &worker_env(&task, "hooks", "review_reject"));
    let doc = wait_stopped(&f, &task);
    let listed = ok(&f, &["status", "--json"], &[]);
    let row = |v: &Value, t: &str| v["tasks"].as_array().unwrap().iter().find(|r| r["task_id"] == t).cloned().unwrap();
    assert_eq!(
        row(&listed, &task)["pipeline"],
        // AUTO-1b: a REJECT with one implementer entry ends implementers_exhausted
        json!({"pipeline_id": doc["pipeline_id"], "status": "stopped", "step": doc["step"], "stop_reason": "implementers_exhausted"})
    );
    assert_eq!(row(&listed, &quiet)["pipeline"], Value::Null);
    let outside = tempfile::tempdir().unwrap();
    let there = ok_in(&f, outside.path(), &["status", "--json"], &[]);
    assert_eq!(there["outside_repo"], true);
    assert_eq!(row(&there, &task)["pipeline"], row(&listed, &task)["pipeline"]);
    assert_eq!(row(&there, &quiet)["pipeline"], Value::Null);
    let one = status(&f, &task)["pipeline"].clone();
    // AUTO-1b (AC9) adds the two list positions and attempts_used
    assert_eq!(
        keys(&one),
        [
            "attempts_used",
            "ended_at",
            "implementer_index",
            "pipeline_id",
            "reviewer_index",
            "stale",
            "started_at",
            "status",
            "step",
            "stop_reason"
        ]
    );
    assert_eq!(one["started_at"], doc["started_at"]);
    assert_eq!(one["ended_at"], doc["ended_at"]);
    assert_eq!(one["implementer_index"], doc["implementer_index"]);
    assert_eq!(one["reviewer_index"], doc["reviewer_index"]);
    assert_eq!(one["attempts_used"], doc["attempts_used"]);
}

// --- AC10: the ownership gate ------------------------------------------------------------

#[test]
fn a_live_pipeline_owns_its_task_against_every_conflicting_command() {
    let f = setup();
    let task = create(&f);
    let (gate, started) = gated_start(&f, &task, &[]);
    let id = s(&started["pipeline_id"]);
    let at = wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let run = s(&steps(&at, "dispatch")[0]["run_id"]);
    let review_doc = plan_file(&f, &json!({"verdict": "PASS", "findings": {"criteria": [], "extra_findings": []}}));
    let spec_file = plan_file(&f, &serde_json::from_str::<Value>(Fixture::SPEC).unwrap());
    let result_file = plan_file(&f, &json!({"task_id": task, "status": "completed", "summary": "x", "claims": []}));
    // the gated run is running; heartbeats keep moving, so the status is
    // compared on what a command could change
    let deadline = Instant::now() + Duration::from_secs(60);
    // and parked at its gate: the fake waits there once three of its
    // commands are in the evidence
    while run_row(&f, &task, &run)["status"] != "running"
        || ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"].as_array().unwrap().len() < 3
    {
        assert!(Instant::now() < deadline, "the run never reached its gate");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(500));
    let stable = |v: Value| {
        json!({
            "status": v["status"],
            "attempt": v["attempt"],
            "latest_verdict": v["latest_verdict"],
            "review_count": v["review_count"],
            "pipeline": [v["pipeline"]["pipeline_id"].clone(), v["pipeline"]["status"].clone(), v["pipeline"]["step"].clone()],
            "runs": v["runs"].as_array().unwrap().iter().map(|r| json!([r["run_id"], r["role"], r["status"], r["attempt"], r["cancel_requested"]])).collect::<Vec<_>>(),
        })
    };
    let snap = |f: &Fixture| {
        (
            stable(status(f, &task)),
            events(f, &task),
            ok(f, &["evidence", "list", &task, "--json"], &[]),
        )
    };
    let before = snap(&f);
    let conflicting: Vec<Vec<&str>> = vec![
        vec!["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", "r"],
        vec!["review", "start", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--reason", "r"],
        vec!["review", "confirm", &task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--reason", "r"],
        vec!["review", "record", &task, "-f", &review_doc],
        vec!["resume", &run],
        vec!["task", "update-spec", &task, "-f", &spec_file],
        vec!["task", "close", &task, "--reason", "r"],
        vec!["spec", "review", "confirm", &task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--reason", "r"],
        vec!["submit", "-f", &result_file, "--run", &run],
    ];
    for args in &conflicting {
        let mut a = args.clone();
        a.push("--json");
        let o = exec(&f, &a, &[]);
        assert_eq!(o.code, 1, "{a:?}: {}", o.stdout);
        let err = o.error();
        for part in [id.as_str(), "(step wait_run)", &format!("thoth pipeline stop {task}")] {
            assert!(err.contains(part), "{a:?}: {err}");
        }
    }
    // a worker that inherited the pipeline id is still refused
    let o = exec(&f, &["task", "close", &task, "--reason", "r", "--json"], &[("THOTH_PIPELINE_ID", id.as_str()), ("THOTH_RUN_ID", run.as_str())]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("being run by pipeline"), "{}", o.stderr);
    // the pipeline's own child passes the gate (and meets dispatch's own rule)
    let o = exec(&f, &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"], &[("THOTH_PIPELINE_ID", id.as_str())]);
    assert_eq!(o.code, 1);
    let err = o.error();
    assert!(!err.contains("being run by pipeline") && err.contains("'running'"), "{err}");
    // what stays available
    ok(&f, &["checks", "approve", "--reason", "still allowed", "--json"], &[]);
    let o = exec(&f, &["verify", &run, "--json"], &[]);
    assert!(!o.stderr.contains("being run by pipeline"), "{}", o.stderr);
    ok(&f, &["review", "show", &task, "--json"], &[]);
    let after = snap(&f);
    assert_eq!(after.0, before.0, "status changed");
    assert_eq!(after.2, before.2, "evidence changed");
    let new_events: Vec<&Value> = after.1.iter().filter(|e| !before.1.contains(e)).collect();
    assert!(
        new_events
            .iter()
            .all(|e| e["type"] == "check_approved" || e["type"] == "protected_change_approved" || e["run_id"] == run.as_str()),
        "only the approval and the worker's own events: {new_events:?}"
    );
    // MCP hands the refusal on as a tool error
    let mut server = McpServer::start(&f, &[]);
    let r = server.call(1, "thoth_task_close", json!({"task": task, "reason": "r"}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["structuredContent"]["error"].as_str().unwrap().contains("thoth pipeline stop"), "{r}");
    drop(server);
    // after a stop the gate is gone
    ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    wait_stopped(&f, &task);
    let o = exec(&f, &["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"], &[]);
    assert_eq!(o.code, 1);
    assert!(!o.error().contains("being run by pipeline"), "{}", o.stderr);
    std::fs::write(&gate, "go").unwrap();
    f.wait_for_task_status(&task, &["in_review"], 120);
}

// --- AC11: MCP ---------------------------------------------------------------------------

struct McpServer {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl McpServer {
    fn start(f: &Fixture, envs: &[(&str, &str)]) -> Self {
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
        for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME", "CLAUDE_PLUGIN_ROOT", "THOTH_PIPELINE_ID"] {
            c.env_remove(var);
        }
        c.env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .env("FAKE_CHECK_DIR", check_dir(f));
        for (k, v) in envs {
            c.env(k, v);
        }
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
fn the_three_pipeline_tools_start_show_and_stop_through_mcp() {
    let f = setup();
    let task = create(&f);
    let gate = f.home_path().join("gate-mcp");
    let gate_s = gate.display().to_string();
    let mut env = worker_env(&task, "hooks", "review_pass");
    env.push(("FAKE_WORKER_GATE", &gate_s));
    let mut server = McpServer::start(&f, &env);
    let list = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let tool = |name: &str| tools.iter().find(|t| t["name"] == name).cloned().unwrap_or_else(|| panic!("no {name}"));
    assert_eq!(tool("thoth_pipeline_start")["annotations"]["readOnlyHint"], false);
    assert_eq!(tool("thoth_pipeline_stop")["annotations"]["readOnlyHint"], false);
    assert_eq!(tool("thoth_pipeline_show")["annotations"]["readOnlyHint"], true);
    let start_schema = &tool("thoth_pipeline_start")["inputSchema"];
    assert_eq!(start_schema["properties"]["implementers"], json!({"type": "array", "items": {"type": "string"}}));
    let mut required: Vec<String> = start_schema["required"].as_array().unwrap().iter().map(s).collect();
    required.sort();
    // LIGHT-1: both lists are optional; a list left out uses the stored default
    assert_eq!(required, ["reason", "task"]);
    assert_eq!(tools.last().unwrap()["name"], "thoth_guide");
    assert!(!tools.iter().any(|t| s(&t["name"]).contains("pipeline_run")));
    assert!(serde_json::to_vec(&list["result"]["tools"]).unwrap().len() <= 20_000);

    // invalid arguments fail before any work
    let r = server.call(2, "thoth_pipeline_start", json!({"task": task, "implementers": [], "reviewers": [REVIEW], "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    let r = server.call(3, "thoth_pipeline_start", json!({"task": task, "implementers": [IMPL], "reviewers": [REVIEW], "reason": REASON, "verify_wait": "soon"}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(exec(&f, &["pipeline", "show", &task, "--json"], &[]).code, 1, "nothing was started");

    let r = server.call(
        4,
        "thoth_pipeline_start",
        json!({"task": task, "implementers": [IMPL, "claude-a:haiku"], "reviewers": [REVIEW], "reason": REASON, "max_attempts": 2, "verify_timeout": "90m"}),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    let started = r["result"]["structuredContent"].clone();
    assert_eq!(started["settings"]["implementers"][1], json!({"profile": "claude-a", "model": "haiku", "effort": null}));
    assert_eq!(started["settings"]["verify_timeout_seconds"], 5400);
    // AUTO-1b: every entry is used now, so the only-the-first-entry warning is gone
    assert_eq!(started["warnings"], json!([]));
    wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let r = server.call(5, "thoth_pipeline_show", json!({"task": task, "wait_seconds": 1}));
    assert_eq!(r["result"]["isError"], false, "exit 3 is a poll, not an error: {r}");
    assert_eq!(r["result"]["structuredContent"]["status"], "running");
    let r = server.call(6, "thoth_pipeline_show", json!({"task": task, "wait_seconds": 99}));
    assert_eq!(r["result"]["isError"], true, "bounded by the server maximum: {r}");
    let r = server.call(7, "thoth_pipeline_stop", json!({"task": task}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["pipeline_id"], started["pipeline_id"]);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "stopped");
    let cli = show(&f, &task);
    let r = server.call(8, "thoth_pipeline_show", json!({"task": task}));
    assert_eq!(r["result"]["structuredContent"], cli);
    std::fs::write(&gate, "go").unwrap();
    f.wait_for_task_status(&task, &["in_review"], 120);
}

// --- masking ---------------------------------------------------------------------------

#[test]
fn a_secret_in_the_reason_never_reaches_show_events_or_the_pipeline_files() {
    let f = setup();
    let task = create(&f);
    let secret = ["sk-", "ant-", "api03-", "Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2"].concat();
    let reason = format!("chosen for speed {secret}");
    let args = ["pipeline", "start", task.as_str(), "--implementer", IMPL, "--reviewer", REVIEW, "--reason", reason.as_str(), "--json"];
    let env = worker_env(&task, "hooks", "review_pass");
    let o = exec(&f, &args, &env);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(!o.stdout.contains(&secret));
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}");
    let shown = exec(&f, &["pipeline", "show", &task, "--json"], &[]);
    assert!(!shown.stdout.contains(&secret), "{}", shown.stdout);
    let human = exec(&f, &["pipeline", "show", &task], &[]);
    assert!(!human.stdout.contains(&secret));
    let ev = exec(&f, &["events", "--since", "0", "--task", &task, "--json"], &[]);
    assert!(!ev.stdout.contains(&secret));
    for e in std::fs::read_dir(s(&doc["dir"])).unwrap().flatten() {
        let body = std::fs::read_to_string(e.path()).unwrap_or_default();
        assert!(!body.contains(&secret), "{:?}", e.path());
    }
}
