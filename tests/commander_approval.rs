//! LESS-5: the commander approves checks and protected test changes with a
//! reason; review start and a PASS review record wait for those approvals.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

// --- an MCP server, as in tests/mcp_jobs.rs ----------------------------------

struct Server {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(f: &Fixture) -> Self {
        Self::start_with_env(f, &[])
    }

    /// The server with `envs` set in its own environment (after the removals).
    fn start_with_env(f: &Fixture, envs: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("FAKE_SCENARIO", "ok")
            .env_remove("FAKE_TASK_ID")
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("THOTH_VIA_MCP")
            .env_remove("CLAUDE_PLUGIN_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output, next_id: 1 }
    }

    /// One request; returns the whole response.
    fn send(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }

    /// One tools/call; returns the `result` member (panics on a protocol error).
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let v = self.send("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(v.get("error").is_none(), "{v}");
        v["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- helpers -------------------------------------------------------------------

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

    /// The `error` of a `--json` refusal (the last stderr line), or all of stderr.
    fn error(&self) -> String {
        self.stderr
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| self.stderr.clone())
    }
}

fn run(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
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

fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let o = run(f, args, envs);
    assert_eq!(o.code, 0, "{args:?}\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    o
}

fn add_check(f: &Fixture, id: &str) {
    ok(f, &["checks", "add", id, "--cmd", "git --version", "--json"], &[]);
}

fn check_status(f: &Fixture, id: &str) -> String {
    let l = ok(f, &["checks", "list", "--json"], &[]).json();
    l["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check_id"] == id)
        .unwrap_or_else(|| panic!("no check {id}: {l}"))["status"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Every event of `type`, from `events --since 0` (check approvals name no task).
fn events_of_type(f: &Fixture, ty: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--json"], &[]).json()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

fn task_events_of_type(f: &Fixture, task: &str, ty: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[]).json()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

fn evidence(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["evidence", "list", task, "--json"], &[]).json()["evidence"]
        .as_array()
        .unwrap()
        .clone()
}

fn evidence_of_kind(f: &Fixture, task: &str, kind: &str) -> Vec<Value> {
    evidence(f, task).into_iter().filter(|e| e["kind"] == kind).collect()
}

fn reviewer_runs(f: &Fixture, task: &str) -> usize {
    ok(f, &["status", task, "--json"], &[]).json()["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["role"] == "reviewer")
        .count()
}

fn packet_json(f: &Fixture, task: &str) -> Value {
    ok(f, &["review", "packet", task, "--json"], &[]).json()
}

fn packet_md(f: &Fixture, task: &str) -> String {
    ok(f, &["review", "packet", task], &[]).stdout
}

/// The packet's Approvals section, checked to sit between the worker claims
/// and the reviews.
fn approvals_section(md: &str) -> String {
    let claims = md.find("## Worker claims").expect("claims section");
    let at = md.find("## Approvals during this task").expect("approvals section");
    let reviews = md.find("## Reviews recorded so far").expect("reviews section");
    assert!(claims < at && at < reviews, "{md}");
    md[at..reviews].to_string()
}

fn write_file(f: &Fixture, name: &str, body: &str) -> String {
    let p = f.home_path().join(name);
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

fn review_doc(f: &Fixture, verdict: &str, criterion: &str, code: &str) -> String {
    let c = if criterion == "met" {
        json!({"id": "AC1", "verdict": "met", "evidence": [code]})
    } else {
        json!({"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "not shown"})
    };
    let doc = json!({"verdict": verdict, "findings": {"criteria": [c], "extra_findings": []}});
    write_file(f, &format!("review-{verdict}.json"), &doc.to_string())
}

const PROTECTED_CLAIM: &str = "a.txt: the task asked for the new wording";

/// P: init, two profiles, `a.txt` protected, a task, and a hooks run that
/// modifies and declares `a.txt`, left `in_review`. Returns (task, run).
fn prelude(f: &Fixture, before_task: impl FnOnce(&Fixture)) -> (String, String) {
    ok(f, &["init"], &[]);
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    ok(f, &["checks", "protect", "a.txt"], &[]);
    before_task(f);
    let (task, _) = f.create_task(Fixture::SPEC);
    let run = dispatch_writing_a(f, &task);
    (task, run)
}

fn dispatch_writing_a(f: &Fixture, task: &str) -> String {
    let d = ok(
        f,
        &["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"],
        &[
            ("FAKE_SCENARIO", "hooks"),
            ("FAKE_TASK_ID", task),
            ("FAKE_WRITE_FILE", "a.txt"),
            ("FAKE_PROTECTED_CLAIM", PROTECTED_CLAIM),
        ],
    )
    .json();
    let (_, status) = f.wait_for(task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(task));
    d["run_id"].as_str().unwrap().to_string()
}

const BOGUS_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// t8-t10's start: a seeded run whose worktree commit modifies the protected
/// `a.txt`, completed (setup only, as tests/antigravity.rs does), then brought
/// to `in_review` by `thoth submit`, which records no diff evidence.
/// Returns (task, run, run's base_commit).
fn submitted_run(f: &Fixture) -> (String, String, String) {
    ok(f, &["init"], &[]);
    f.add_profile("claude-b");
    ok(f, &["checks", "protect", "a.txt"], &[]);
    let (task, run, wt) = f.seed_run("claude-b", Fixture::SPEC);
    std::fs::write(wt.join("a.txt"), "changed by the run\n").unwrap();
    common::git(&wt, &["add", "a.txt"]);
    common::git(&wt, &["commit", "-m", "change a"]);
    let candidate = common::git(&wt, &["rev-parse", "HEAD"]);
    let base: String = f
        .state_conn()
        .query_row("SELECT base_commit FROM runs WHERE run_id = ?1", [&run], |r| r.get(0))
        .unwrap();
    f.state_conn()
        .execute(
            "UPDATE runs SET status='completed', candidate_commit=?1 WHERE run_id=?2",
            rusqlite::params![candidate, run],
        )
        .unwrap();
    let result = json!({
        "task_id": task,
        "status": "completed",
        "summary": "changed a.txt",
        "claims": [
            {"kind": "protected_change", "text": PROTECTED_CLAIM},
            {"kind": "completion", "text": "a.txt says what the task asked"},
        ],
    });
    let file = write_file(f, "result.json", &result.to_string());
    let s = ok(f, &["submit", "-f", &file, "--run", &run, "--json"], &[]).json();
    assert_eq!(s["task_status"], "in_review", "{s}");
    (task, run, base)
}

fn set_run_base(f: &Fixture, run: &str, base: &str) {
    f.state_conn()
        .execute("UPDATE runs SET base_commit=?1 WHERE run_id=?2", [base, run])
        .unwrap();
}

/// The run's newest diff evidence code (largest number).
fn newest_diff(f: &Fixture, task: &str, run: &str) -> String {
    evidence_of_kind(f, task, "diff")
        .into_iter()
        .filter(|e| e["run_id"] == run)
        .map(|e| e["code"].as_str().unwrap().to_string())
        .max_by_key(|c| c.trim_start_matches('E').parse::<u64>().unwrap())
        .expect("a diff evidence")
}

/// Rewrites one diff evidence's `protected_changes` (setup only: no command
/// writes an old-style or damaged diff evidence).
fn set_protected_changes(f: &Fixture, task: &str, code: &str, value: Option<Value>) {
    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM evidence WHERE task_id=?1 AND code=?2",
            [task, code],
            |r| r.get(0),
        )
        .unwrap();
    let mut d: Value = serde_json::from_str(&detail).unwrap();
    let o = d.as_object_mut().unwrap();
    match value {
        None => {
            o.remove("protected_changes");
        }
        Some(v) => {
            o.insert("protected_changes".to_string(), v);
        }
    }
    conn.execute(
        "UPDATE evidence SET detail=?1 WHERE task_id=?2 AND code=?3",
        [d.to_string().as_str(), task, code],
    )
    .unwrap();
}

fn diffs_of(f: &Fixture, task: &str, run: &str) -> usize {
    evidence_of_kind(f, task, "diff").iter().filter(|e| e["run_id"] == run).count()
}

fn review_start(f: &Fixture, task: &str, profile: &str, envs: &[(&str, &str)]) -> Out {
    run(f, &["review", "start", task, "--profile", profile, "--model", "haiku", "--json"], envs)
}

// --- tests ---------------------------------------------------------------------

/// AC1, AC2: without a terminal the reason is required, blank is refused, and
/// who and why are recorded; a run's environment is still refused.
#[test]
fn commander_approval_cli_needs_a_reason_and_records_who() {
    let f = common::fixture();
    ok(&f, &["init"], &[]);
    add_check(&f, "gv");

    let o = run(&f, &["checks", "approve", "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.error().contains("--reason"), "{}", o.stderr);
    assert_eq!(check_status(&f, "gv"), "pending");
    assert!(events_of_type(&f, "check_approved").is_empty());

    let o = run(&f, &["checks", "approve", "--reason", "   ", "--json"], &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("must not be empty"), "{}", o.stderr);

    let v = ok(&f, &["checks", "approve", "--reason", "R1", "--json"], &[]).json();
    assert_eq!(v["approved"][0]["check_id"], "gv");
    assert_eq!(v["approved_by"], "commander_cli");
    assert_eq!(v["reason"], "R1");
    assert_eq!(v["protected_approved"], json!([]));
    assert_eq!(v["protected_unchanged"], json!([]));
    assert_eq!(check_status(&f, "gv"), "approved");
    let ev = events_of_type(&f, "check_approved");
    let d = &ev.last().unwrap()["detail"];
    assert_eq!(d["approved_by"], "commander_cli");
    assert_eq!(d["reason"], "R1");
    assert_eq!(d["live_runs"], json!([]));
    assert!(ev.last().unwrap()["task_id"].is_null());

    add_check(&f, "gv2");
    let before = events_of_type(&f, "check_approved").len();
    let o = run(
        &f,
        &["checks", "approve", "--reason", "x", "--json"],
        &[("THOTH_RUN_ID", "01TESTRUN")],
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("THOTH_RUN_ID"), "{}", o.stderr);
    assert_eq!(check_status(&f, "gv2"), "pending");
    assert_eq!(events_of_type(&f, "check_approved").len(), before);
}

/// AC5: the MCP tool needs a reason and records commander_mcp.
#[test]
fn commander_approval_mcp_tool_requires_a_reason() {
    let f = common::fixture();
    ok(&f, &["init"], &[]);
    let mut s = Server::start(&f);
    let list = s.send("tools/list", json!({}));
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "thoth_checks_approve")
        .expect("thoth_checks_approve is listed")
        .clone();
    assert_eq!(tool["inputSchema"]["required"], json!(["reason"]));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);

    let r = s.call("thoth_checks_add", json!({"id": "gv", "cmd": "git --version"}));
    assert_eq!(r["isError"], false, "{r}");

    let r = s.call("thoth_checks_approve", json!({}));
    assert_eq!(r["isError"], true);
    assert!(r["structuredContent"]["error"].as_str().unwrap().contains("missing parameter reason"), "{r}");

    let r = s.call("thoth_checks_approve", json!({"reason": "  "}));
    assert_eq!(r["isError"], true);
    assert!(r["structuredContent"]["error"].as_str().unwrap().contains("must not be empty"), "{r}");

    let r = s.call("thoth_checks_approve", json!({"reason": "R2"}));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["approved_by"], "commander_mcp");
    assert_eq!(r["structuredContent"]["reason"], "R2");
    assert_eq!(check_status(&f, "gv"), "approved");
    let ev = events_of_type(&f, "check_approved");
    assert_eq!(ev.last().unwrap()["detail"]["approved_by"], "commander_mcp");
}

/// AC3, AC4, AC7: a declared test change blocks review start and a PASS
/// record until `checks approve --run` approves it, once; the packet and the
/// reviewer's prompt show who approved it and why.
#[test]
fn commander_approval_a_declared_test_change_waits_for_the_commander() {
    let f = common::fixture();
    let (task, run_id) = prelude(&f, |f| add_check(f, "gv"));

    let o = review_start(&f, &task, "claude-a", &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains("a.txt (modified, test)"), "{err}");
    assert!(err.contains(&format!("thoth checks approve --run {run_id} --reason")), "{err}");
    assert_eq!(reviewer_runs(&f, &task), 0);
    assert!(task_events_of_type(&f, &task, "review_started").is_empty());

    let pass = review_doc(&f, "PASS", "met", "E001");
    let o = run(&f, &["review", "record", &task, "-f", &pass, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.starts_with(&format!("refusing to record a PASS for task {task}: ")), "{err}");
    assert!(err.contains(&format!("thoth checks approve --run {run_id} --reason")), "{err}");
    assert_eq!(f.task_status(&task), "in_review");

    let v = ok(&f, &["checks", "approve", "--run", &run_id, "--reason", "R3", "--json"], &[]).json();
    assert_eq!(
        v["protected_approved"],
        json!([{"path": "a.txt", "status": "modified", "kind": "test"}])
    );
    assert!(v["approved"].as_array().unwrap().iter().any(|c| c["check_id"] == "gv"), "{v}");
    assert_eq!(v["approved_by"], "commander_cli");
    let again = ok(&f, &["checks", "approve", "--run", &run_id, "--reason", "R3", "--json"], &[]).json();
    assert_eq!(again["protected_approved"], json!([]));
    assert_eq!(again["protected_unchanged"], json!(["a.txt"]));

    let pe = task_events_of_type(&f, &task, "protected_change_approved");
    assert_eq!(pe.len(), 1, "{pe:?}");
    assert_eq!(pe[0]["detail"]["reason"], "R3");
    assert_eq!(pe[0]["detail"]["changes"][0]["path"], "a.txt");
    let ce: Vec<Value> = events_of_type(&f, "check_approved")
        .into_iter()
        .filter(|e| e["detail"]["check_id"] == "gv")
        .collect();
    assert_eq!(ce.len(), 1);
    assert_eq!(ce[0]["run_id"], run_id.as_str());
    assert_eq!(ce[0]["detail"]["reason"], "R3");

    let p = packet_json(&f, &task);
    let a = p["approvals"].as_array().unwrap();
    assert_eq!(a.len(), 2, "{a:?}");
    assert_eq!(a[0]["type"], "check_approved");
    assert_eq!(a[0]["check_id"], "gv");
    assert_eq!(a[0]["source"], "run");
    assert_eq!(a[0]["reason"], "R3");
    assert_eq!(a[1]["type"], "protected_change_approved");
    assert_eq!(a[1]["reason"], "R3");
    let section = approvals_section(&packet_md(&f, &task));
    assert!(section.contains("R3") && section.contains("a.txt"), "{section}");

    let r = f.review_start(&task, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer = r["run_id"].as_str().unwrap().to_string();
    ok(&f, &["wait", "--run", &reviewer, "--timeout", "180s", "--json"], &[]);
    assert_eq!(f.task_status(&task), "pass", "{}", f.explain_task(&task));
    let prompt =
        std::fs::read_to_string(Path::new(r["run_dir"].as_str().unwrap()).join("prompt.txt")).unwrap();
    let line = prompt
        .lines()
        .find(|l| l.contains("a.txt (modified, test)"))
        .unwrap_or_else(|| panic!("{prompt}"));
    assert!(line.contains("approved by commander_cli: R3"), "{line}");
}

/// AC4: a REJECT needs no approval.
#[test]
fn commander_approval_reject_needs_no_approval() {
    let f = common::fixture();
    let (task, _) = prelude(&f, |_| {});
    let reject = review_doc(&f, "REJECT", "unmet", "E001");
    ok(&f, &["review", "record", &task, "-f", &reject, "--json"], &[]);
    assert_eq!(f.task_status(&task), "reject");
}

/// AC7: the packet lists check approvals made after the task was created only.
#[test]
fn commander_approval_packet_lists_check_approvals_since_the_task_was_created() {
    let f = common::fixture();
    ok(&f, &["init"], &[]);
    add_check(&f, "c1");
    ok(&f, &["checks", "approve", "--reason", "reason-pre-7f", "--json"], &[]);
    let (task, _) = f.create_task(Fixture::SPEC);
    add_check(&f, "c2");
    ok(&f, &["checks", "approve", "--reason", "reason-post-7f", "--json"], &[]);

    let a = packet_json(&f, &task)["approvals"].clone();
    let a = a.as_array().unwrap();
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0]["type"], "check_approved");
    assert_eq!(a[0]["check_id"], "c2");
    assert_eq!(a[0]["source"], "main");
    assert_eq!(a[0]["reason"], "reason-post-7f");
    let md = packet_md(&f, &task);
    assert!(md.contains("reason-post-7f"), "{md}");
    assert!(!md.contains("reason-pre-7f"), "{md}");
}

/// AC13: a live run of another task does not block `--run`, one of the same
/// task does; every approval records the live runs.
#[test]
fn commander_approval_live_runs_block_only_their_own_task() {
    let f = common::fixture();
    ok(&f, &["init"], &[]);
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    add_check(&f, "gv");
    let (task_a, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task_a, "claude-b", "hooks", &[]);
    let run_a = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_a, "180s").1, "in_review");

    let gate = f.home_path().join("gate");
    let gate_s = gate.display().to_string();
    let mut spec_runs = Vec::new();
    for _ in 0..2 {
        let (t, _) = f.create_task(Fixture::SPEC);
        let r = ok(
            &f,
            &["spec", "review", "start", &t, "--profile", "claude-b", "--model", "haiku", "--json"],
            &[
                ("FAKE_SCENARIO", "spec_review_approve"),
                ("FAKE_TASK_ID", &t),
                ("FAKE_REVIEW_GATE", &gate_s),
            ],
        )
        .json();
        spec_runs.push(r["run_id"].as_str().unwrap().to_string());
    }
    let mut live = spec_runs.clone();
    live.sort();

    ok(&f, &["checks", "approve", "--run", &run_a, "--reason", "x", "--json"], &[]);
    let ev = events_of_type(&f, "check_approved");
    assert_eq!(ev.last().unwrap()["detail"]["live_runs"], json!(live));
    add_check(&f, "gv2");
    ok(&f, &["checks", "approve", "--reason", "y", "--json"], &[]);
    let ev = events_of_type(&f, "check_approved");
    assert_eq!(ev.last().unwrap()["detail"]["live_runs"], json!(live));

    let r = ok(
        &f,
        &["review", "start", &task_a, "--profile", "claude-a", "--model", "haiku", "--json"],
        &[
            ("FAKE_SCENARIO", "review_pass"),
            ("FAKE_TASK_ID", &task_a),
            ("FAKE_REVIEW_GATE", &gate_s),
        ],
    )
    .json();
    let run_a2 = r["run_id"].as_str().unwrap().to_string();

    add_check(&f, "gv3");
    let before = events_of_type(&f, "check_approved").len();
    let o = run(&f, &["checks", "approve", "--run", &run_a, "--reason", "z", "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains(&run_a2) && err.contains("live run"), "{err}");
    assert_eq!(check_status(&f, "gv3"), "pending");
    assert_eq!(events_of_type(&f, "check_approved").len(), before);

    let a = packet_json(&f, &task_a)["approvals"].clone();
    let a = a.as_array().unwrap();
    assert_eq!(a.len(), 2, "{a:?}");
    for item in a {
        assert_eq!(item["live_runs"], json!(live));
    }
    let section = approvals_section(&packet_md(&f, &task_a));
    let note = format!("(while runs {}, {} were running)", live[0], live[1]);
    assert_eq!(section.matches(&note).count(), 2, "{section}");

    std::fs::write(&gate, b"").unwrap();
    for r in [&run_a2, &spec_runs[0], &spec_runs[1]] {
        ok(&f, &["wait", "--run", r, "--timeout", "180s", "--json"], &[]);
    }
}

/// AC3, AC4: an approval belongs to its run; the rework run with the same
/// change waits again.
#[test]
fn commander_approval_binds_to_the_run() {
    let f = common::fixture();
    let (task, run1) = prelude(&f, |_| {});
    ok(&f, &["checks", "approve", "--run", &run1, "--reason", "X", "--json"], &[]);
    let reject = review_doc(&f, "REJECT", "unmet", "E001");
    ok(&f, &["review", "record", &task, "-f", &reject, "--json"], &[]);
    assert_eq!(f.task_status(&task), "reject");

    let run2 = dispatch_writing_a(&f, &task);
    let o = review_start(&f, &task, "claude-a", &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains(&run2) && err.contains("a.txt (modified, test)"), "{err}");
    assert!(!err.contains(&run1), "{err}");
}

/// AC4 (a): a run without diff evidence gets one recorded before it is judged.
#[test]
fn commander_approval_records_a_missing_diff_before_judging() {
    let f = common::fixture();
    let (task, run_id, _) = submitted_run(&f);
    assert!(evidence_of_kind(&f, &task, "diff").is_empty());

    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains("a.txt (modified, test)") && err.contains(&run_id), "{err}");
    let diffs = evidence_of_kind(&f, &task, "diff");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0]["run_id"], run_id.as_str());
    assert_eq!(reviewer_runs(&f, &task), 0);
}

/// AC4 (a): a diff that cannot be recorded refuses the review; UNVERIFIED is
/// still recorded.
#[test]
fn commander_approval_fails_closed_when_the_diff_cannot_be_recorded() {
    let f = common::fixture();
    let (task, run_id, base) = submitted_run(&f);
    set_run_base(&f, &run_id, BOGUS_SHA);

    for _ in 0..2 {
        let o = review_start(&f, &task, "claude-b", &[]);
        assert_eq!(o.code, 1, "{}", o.stdout);
        let err = o.error();
        assert!(err.contains("has no diff evidence") && err.contains(&run_id), "{err}");
        assert_eq!(reviewer_runs(&f, &task), 0);
        assert_eq!(evidence_of_kind(&f, &task, "commit").len(), 1);
        assert!(evidence_of_kind(&f, &task, "diff").is_empty());
    }

    set_run_base(&f, &run_id, &base);
    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.error().contains("a.txt (modified, test)"), "{}", o.stderr);
    assert_eq!(evidence_of_kind(&f, &task, "diff").len(), 1);
    let commits = evidence_of_kind(&f, &task, "commit");
    assert_eq!(commits.len(), 1);

    let code = commits[0]["code"].as_str().unwrap().to_string();
    let doc = review_doc(&f, "UNVERIFIED", "met", &code);
    ok(&f, &["review", "record", &task, "-f", &doc, "--json"], &[]);
    assert_eq!(f.task_status(&task), "unverified");
}

/// AC3, AC4 (a2): an old-style or damaged diff evidence is "not known", never
/// "nothing changed": approve writes nothing for it, and the gates record the
/// diff again or refuse.
#[test]
fn commander_approval_does_not_read_an_old_or_broken_diff_as_empty() {
    let f = common::fixture();
    let (task, run_id, _) = submitted_run(&f);
    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("a.txt (modified, test)"), "{}", o.stderr);

    let code = newest_diff(&f, &task, &run_id);
    set_protected_changes(&f, &task, &code, None);
    let o = ok(&f, &["checks", "approve", "--run", &run_id, "--reason", "r", "--json"], &[]);
    let v = o.json();
    assert_eq!(v["protected_approved"], json!([]));
    assert_eq!(v["protected_unchanged"], json!([]));
    assert!(task_events_of_type(&f, &task, "protected_change_approved").is_empty());
    let warning = format!(
        "warning: run {run_id}'s diff evidence {code} has no readable protected_changes; run review start, then approve again"
    );
    assert!(o.stderr.contains(&warning), "{}", o.stderr);

    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("a.txt (modified, test)"), "{}", o.stderr);
    assert_eq!(diffs_of(&f, &task, &run_id), 2);

    let newest = newest_diff(&f, &task, &run_id);
    set_protected_changes(&f, &task, &newest, None);
    let commit = evidence_of_kind(&f, &task, "commit")[0]["code"].as_str().unwrap().to_string();
    let pass = review_doc(&f, "PASS", "met", &commit);
    let o = run(&f, &["review", "record", &task, "-f", &pass, "--json"], &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let err = o.error();
    assert!(err.contains("refusing to record a PASS") && err.contains("a.txt"), "{err}");
    assert_eq!(diffs_of(&f, &task, &run_id), 3);

    let newest = newest_diff(&f, &task, &run_id);
    set_protected_changes(&f, &task, &newest, Some(json!("a.txt")));
    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("a.txt (modified, test)"), "{}", o.stderr);
    assert_eq!(diffs_of(&f, &task, &run_id), 4);

    set_run_base(&f, &run_id, BOGUS_SHA);
    let newest = newest_diff(&f, &task, &run_id);
    set_protected_changes(&f, &task, &newest, None);
    let o = review_start(&f, &task, "claude-b", &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("no readable protected_changes"), "{}", o.stderr);
    assert_eq!(diffs_of(&f, &task, &run_id), 4);
    assert_eq!(reviewer_runs(&f, &task), 0);
}

/// A script that exits 1 when THOTH_VIA_MCP is set in its environment, and
/// the check's shell and cmd.
#[cfg(windows)]
const VIA_MCP_PROBE: (&str, &str, &str, &str) = (
    "probe.ps1",
    "if ($env:THOTH_VIA_MCP) { exit 1 }\r\nexit 0\r\n",
    "powershell",
    r".\probe.ps1",
);
#[cfg(not(windows))]
const VIA_MCP_PROBE: (&str, &str, &str, &str) =
    ("probe.sh", "#!/bin/sh\ntest -z \"$THOTH_VIA_MCP\"\n", "sh", "./probe.sh");

/// Commits the probe script the way tests/mcp_jobs.rs commits its slow script.
fn commit_probe_script(f: &Fixture) {
    let repo = f.repo_path();
    let (name, body, _, _) = VIA_MCP_PROBE;
    std::fs::write(repo.join(name), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    common::git(&repo, &["add", name]);
    common::git(&repo, &["commit", "-m", "probe script"]);
    std::fs::remove_file(repo.join(name)).unwrap();
    common::git(&repo, &["checkout", "HEAD", "--", name]);
}

/// The probe check's exit code in a verify document.
fn probe_exit(doc: &Value) -> Value {
    doc["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {doc}"))
        .iter()
        .find(|r| r["check_id"] == "via")
        .unwrap_or_else(|| panic!("no probe result: {doc}"))["exit_code"]
        .clone()
}

/// AC5: only thoth_checks_approve's child carries THOTH_VIA_MCP; a verify job
/// started by a server that has it in its own environment does not pass it on.
#[test]
fn commander_approval_verify_jobs_do_not_carry_the_mcp_marker() {
    let f = common::fixture();
    commit_probe_script(&f);
    ok(&f, &["init"], &[]);
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    let (_, _, shell, cmd) = VIA_MCP_PROBE;
    ok(&f, &["checks", "add", "via", "--shell", shell, "--cmd", cmd, "--json"], &[]);
    ok(&f, &["checks", "approve", "--reason", "probe", "--json"], &[]);

    // The probe sees the marker when verify's environment has it.
    let o = run(&f, &["verify", &run_id, "--json"], &[("THOTH_VIA_MCP", "1")]);
    assert_eq!(probe_exit(&o.json()), 1, "{}", o.stdout);

    let mut s = Server::start_with_env(&f, &[("THOTH_VIA_MCP", "1")]);
    let started = s.call("thoth_verify_start", json!({"run": run_id}));
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let done = loop {
        let r = s.call("thoth_job_status", json!({"job_id": job_id, "wait_seconds": 5}));
        assert_eq!(r["isError"], false, "{r}");
        if r["structuredContent"]["status"] != "working" {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "job {job_id} never finished: {r}");
    };
    assert_eq!(done["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(probe_exit(&done["structuredContent"]["result"]), 0, "{done}");
}
