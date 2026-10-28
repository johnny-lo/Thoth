//! DT-5: the decision table (`thoth models`' `thoth_breakdown`). RJ-1 took
//! the automatic pick that read it out of `recommend` and the launch
//! commands; what is left checks the table itself. Always the fakes and a
//! temporary THOTH_HOME; nothing here reaches a network or a real harness.
//!
//! The helpers are copies of tests/records.rs's, because those files are not
//! modules.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json as j, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

// --- copied from tests/records.rs ------------------------------------------------

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

/// A complete line as another computer would have written it.
fn remote_line(run_id: &str, machine: &str, profile: &str) -> Value {
    j!({
        "v": 1, "machine": machine, "repo_id": "deadbeef0000", "repo": "Thoth",
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
        "recommended": null, "deviates": false, "exempt": "no_recommendation", "reason": null,
    })
}

fn write_jsonl(path: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(path, text).unwrap();
}

// --- this file's own -----------------------------------------------------------

static SEEDS: AtomicUsize = AtomicUsize::new(0);

/// One task labelled `task_type` / `difficulty` with `pass + reject` completed
/// implementer runs of `profile` / `model` at `effort`, the first `pass`
/// reviewed PASS and the rest REJECT, each with a reliable `run_quota` of
/// `points` when given. Returns the run ids.
#[allow(clippy::too_many_arguments)]
fn seed_cell(
    f: &Fixture,
    profile: &str,
    provider: &str,
    model: &str,
    effort: Option<&str>,
    task_type: &str,
    difficulty: &str,
    pass: usize,
    reject: usize,
    points: Option<f64>,
) -> Vec<String> {
    let n = SEEDS.fetch_add(1, Ordering::SeqCst);
    let task_id = format!("CELL-{n}-{model}");
    let conn = f.state_conn();
    conn.execute(
        "INSERT INTO tasks(task_id,title,spec,risk,status,base_commit,created_at,updated_at)
         VALUES(?1,'seeded cell',?2,'normal','completed','abc','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
        params![task_id, j!({"task_type":task_type,"difficulty":difficulty}).to_string()],
    )
    .unwrap();
    let spec = match effort {
        Some(e) => j!({"launch":{"effort_requested":e,"effort":e}}).to_string(),
        None => "{}".to_string(),
    };
    let mut ids = Vec::new();
    for i in 0..pass + reject {
        let run_id = format!("{task_id}-R{i}");
        conn.execute(
            "INSERT INTO runs(run_id,task_id,attempt,role,provider,account,model,spec,status,config_dir,
                              allowed_tools,worktree,run_dir,base_commit,cancel_requested,timeout_seconds,
                              started_at,ended_at)
             VALUES(?1,?2,1,'implementer',?3,?4,?5,?6,'completed','','','','','abc',0,0,
                    '2026-01-01T00:00:00Z','2026-01-01T00:10:00Z')",
            params![run_id, task_id, provider, profile, model, spec],
        )
        .unwrap();
        let verdict = if i < pass { "PASS" } else { "REJECT" };
        conn.execute(
            "INSERT INTO reviews(review_id,target_run_id,reviewer_run_id,verdict,findings,created_at)
             VALUES(?1,?2,NULL,?3,'{}','2026-01-01T00:20:00Z')",
            params![format!("{run_id}-V"), run_id, verdict],
        )
        .unwrap();
        if let Some(p) = points {
            conn.execute(
                "INSERT INTO events(ts,run_id,type,detail) VALUES('2026-01-01T00:10:00Z',?1,'run_quota',?2)",
                params![run_id, j!({"points":p,"reliable":true}).to_string()],
            )
            .unwrap();
        }
        ids.push(run_id);
    }
    ids
}

fn models_json(f: &Fixture) -> Value {
    json_out(f.cmd().args(["models", "--json"]))
}

/// `thoth models --json`'s `thoth_breakdown` of `profile` / `model`.
fn breakdown(v: &Value, profile: &str, model: &str) -> Vec<Value> {
    v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == profile)
        .unwrap_or_else(|| panic!("no profile {profile}: {v}"))["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == model)
        .unwrap_or_else(|| panic!("no model {model}: {v}"))["thoth_breakdown"]
        .as_array()
        .unwrap()
        .clone()
}

// (h) ---------------------------------------------------------------------------

#[test]
fn the_table_counts_runs_not_events() {
    let f = init();
    f.add_profile("claude-a");
    let runs = seed_cell(&f, "claude-a", "anthropic", "sonnet", Some("high"), "code", "high", 1, 0, None);
    let run = &runs[0];
    let conn = f.state_conn();
    for _ in 0..3 {
        conn.execute(
            "INSERT INTO events(ts,run_id,type,detail) VALUES('2026-01-01T00:10:00Z',?1,'run_usage',?2)",
            params![run, r#"{"models":{}}"#],
        )
        .unwrap();
    }
    for p in [5, 1] {
        conn.execute(
            "INSERT INTO events(ts,run_id,type,detail) VALUES('2026-01-01T00:10:00Z',?1,'run_quota',?2)",
            params![run, j!({"points":p,"reliable":true}).to_string()],
        )
        .unwrap();
    }

    let m = models_json(&f);
    let groups = breakdown(&m, "claude-a", "sonnet");
    assert_eq!(groups.len(), 1, "{m}");
    assert_eq!(groups[0]["implementer_runs"], 1, "{m}");
    assert_eq!(groups[0]["reviewed"], 1, "{m}");
    // RJ-3 (AC10): the JSON has no pass count; `reviewed` still counts runs
    assert!(groups[0].get("pass").is_none(), "{m}");
    assert_eq!(groups[0]["quota_runs"], 1, "{m}");
    assert_eq!(groups[0]["quota_points"], 1.0, "{m}");
}

// (i) ---------------------------------------------------------------------------

#[test]
fn remote_lines_feed_the_table() {
    let f = init();
    f.add_profile("claude-a");
    let dir = records_dir(&f);
    let lines: Vec<String> = ["R1", "R2", "R3", "R4", "R5"]
        .iter()
        .map(|id| {
            let mut l = remote_line(id, "other", "claude-a");
            l["verdict"] = j!("PASS");
            l["effort_requested"] = j!("medium");
            l["effort"] = j!("medium");
            l.to_string()
        })
        .collect();
    write_jsonl(&dir.join("other.jsonl"), &lines);
    set_records(&f, &dir, "alpha");

    let m = models_json(&f);
    let groups = breakdown(&m, "claude-a", "sonnet");
    assert_eq!(groups.len(), 1, "{m}");
    assert_eq!(groups[0]["reviewed"], 5, "{m}");
    // RJ-3 (AC10): the JSON has no pass count; `reviewed` still counts runs
    assert!(groups[0].get("pass").is_none(), "{m}");
    assert_eq!(groups[0]["machines"], j!(["other"]), "{m}");
}

// RJ-3 (AC2) ----------------------------------------------------------------------

#[test]
fn every_launch_is_a_choice_and_the_recent_ones_without_a_reason_are_counted() {
    let f = init();
    f.add_profile("claude-a");
    // this computer: one run launched with --reason
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(
        &task,
        "claude-a",
        "ok",
        &["--model", "sonnet", "--effort", "high", "--reason", "tests are many"],
    );
    let local = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    // the shared folder: one v2 line without a reason, started yesterday
    let dir = records_dir(&f);
    let yesterday = (time::OffsetDateTime::now_utc() - time::Duration::days(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut remote = remote_line("REMOTE-1", "other", "claude-a");
    remote["v"] = j!(2);
    for key in ["recommended", "deviates", "exempt"] {
        remote.as_object_mut().unwrap().remove(key);
    }
    remote["started_at"] = j!(yesterday);
    write_jsonl(&dir.join("other.jsonl"), &[remote.to_string()]);
    set_records(&f, &dir, "alpha");

    let m = models_json(&f);
    let table = &m["decision_table"];
    let choices = table["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 2, "{m}");
    assert_eq!(table["summary"]["choices"], 2, "{m}");
    assert_eq!(table["summary"]["choices_recent"], 2, "{m}");
    assert_eq!(table["summary"]["choices_recent_without_reason"], 1, "{m}");
    assert_eq!(table["summary"]["recent_days"], 7, "{m}");
    assert!(table["summary"].get("deviations").is_none(), "{m}");
    assert!(table["summary"].get("deviations_recent").is_none(), "{m}");
    assert!(table.get("deviations").is_none(), "{m}");
    // newest first: this computer's run started today
    assert_eq!(choices[0]["run_id"], local.as_str(), "{m}");
    assert_eq!(choices[0]["machine"], "alpha", "{m}");
    assert_eq!(choices[0]["reason"], "tests are many", "{m}");
    assert_eq!(choices[0]["role"], "implementer", "{m}");
    assert_eq!(choices[0]["task_id"], task.as_str(), "{m}");
    assert_eq!(choices[0]["attempt"], 1, "{m}");
    assert_eq!(choices[0]["profile"], "claude-a", "{m}");
    assert_eq!(choices[0]["model"], "sonnet", "{m}");
    assert_eq!(choices[0]["effort_requested"], "high", "{m}");
    assert_eq!(choices[0]["previous_run"], Value::Null, "{m}");
    assert_eq!(choices[0]["status"], "completed", "{m}");
    assert_eq!(choices[1]["run_id"], "REMOTE-1", "{m}");
    assert_eq!(choices[1]["machine"], "other", "{m}");
    assert_eq!(choices[1]["reason"], Value::Null, "{m}");
    assert_eq!(choices[1]["previous_run"], Value::Null, "{m}");
    assert_eq!(choices[1]["task_type"], "code", "{m}");
    assert_eq!(choices[1]["difficulty"], "high", "{m}");
    assert_eq!(choices[1]["risk"], "normal", "{m}");
    let mut keys: Vec<&str> = choices[1].as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "attempt", "difficulty", "effort", "effort_requested", "machine", "model",
            "previous_run", "profile", "reason", "risk", "role", "run_id", "started_at",
            "status", "task_id", "task_title", "task_type",
        ],
        "{m}"
    );
}

#[test]
fn a_local_run_shows_the_previous_run_of_its_launch_choice() {
    let f = init();
    f.add_profile("claude-a");
    let runs = seed_cell(&f, "claude-a", "anthropic", "sonnet", Some("high"), "code", "high", 1, 1, None);
    f.state_conn()
        .execute(
            "INSERT INTO events(ts,run_id,type,detail) VALUES('2026-01-01T00:00:00Z',?1,'launch_choice',?2)",
            params![runs[1], j!({"role":"implementer","previous_run":runs[0],"reason":"step up"}).to_string()],
        )
        .unwrap();
    let m = models_json(&f);
    let choices = m["decision_table"]["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 2, "{m}");
    let second = choices.iter().find(|c| c["run_id"] == runs[1].as_str()).unwrap();
    assert_eq!(second["previous_run"], runs[0].as_str(), "{m}");
    assert_eq!(second["reason"], "step up", "{m}");
    let first = choices.iter().find(|c| c["run_id"] == runs[0].as_str()).unwrap();
    assert_eq!(first["previous_run"], Value::Null, "{m}");
    // both started in 2026-01: not recent
    assert_eq!(m["decision_table"]["summary"]["choices_recent"], 0, "{m}");
}
