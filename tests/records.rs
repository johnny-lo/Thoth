//! DT-3: the shared records folder. Every computer writes only its own
//! `NAME.jsonl` there, one line per run, regenerated whole; `thoth models`
//! reads the other computers' files. Always the fakes and a temporary
//! THOTH_HOME; nothing here reaches a real harness or the network.
//!
//! The helpers are copies of tests/launch_choice.rs's, because those files
//! are not modules.

mod common;

use common::Fixture;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// --- copied from tests/launch_choice.rs -----------------------------------------

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

// --- this file's own -----------------------------------------------------------

fn records_dir(f: &Fixture) -> PathBuf {
    let dir = f.home_path().join("records");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn set_records(f: &Fixture, dir: &Path, machine: &str) {
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(dir)
        .arg("--json")
        .assert()
        .success();
    f.cmd()
        .args(["config", "set", "machine-name", machine, "--json"])
        .assert()
        .success();
}

fn read_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Rereads `path` every 250 ms until `pred` holds. The runner exports after it
/// has settled the run, so `wait` can return before the file is written.
fn poll_lines(path: &Path, pred: impl Fn(&[Value]) -> bool, secs: u64) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let lines: Vec<Value> = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if pred(&lines) {
            return lines;
        }
        if Instant::now() >= deadline {
            panic!(
                "{} never reached the wanted shape within {secs}s; it holds:\n{}",
                path.display(),
                fs::read_to_string(path).unwrap_or_else(|e| format!("(unreadable: {e})"))
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn line_of<'a>(lines: &'a [Value], run_id: &str) -> &'a Value {
    lines
        .iter()
        .find(|l| l["run_id"] == run_id)
        .unwrap_or_else(|| panic!("no line for run {run_id} in {lines:?}"))
}

fn insert_task(conn: &Connection, id: &str, title: &str, spec: &str, risk: &str) {
    conn.execute(
        "INSERT INTO tasks(task_id,title,spec,risk,status,base_commit,created_at,updated_at)
         VALUES(?1,?2,?3,?4,'completed','abc','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        params![id, title, spec, risk],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn insert_run(
    conn: &Connection,
    run_id: &str,
    task_id: &str,
    attempt: i64,
    role: &str,
    provider: &str,
    account: &str,
    model: &str,
    spec: &str,
    status: &str,
    started: &str,
    ended: &str,
    input: Option<i64>,
    output: Option<i64>,
) {
    conn.execute(
        "INSERT INTO runs(run_id,task_id,attempt,role,provider,account,model,config_dir,allowed_tools,
                          worktree,base_commit,spec,status,started_at,ended_at,cancel_requested,
                          timeout_seconds,run_dir,input_tokens,output_tokens)
         VALUES(?1,?2,?3,?4,?5,?6,?7,'','','','abc',?8,?9,?10,?11,0,0,'',?12,?13)",
        params![run_id, task_id, attempt, role, provider, account, model, spec, status, started, ended, input, output],
    )
    .unwrap();
}

fn run_dir(f: &Fixture, run_id: &str) -> PathBuf {
    f.home_path().join("repos").join(f.repo_id()).join("runs").join(run_id)
}

/// The local data of tests (g) and (h): one reviewed implementer run.
fn insert_local_history(f: &Fixture) {
    let conn = f.state_conn();
    insert_task(&conn, "t1", "local task", r#"{"task_type":"code","difficulty":"high"}"#, "normal");
    insert_run(
        &conn,
        "r1",
        "t1",
        1,
        "implementer",
        "anthropic",
        "claude-a",
        "sonnet",
        r#"{"launch":{"effort_requested":"high","effort":"high"}}"#,
        "completed",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:10:00Z",
        Some(10),
        Some(20),
    );
    conn.execute(
        "INSERT INTO reviews(review_id,target_run_id,reviewer_run_id,verdict,findings,created_at)
         VALUES('v1','r1',NULL,'PASS','{}','2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
}

/// Every key of a v2 line: v1's list without `recommended`, `deviates` and
/// `exempt` (RJ-3), nothing added.
const LINE_KEYS: [&str; 27] = [
    "v", "machine", "repo_id", "repo", "run_id", "task_id", "task_title", "attempt", "role",
    "profile", "provider", "harness", "model", "effort_requested", "effort", "task_type",
    "difficulty", "risk", "status", "interrupted_reason", "exit_code", "started_at", "ended_at",
    "verdict", "tokens", "quota", "reason",
];

fn keys_of(line: &Value) -> Vec<String> {
    let mut keys: Vec<String> = line.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

fn line_keys() -> Vec<String> {
    let mut keys: Vec<String> = LINE_KEYS.iter().map(|k| k.to_string()).collect();
    keys.sort();
    keys
}

/// A complete line as another computer would have written it.
fn remote_line(run_id: &str, machine: &str, profile: &str) -> Value {
    json!({
        "v": 2, "machine": machine, "repo_id": "deadbeef0000", "repo": "Thoth",
        "run_id": run_id, "task_id": "T-REMOTE", "task_title": "remote task", "attempt": 1,
        "role": "implementer", "profile": profile, "provider": "anthropic",
        "harness": "claude-code", "model": "sonnet",
        "effort_requested": "high", "effort": "high",
        "task_type": "code", "difficulty": "high", "risk": "normal",
        "status": "completed", "interrupted_reason": null, "exit_code": 0,
        "started_at": "2026-01-02T00:00:00Z", "ended_at": "2026-01-02T00:10:00Z",
        "verdict": "REJECT",
        "tokens": {"input": 5, "cache_read": null, "cache_write": null, "output": 6, "reasoning": null, "total": 11},
        "quota": {"five_hour_points": 2.0, "seven_day_points": null, "reliable": true, "unreliable_reasons": [], "source": "claude_stream"},
        "reason": null,
    })
}

/// The same line as a computer whose thoth is not rebuilt yet writes it (v1).
fn remote_line_v1(run_id: &str, machine: &str, profile: &str) -> Value {
    let mut line = remote_line(run_id, machine, profile);
    line["v"] = json!(1);
    line["recommended"] = json!({"profile": "codex-a", "model": "gpt-5.6-luna", "effort_requested": "medium"});
    line["deviates"] = json!(true);
    line["exempt"] = json!(null);
    line["reason"] = json!("codex is busy");
    line
}

fn write_jsonl(path: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(path, text).unwrap();
}

// (a) the two settings ------------------------------------------------------------

#[test]
fn the_two_settings_are_listed_validated_and_overridden_by_env() {
    let f = init();
    let list = json_out(f.cmd().args(["config", "list", "--json"]));
    let settings = list["settings"].as_array().unwrap();
    // RJ-2 added `arena-url` after `aa-url`: 16 settings, the two records
    // settings one place later.
    assert_eq!(settings.len(), 16, "{list}");
    assert_eq!(settings[11]["key"], "machine-name");
    assert_eq!(settings[11]["kind"], "text");
    assert_eq!(settings[11]["env"], "THOTH_MACHINE_NAME");
    assert_eq!(settings[11]["settable"], true);
    assert_eq!(settings[11]["source"], "default");
    let shown = settings[11]["value"].as_str().unwrap();
    assert!(!shown.is_empty());
    assert!(
        shown.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "{shown}"
    );
    assert_eq!(settings[12]["key"], "records-dir");
    assert_eq!(settings[12]["env"], "THOTH_RECORDS_DIR");
    assert_eq!(settings[12]["value"], "unset");
    assert_eq!(settings[12]["default"], "unset");
    assert_eq!(settings[12]["source"], "default");
    assert_eq!(settings[13]["key"], "home");
    assert_eq!(settings[3]["key"], "language");

    let out = f
        .cmd()
        .args(["config", "set", "machine-name", "bad name"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("machine-name"));
    assert!(!f.home_path().join("config.json").exists());

    let out = f
        .cmd()
        .args(["config", "set", "machine-name", ".hidden"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));

    let v = json_out(f.cmd().args(["config", "set", "machine-name", "host-1.local", "--json"]));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["value"], "host-1.local", "{v}");
    assert_eq!(v["previous"], Value::Null, "{v}");
    let v = json_out(f.cmd().args(["config", "get", "machine-name", "--json"]));
    assert_eq!(v, json!({"key": "machine-name", "value": "host-1.local"}));

    let v = json_out(
        f.cmd()
            .env("THOTH_MACHINE_NAME", "env box")
            .args(["config", "get", "machine-name", "--json"]),
    );
    assert_eq!(v["value"], "env box", "{v}");
    let list = json_out(
        f.cmd()
            .env("THOTH_MACHINE_NAME", "env box")
            .args(["config", "list", "--json"]),
    );
    assert_eq!(list["settings"][10]["source"], "env", "{list}");

    let out = f
        .cmd()
        .args(["config", "set", "records-dir", "relative/dir"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("absolute"));

    let dir = records_dir(&f);
    let dir_text = dir.to_string_lossy().to_string();
    let v = json_out(f.cmd().args(["config", "set", "records-dir"]).arg(&dir).arg("--json"));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["value"], dir_text.as_str(), "{v}");
    let v = json_out(f.cmd().args(["config", "get", "records-dir", "--json"]));
    assert_eq!(v["value"], dir_text.as_str(), "{v}");
    let v = json_out(f.cmd().args(["config", "unset", "records-dir", "--json"]));
    assert_eq!(v["value"], "unset", "{v}");
    assert_eq!(v["source"], "default", "{v}");
}

// (b) the refusals ----------------------------------------------------------------

#[test]
fn export_refuses_when_unset_missing_or_inside_a_run() {
    let f = init();
    let out = f.cmd().args(["records", "export"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("records-dir is not set"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let nowhere = f.home_path().join("nowhere");
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(&nowhere)
        .assert()
        .success();
    let out = f.cmd().args(["records", "export", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: Value = serde_json::from_str(String::from_utf8_lossy(&out.stderr).trim()).unwrap();
    assert_eq!(err["ok"], false, "{err}");
    assert!(err["error"].as_str().unwrap().contains("is not a directory"), "{err}");

    let dir = records_dir(&f);
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(&dir)
        .assert()
        .success();
    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["records", "export"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("THOTH_RUN_ID"));
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
}

// (c) the whole history, in a fixed shape -----------------------------------------

#[test]
fn export_regenerates_the_whole_history_in_a_fixed_shape() {
    let f = init();
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    let conn = f.state_conn();
    insert_task(
        &conn,
        "t1",
        "first task",
        r#"{"task_type":"docs","difficulty":"low"}"#,
        "normal",
    );
    insert_task(&conn, "t2", "second", "{}", "low");
    insert_run(
        &conn, "r1", "t1", 1, "implementer", "anthropic", "claude-a", "sonnet",
        r#"{"launch":{"effort_requested":"high","effort":"high"}}"#, "completed",
        "2026-01-01T00:00:00Z", "2026-01-01T00:10:00Z", Some(10), Some(20),
    );
    conn.execute(
        "UPDATE runs SET cache_read_tokens=5, cache_write_tokens=1, reasoning_tokens=2, total_tokens=36, exit_code=0 WHERE run_id='r1'",
        [],
    )
    .unwrap();
    insert_run(
        &conn, "r2", "t2", 1, "implementer", "anthropic", "claude-a", "sonnet", "{}", "failed",
        "2026-01-01T01:00:00Z", "2026-01-01T01:10:00Z", None, None,
    );
    insert_run(
        &conn, "rv", "t1", 1, "reviewer", "anthropic", "claude-a", "opus", "{}", "completed",
        "2026-01-01T00:15:00Z", "2026-01-01T00:20:00Z", Some(99), Some(99),
    );
    insert_run(
        &conn, "r3", "t2", 2, "implementer", "openai", "codex-a", "gpt-5.6-luna", "{}", "completed",
        "2026-01-01T02:00:00Z", "2026-01-01T02:10:00Z", Some(7), Some(8),
    );
    conn.execute(
        "INSERT INTO reviews(review_id,target_run_id,reviewer_run_id,verdict,findings,created_at)
         VALUES('v1','r1','rv','PASS','{}','2026-01-01T00:20:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events(ts,task_id,run_id,type,detail) VALUES('2026-01-01T00:00:00Z','t1','r1','launch_choice',?1)",
        [r#"{"task_type":"code","difficulty":null,"risk":"low","recommended":{"profile":"codex-a","model":"gpt-5.6-luna","effort_requested":"medium","effort":"medium"},"deviates":true,"exempt":null,"reason":"codex is busy"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events(ts,task_id,run_id,type,detail) VALUES('2026-01-01T00:10:00Z','t1','r1','run_quota',?1)",
        [r#"{"window":"five_hour","points":1.5,"seven_day":{"points":0.5},"reliable":true,"unreliable_reasons":[],"source":"claude_stream"}"#],
    )
    .unwrap();
    // RJ-3: the shape `LaunchChoice::detail` writes since RJ-1, with --reason
    conn.execute(
        "INSERT INTO events(ts,task_id,run_id,type,detail) VALUES('2026-01-01T02:00:00Z','t2','r3','launch_choice',?1)",
        [r#"{"role":"implementer","source":"dispatch","task_type":null,"difficulty":null,"risk":"low","actual":{"profile":"codex-a","model":"gpt-5.6-luna","effort_requested":null,"effort":null},"inherited":{"model":false,"effort":false},"previous_run":"r2","reason":"claude-a is out of quota","require_reason":false}"#],
    )
    .unwrap();
    drop(conn);

    let dir = records_dir(&f);
    set_records(&f, &dir, "alpha");
    let v = json_out(f.cmd().args(["records", "export", "--json"]));
    let file = dir.join("alpha.jsonl");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["path"], file.to_string_lossy().as_ref(), "{v}");
    assert_eq!(v["machine"], "alpha", "{v}");
    assert_eq!(v["lines"], 4, "{v}");
    assert_eq!(v["repos"], 1, "{v}");

    let lines = read_lines(&file);
    assert_eq!(lines.len(), 4);
    let order: Vec<&str> = lines.iter().map(|l| l["run_id"].as_str().unwrap()).collect();
    assert_eq!(order, ["r1", "r2", "r3", "rv"]);

    // RJ-3: every line has exactly v1's keys less the three about a
    // recommendation; none of the launch_choice-only keys is added
    for line in &lines {
        assert_eq!(keys_of(line), line_keys(), "{line}");
    }

    // r1's launch_choice is a DT-2 one: its reason is read, the old keys are not
    let r1 = line_of(&lines, "r1");
    assert_eq!(r1["v"], 2);
    assert_eq!(r1["machine"], "alpha");
    assert_eq!(r1["repo_id"], f.repo_id().as_str());
    assert_eq!(
        r1["repo"],
        f.repo_path().file_name().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(r1["task_id"], "t1");
    assert_eq!(r1["task_title"], "first task");
    assert_eq!(r1["attempt"], 1);
    assert_eq!(r1["role"], "implementer");
    assert_eq!(r1["profile"], "claude-a");
    assert_eq!(r1["provider"], "anthropic");
    assert_eq!(r1["harness"], "claude-code");
    assert_eq!(r1["model"], "sonnet");
    assert_eq!(r1["effort_requested"], "high");
    assert_eq!(r1["effort"], "high");
    assert_eq!(r1["task_type"], "code");
    assert_eq!(r1["difficulty"], "low");
    assert_eq!(r1["risk"], "low");
    assert_eq!(r1["status"], "completed");
    assert_eq!(r1["interrupted_reason"], Value::Null);
    assert_eq!(r1["exit_code"], 0);
    assert_eq!(r1["started_at"], "2026-01-01T00:00:00Z");
    assert_eq!(r1["ended_at"], "2026-01-01T00:10:00Z");
    assert_eq!(r1["verdict"], "PASS");
    assert_eq!(
        r1["tokens"],
        json!({"input": 10, "cache_read": 5, "cache_write": 1, "output": 20, "reasoning": 2, "total": 36})
    );
    assert_eq!(
        r1["quota"],
        json!({"five_hour_points": 1.5, "seven_day_points": 0.5, "reliable": true, "unreliable_reasons": [], "source": "claude_stream"})
    );
    assert_eq!(r1["reason"], "codex is busy");

    let r2 = line_of(&lines, "r2");
    assert_eq!(r2["effort_requested"], Value::Null);
    assert_eq!(r2["effort"], Value::Null);
    assert_eq!(r2["task_type"], Value::Null);
    assert_eq!(r2["difficulty"], Value::Null);
    assert_eq!(r2["risk"], "low");
    assert_eq!(r2["status"], "failed");
    assert_eq!(r2["verdict"], Value::Null);
    assert_eq!(
        r2["tokens"],
        json!({"input": null, "cache_read": null, "cache_write": null, "output": null, "reasoning": null, "total": null})
    );
    assert_eq!(r2["quota"], Value::Null);
    // no launch_choice at all (a run from before DT-2)
    assert_eq!(r2["reason"], Value::Null);
    assert_eq!(r2["task_title"], "second");

    let rv = line_of(&lines, "rv");
    assert_eq!(rv["role"], "reviewer");
    assert_eq!(rv["model"], "opus");
    assert_eq!(rv["verdict"], "PASS");

    let r3 = line_of(&lines, "r3");
    assert_eq!(r3["profile"], "codex-a");
    assert_eq!(r3["provider"], "openai");
    assert_eq!(r3["harness"], "codex");
    assert_eq!(r3["model"], "gpt-5.6-luna");
    assert_eq!(r3["attempt"], 2);
    // an RJ-1 launch_choice: the reason given with --reason
    assert_eq!(r3["reason"], "claude-a is out of quota");

    for line in &lines {
        for key in [
            "spec", "error_text", "worktree", "run_dir", "config_dir", "harness_session_id",
            "candidate_commit",
        ] {
            assert!(line.get(key).is_none(), "{key} must not be written: {line}");
        }
    }

    let b1 = fs::read(&file).unwrap();
    f.cmd().args(["records", "export", "--json"]).assert().success();
    let b2 = fs::read(&file).unwrap();
    assert_eq!(b1, b2);

    f.cmd().args(["config", "set", "machine-name", "beta"]).assert().success();
    f.cmd().args(["records", "export"]).assert().success();
    assert!(dir.join("beta.jsonl").exists());
    assert!(dir.join("alpha.jsonl").exists());
    assert_eq!(fs::read(&file).unwrap(), b1);

    let v = json_out(
        f.cmd()
            .env("THOTH_MACHINE_NAME", "env box")
            .args(["records", "export", "--json"]),
    );
    assert_eq!(v["machine"], "env-box", "{v}");
    assert!(dir.join("env-box.jsonl").exists());
}

// (d) the runner writes the file when a run ends ----------------------------------

#[test]
fn the_runner_writes_the_file_when_a_run_ends() {
    let f = init();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let dir = records_dir(&f);
    set_records(&f, &dir, "beta");
    // RJ-1: the launch_choice event records the labels as written, so the
    // task carries them
    let mut spec: Value = serde_json::from_str(Fixture::SPEC_TWO_AC).unwrap();
    spec["task_type"] = json!("code");
    spec["difficulty"] = json!("normal");
    let (task, _) = f.create_task(&spec.to_string());
    let d = f.dispatch(
        &task,
        "claude-b",
        "hooks",
        &["--model", "sonnet", "--reason", "tests are many"],
    );
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");

    let file = dir.join("beta.jsonl");
    let lines = poll_lines(
        &file,
        |l| l.len() == 1 && l[0]["status"] == "completed",
        30,
    );
    let l = &lines[0];
    assert_eq!(l["run_id"], run_id.as_str());
    assert_eq!(l["status"], "completed");
    assert_eq!(l["role"], "implementer");
    assert_eq!(l["machine"], "beta");
    assert_eq!(l["model"], "sonnet");
    assert_eq!(l["effort_requested"], "medium");
    assert_eq!(l["effort"], "medium");
    assert_eq!(l["task_type"], "code");
    assert_eq!(l["difficulty"], "normal");
    assert_eq!(l["risk"], "normal");
    assert_eq!(l["verdict"], Value::Null);
    assert_eq!(l["tokens"]["input"], 34);
    assert_eq!(l["tokens"]["cache_read"], 202655);
    assert_eq!(l["tokens"]["cache_write"], 1840);
    assert_eq!(l["tokens"]["output"], 610);
    assert_eq!(l["tokens"]["reasoning"], 226);
    assert_eq!(l["tokens"]["total"], 205139);
    assert_eq!(l["quota"]["reliable"], true);
    assert_eq!(l["quota"]["source"], "claude_stream");
    assert_eq!(l["quota"]["five_hour_points"], Value::Null);
    assert_eq!(l["quota"]["seven_day_points"], Value::Null);
    // RJ-3: the line carries the reason given with --reason, and no key
    // about a recommendation
    assert_eq!(l["v"], 2);
    assert_eq!(l["reason"], "tests are many");
    assert_eq!(keys_of(l), line_keys(), "{l}");
    assert!(!l["started_at"].as_str().unwrap().is_empty());
    assert!(!l["ended_at"].as_str().unwrap().is_empty());
    let title: String = f
        .state_conn()
        .query_row("SELECT title FROM tasks WHERE task_id=?1", [&task], |r| r.get(0))
        .unwrap();
    assert_eq!(l["task_title"], title.as_str());

    let review = f.review_start(&task, "claude-a", "review_pass", &["--model", "opus"]);
    let review_run = review["run_id"].as_str().unwrap().to_string();
    let mut c = f.cmd();
    c.args(["wait", "--run", &review_run, "--timeout", "180s"]);
    f.run_ok(c, "wait --run");
    let lines = poll_lines(
        &file,
        |l| {
            l.len() == 2
                && l.iter().any(|x| x["role"] == "reviewer" && x["status"] == "completed")
        },
        30,
    );
    assert_eq!(line_of(&lines, &run_id)["verdict"], "PASS");
    let rv = line_of(&lines, &review_run);
    assert_eq!(rv["role"], "reviewer");
    assert_eq!(rv["model"], "opus");
    assert_eq!(rv["verdict"], "PASS");
    assert_eq!(rv["quota"]["five_hour_points"], Value::Null);
    assert_eq!(rv["quota"]["reliable"], true);
}

// (e) review record updates the file ----------------------------------------------

#[test]
fn review_record_updates_the_file() {
    let f = init();
    f.add_profile("claude-b");
    let dir = records_dir(&f);
    set_records(&f, &dir, "gamma");
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task, "claude-b", "hooks", &["--model", "sonnet"]);
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");

    let file = dir.join("gamma.jsonl");
    let lines = poll_lines(&file, |l| l.len() == 1, 30);
    assert_eq!(lines[0]["verdict"], Value::Null);

    let review_file = f.home_path().join("review-reject.json");
    fs::write(
        &review_file,
        r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the retry never fires"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[{"severity":"minor","note":"a stray TODO","evidence":["E001"]}]}}"#,
    )
    .unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", &task, "-f"]).arg(&review_file).arg("--json");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let lines = read_lines(&file);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["verdict"], "REJECT");
    assert_eq!(f.task_status(&task), "reject");
}

// (f) an export failure does not touch the run ------------------------------------

#[test]
fn an_export_failure_does_not_touch_the_run() {
    let f = init();
    f.add_profile("claude-b");
    let not_a_dir = f.home_path().join("records-file");
    fs::write(&not_a_dir, "x").unwrap();
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(&not_a_dir)
        .assert()
        .success();
    f.cmd().args(["config", "set", "machine-name", "delta"]).assert().success();
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "ok", &["--model", "sonnet"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    let status: String = f
        .state_conn()
        .query_row("SELECT status FROM runs WHERE run_id=?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "completed");

    let log = run_dir(&f, &run_id).join("runner-error.log");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if fs::read_to_string(&log).is_ok_and(|t| t.contains("records export failed")) {
            break;
        }
        if Instant::now() >= deadline {
            panic!(
                "{} never mentioned the export failure: {:?}",
                log.display(),
                fs::read_to_string(&log)
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let out = f.cmd().args(["records", "export"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a directory"));
}

// (g) thoth models merges the other computers' files ------------------------------

#[test]
fn models_merges_the_other_machines_files() {
    let f = init();
    f.add_profile("claude-a");
    insert_local_history(&f);
    let dir = records_dir(&f);
    let l1 = remote_line("X1", "other", "claude-a");
    let l2 = remote_line("r1", "other", "claude-a");
    let l3 = remote_line("X3", "other", "claude-z");
    write_jsonl(
        &dir.join("other.jsonl"),
        &[l1.to_string(), l2.to_string(), l3.to_string(), "not json".to_string()],
    );
    write_jsonl(
        &dir.join("alpha.jsonl"),
        &[{
            let mut own = remote_line("X9", "alpha", "claude-a");
            own["machine"] = json!("alpha");
            own.to_string()
        }],
    );
    set_records(&f, &dir, "alpha");

    let v = json_out(f.cmd().args(["models", "--json"]));
    assert_eq!(v["records"]["dir"], dir.to_string_lossy().as_ref(), "{v}");
    assert_eq!(v["records"]["machine"], "alpha", "{v}");
    assert_eq!(v["records"]["error"], Value::Null, "{v}");
    assert_eq!(
        v["records"]["files"],
        json!([{"name": "other.jsonl", "machine": "other", "lines": 3, "skipped_lines": 1}]),
        "{v}"
    );
    let profile = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "claude-a")
        .unwrap();
    let sonnet = profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "sonnet")
        .unwrap();
    // RJ-3 (AC10): the JSON has no pass or reject counts
    assert_eq!(
        sonnet["thoth"],
        json!({"implementer_runs": 2, "reviewed": 2}),
        "{sonnet}"
    );
    let groups = sonnet["thoth_breakdown"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{sonnet}");
    let g = &groups[0];
    assert_eq!(g["effort"], "high");
    assert_eq!(g["task_type"], "code");
    assert_eq!(g["difficulty"], "high");
    assert_eq!(g["implementer_runs"], 2);
    assert_eq!(g["completed"], 2);
    assert_eq!(g["reviewed"], 2);
    assert!(g.get("pass").is_none(), "{g}");
    assert!(g.get("reject").is_none(), "{g}");
    assert_eq!(g["input_tokens"], 15);
    assert_eq!(g["output_tokens"], 26);
    assert_eq!(g["total_tokens"], 11);
    assert_eq!(g["quota_points"], 2.0);
    assert_eq!(g["quota_runs"], 1);
    assert_eq!(g["quota_runs_unreliable"], 0);
    assert_eq!(g["machines"], json!(["alpha", "other"]));

    let out = f.cmd().arg("models").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("records: "), "{text}");
    assert!(text.contains("1 other file(s), 3 line(s)"), "{text}");

    let (task, _) = f.create_task(Fixture::SPEC);
    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--task", &task, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Review finding on (g): a blank or whitespace-only line does not parse as a
/// line, so it counts as skipped; and the machine of `NAME.jsonl` is the file
/// name minus one `.jsonl`, so `foo.jsonl.jsonl` belongs to `foo.jsonl`.
#[test]
fn blank_lines_count_as_skipped_and_only_one_suffix_is_removed_from_a_name() {
    let f = init();
    f.add_profile("claude-a");
    let dir = records_dir(&f);
    let line = |run_id: &str| remote_line(run_id, "any", "claude-a").to_string();
    fs::write(dir.join("empty.jsonl"), "").unwrap();
    fs::write(
        dir.join("foo.jsonl.jsonl"),
        format!("{}\n\n   \n{}\n\t\n", line("A1"), line("A2")),
    )
    .unwrap();
    fs::write(dir.join("plain.jsonl"), format!("{}\n\n", line("B1"))).unwrap();
    set_records(&f, &dir, "alpha");

    let v = json_out(f.cmd().args(["models", "--json"]));
    assert_eq!(
        v["records"]["files"],
        json!([
            {"name": "empty.jsonl", "machine": "empty", "lines": 0, "skipped_lines": 0},
            {"name": "foo.jsonl.jsonl", "machine": "foo.jsonl", "lines": 2, "skipped_lines": 3},
            {"name": "plain.jsonl", "machine": "plain", "lines": 1, "skipped_lines": 1},
        ]),
        "{v}"
    );
}

/// RJ-3: a computer whose thoth is not rebuilt yet still writes v1 lines;
/// they are read (their old keys ignored), and a version this thoth does not
/// know is skipped.
#[test]
fn a_v1_line_is_read_and_an_unknown_version_is_skipped() {
    let f = init();
    f.add_profile("claude-a");
    let dir = records_dir(&f);
    let mut v3 = remote_line("V3", "old", "claude-a");
    v3["v"] = json!(3);
    write_jsonl(
        &dir.join("old.jsonl"),
        &[remote_line_v1("V1", "old", "claude-a").to_string(), v3.to_string()],
    );
    set_records(&f, &dir, "alpha");

    let v = json_out(f.cmd().args(["models", "--json"]));
    assert_eq!(
        v["records"]["files"],
        json!([{"name": "old.jsonl", "machine": "old", "lines": 1, "skipped_lines": 1}]),
        "{v}"
    );
    let choices = v["decision_table"]["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 1, "{v}");
    assert_eq!(choices[0]["run_id"], "V1");
    assert_eq!(choices[0]["reason"], "codex is busy");
    for key in ["recommended", "deviates", "exempt"] {
        assert!(choices[0].get(key).is_none(), "{key}: {v}");
    }
}

// (h) without records-dir nothing changes -----------------------------------------

#[test]
fn without_records_dir_nothing_changes() {
    let f = init();
    f.add_profile("claude-a");
    insert_local_history(&f);

    let v = json_out(f.cmd().args(["models", "--json"]));
    let machine = v["records"]["machine"].as_str().unwrap().to_string();
    assert!(!machine.is_empty(), "{v}");
    assert_eq!(
        v["records"],
        json!({"dir": null, "machine": machine, "files": [], "error": null}),
        "{v}"
    );
    let profile = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "claude-a")
        .unwrap();
    let sonnet = profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "sonnet")
        .unwrap();
    // RJ-3 (AC10): the JSON has no pass or reject counts
    assert_eq!(
        sonnet["thoth"],
        json!({"implementer_runs": 1, "reviewed": 1}),
        "{sonnet}"
    );
    let groups = sonnet["thoth_breakdown"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{sonnet}");
    assert_eq!(groups[0]["machines"], json!([machine]), "{sonnet}");

    let out = f.cmd().arg("models").output().unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("records:"));
}

// (i) doctor reports the records dir ----------------------------------------------

const SHELL_VARS: [&str; 7] = [
    "MSYSTEM",
    "SHLVL",
    "PROMPT",
    "PSModulePath",
    "PSExecutionPolicyPreference",
    "SHELL",
    "THOTH_LANG",
];

/// Copied from tests/doctor.rs: `init`, profile `claude-a` and an approved
/// check once per fixture, then a `doctor` command with the fakes, a thoth
/// plugin of this version and a clean shell environment.
fn healthy_base(f: &Fixture) -> assert_cmd::Command {
    if !f.state_db().exists() {
        f.cmd().arg("init").assert().success();
        f.add_profile("claude-a");
        f.cmd()
            .args(["checks", "add", "t1", "--cmd", "git --version"])
            .assert()
            .success();
        f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    }
    let mut c = f.cmd();
    let version = format!("{}-abc123", env!("CARGO_PKG_VERSION"));
    c.env(
        "FAKE_CLAUDE_PLUGINS",
        json!([
            {"id":"superpowers@anthropic-plugin-directory","version":"6.4.2-8ca22dba9a94","scope":"user","enabled":true,"installPath":"y"},
            {"id":"thoth@thoth","version":version,"scope":"user","enabled":true,"installPath":"x"}
        ])
        .to_string(),
    );
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c
}

fn doctor_config_item(f: &Fixture) -> Value {
    let mut c = healthy_base(f);
    c.args(["doctor", "--json"]);
    let out = c.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let items: BTreeMap<String, Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["id"].as_str().unwrap().to_string(), i.clone()))
        .collect();
    items["config"].clone()
}

#[test]
fn doctor_reports_the_records_dir() {
    let f = common::fixture();
    healthy_base(&f);
    f.cmd().args(["config", "set", "machine-name", "epsilon"]).assert().success();
    let missing = f.home_path().join("missing");
    f.cmd()
        .args(["config", "set", "records-dir"])
        .arg(&missing)
        .assert()
        .success();
    let missing_text = missing.to_string_lossy().to_string();

    let config = doctor_config_item(&f);
    assert_eq!(config["status"], "warn", "{config:#}");
    assert!(config["detail"].as_str().unwrap().contains("records-dir"), "{config:#}");
    assert!(
        config["fix"].as_str().unwrap().contains("thoth config set records-dir"),
        "{config:#}"
    );
    assert_eq!(
        config["data"]["records_dir"],
        json!({"value": missing_text, "source": "file", "exists": false}),
        "{config:#}"
    );
    assert_eq!(config["data"]["machine_name"], "epsilon", "{config:#}");

    let mut c = healthy_base(&f);
    c.env("THOTH_LANG", "zh-TW").arg("doctor");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("不是資料夾"), "{text}");

    fs::create_dir_all(&missing).unwrap();
    let config = doctor_config_item(&f);
    assert_eq!(config["status"], "ok", "{config:#}");
    assert_eq!(config["fix"], Value::Null, "{config:#}");
    assert_eq!(config["data"]["records_dir"]["exists"], true, "{config:#}");

    f.cmd().args(["config", "unset", "records-dir"]).assert().success();
    let config = doctor_config_item(&f);
    assert_eq!(
        config["data"]["records_dir"],
        json!({"value": null, "source": "default", "exists": null}),
        "{config:#}"
    );
}
