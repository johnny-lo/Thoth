//! DT-1: which `run_quota` numbers can be trusted as a run's own, and what
//! `thoth models` and `thoth recommend` do with that. Driven end to end
//! through `fake-claude` and `fake-codex`; never a real harness, never the
//! network.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;

fn out_json(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim()).unwrap()
}

// --- profile set-shared (AC5) ----------------------------------------------

fn claude_b() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

fn list_json(f: &Fixture) -> Value {
    let out = f
        .cmd()
        .args(["profile", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    out_json(&out)
}

fn shared_in_list(f: &Fixture, id: &str) -> Value {
    let v = list_json(f);
    let p = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {v}"));
    p["shared"].clone()
}

fn plain_line_of(f: &Fixture, id: &str) -> String {
    let out = f
        .cmd()
        .args(["profile", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8_lossy(&out)
        .lines()
        .find(|l| l.starts_with(id))
        .unwrap_or_else(|| panic!("no line for {id}"))
        .to_string()
}

// AC5 (e)
#[test]
fn set_shared_flips_the_mark_and_profile_list_shows_it() {
    let f = claude_b();
    assert_eq!(shared_in_list(&f, "claude-b"), json!(false));

    let out = f
        .cmd()
        .args(["profile", "set-shared", "claude-b", "on", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        out_json(&out),
        json!({"ok": true, "profile_id": "claude-b", "shared": true, "previous": false})
    );
    assert_eq!(shared_in_list(&f, "claude-b"), json!(true));
    assert!(plain_line_of(&f, "claude-b").ends_with(" shared"), "{}", plain_line_of(&f, "claude-b"));

    let out = f
        .cmd()
        .args(["profile", "set-shared", "claude-b", "off", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = out_json(&out);
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["profile_id"], "claude-b", "{v}");
    assert_eq!(v["shared"], false, "{v}");
    assert_eq!(v["previous"], true, "{v}");
    assert_eq!(shared_in_list(&f, "claude-b"), json!(false));
    assert!(!plain_line_of(&f, "claude-b").contains(" shared"), "{}", plain_line_of(&f, "claude-b"));
}

// AC5 (j)
#[test]
fn set_shared_refuses_an_unknown_profile_a_worker_and_a_bad_value() {
    let f = claude_b();

    let out = f
        .cmd()
        .args(["profile", "set-shared", "nope", "on"])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("no such profile"), "{}", String::from_utf8_lossy(&out));

    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["profile", "set-shared", "claude-b", "on"])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("THOTH_RUN_ID"), "{}", String::from_utf8_lossy(&out));
    assert_eq!(shared_in_list(&f, "claude-b"), json!(false));

    f.cmd()
        .args(["profile", "set-shared", "claude-b", "maybe"])
        .assert()
        .failure()
        .code(2);
    assert_eq!(shared_in_list(&f, "claude-b"), json!(false));
}

// --- the reasons (AC6) -------------------------------------------------------

const ACCOUNT: &str = "claude-b";

/// Inserts a task row of `repo_conn` (for another repo, whose tasks the
/// fixture has no `create_task` for) and a run on `claude-b` under it.
fn insert_run(
    conn: &rusqlite::Connection,
    run_id: &str,
    task_id: &str,
    status: &str,
    started_at: Option<&str>,
    ended_at: Option<&str>,
) {
    conn.execute(
        "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, model, config_dir,
            allowed_tools, worktree, base_commit, spec, status, started_at, ended_at,
            cancel_requested, timeout_seconds, run_dir)
         VALUES (?1, ?2, 1, 'implementer', 'anthropic', ?3, 'sonnet', '', '', '', 'abc', '{}',
            ?4, ?5, ?6, 0, 0, '')",
        params![run_id, task_id, ACCOUNT, status, started_at, ended_at],
    )
    .unwrap();
}

fn insert_task(conn: &rusqlite::Connection, task_id: &str) {
    conn.execute(
        "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
         VALUES (?1, 't', '{}', 'normal', 'completed', 'abc', '2026-01-01T00:00:00Z',
            '2026-01-01T00:00:00Z')",
        [task_id],
    )
    .unwrap();
}

/// A run row of another run of the same account in this repo, under a task of
/// its own (`create_task`).
fn fake_run_here(
    f: &Fixture,
    run_id: &str,
    status: &str,
    started_at: Option<&str>,
    ended_at: Option<&str>,
) {
    let (task_id, _) = f.create_task(Fixture::SPEC);
    insert_run(&f.state_conn(), run_id, &task_id, status, started_at, ended_at);
}

/// Dispatches a fresh task on `claude-b`, waits for `in_review`, and returns
/// the run's single `run_quota` detail.
fn dispatch_and_quota(f: &Fixture) -> Value {
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task_id, ACCOUNT, "ok", &["--model", "sonnet"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = 'run_quota' ORDER BY seq")
        .unwrap();
    let all: Vec<Value> = stmt
        .query_map([&run_id], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect();
    assert_eq!(all.len(), 1, "one run_quota: {all:?}\n{}", f.explain_run(&run_id));
    all[0].clone()
}

fn assert_points_zero(q: &Value) {
    let p = q["points"].as_f64().unwrap_or_else(|| panic!("points must be a number: {q}"));
    assert!(p.abs() < 1e-9, "{q}");
}

// AC6 (f)
#[test]
fn another_run_of_the_account_in_this_repo_that_overlaps_makes_the_points_unreliable() {
    let f = claude_b();
    fake_run_here(
        &f,
        "OVERLAP1",
        "completed",
        Some("2000-01-01T00:00:00Z"),
        Some("2100-01-01T00:00:00Z"),
    );
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], false, "{q}");
    assert_eq!(q["unreliable_reasons"], json!(["overlap:OVERLAP1"]), "{q}");
    // the points are still recorded
    assert_points_zero(&q);
}

/// A second git repo registered in the fixture's THOTH_HOME; returns its
/// state.db connection.
fn second_repo(f: &Fixture) -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    common::git(r, &["init", "--initial-branch=main"]);
    common::git(r, &["config", "user.email", "test@example.com"]);
    common::git(r, &["config", "user.name", "thoth test"]);
    fs::write(r.join("x.txt"), "x\n").unwrap();
    common::git(r, &["add", "x.txt"]);
    common::git(r, &["commit", "-m", "first"]);
    f.cmd_in(r).arg("init").assert().success();
    let repo_id: String = f
        .global_conn()
        .query_row(
            "SELECT repo_id FROM repos WHERE repo_id != ?1",
            [f.repo_id()],
            |r| r.get(0),
        )
        .unwrap();
    let db = f.home_path().join("repos").join(repo_id).join("state.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.busy_timeout(std::time::Duration::from_millis(5000)).unwrap();
    (dir, conn)
}

// AC6 (g)
#[test]
fn a_run_of_the_account_in_another_registered_repo_counts_too() {
    let f = claude_b();
    let (_dir, other) = second_repo(&f);
    insert_task(&other, "OTHER-T");
    insert_run(&other, "OVERLAP2", "OTHER-T", "running", Some("2000-01-01T00:00:00Z"), None);
    drop(other);
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], false, "{q}");
    assert_eq!(q["unreliable_reasons"], json!(["overlap:OVERLAP2"]), "{q}");
}

// AC6 (h)
#[test]
fn runs_that_ended_long_ago_or_never_started_do_not_overlap() {
    let f = claude_b();
    fake_run_here(
        &f,
        "OLD1",
        "completed",
        Some("2000-01-01T00:00:00Z"),
        Some("2000-01-02T00:00:00Z"),
    );
    fake_run_here(&f, "QUEUED1", "queued", None, None);
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], true, "{q}");
    assert_eq!(q["unreliable_reasons"], json!([]), "{q}");
}

// AC6 (i)
#[test]
fn shared_comes_before_the_overlaps_in_the_reasons() {
    let f = claude_b();
    f.cmd().args(["profile", "set-shared", "claude-b", "on"]).assert().success();
    fake_run_here(
        &f,
        "OVERLAP1",
        "completed",
        Some("2000-01-01T00:00:00Z"),
        Some("2100-01-01T00:00:00Z"),
    );
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], false, "{q}");
    assert_eq!(q["unreliable_reasons"], json!(["shared", "overlap:OVERLAP1"]), "{q}");
}

// AC6 (i2)
#[test]
fn a_shared_profile_alone_is_the_only_reason_and_off_makes_it_reliable_again() {
    let f = claude_b();
    f.cmd().args(["profile", "set-shared", "claude-b", "on"]).assert().success();
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], false, "{q}");
    assert_eq!(q["unreliable_reasons"], json!(["shared"]), "{q}");

    f.cmd().args(["profile", "set-shared", "claude-b", "off"]).assert().success();
    let q = dispatch_and_quota(&f);
    assert_eq!(q["reliable"], true, "{q}");
    assert_eq!(q["unreliable_reasons"], json!([]), "{q}");
}

// --- the averages (AC7) --------------------------------------------------------

fn models_json(f: &Fixture) -> Value {
    out_json(
        &f.cmd()
            .args(["models", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
}

fn breakdown_of(v: &Value, profile: &str, model: &str) -> Vec<Value> {
    let p = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == profile)
        .unwrap_or_else(|| panic!("no {profile} in {v}"));
    let m = p["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == model)
        .unwrap_or_else(|| panic!("no {model} in {p}"));
    m["thoth_breakdown"].as_array().unwrap().clone()
}

// AC7 (l)
#[test]
fn models_leaves_unreliable_runs_out_of_the_quota_sums() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute(
        "INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at)
         VALUES ('t1','t1','{}','normal','completed','abc','2026-01-01','2026-01-01')",
        [],
    )
    .unwrap();
    for id in ["r1", "r2", "r3", "r4"] {
        c.execute(
            "INSERT INTO runs (run_id,task_id,role,account,model,status,spec)
             VALUES (?1,'t1','implementer','claude-a','sonnet','completed','{}')",
            [id],
        )
        .unwrap();
    }
    let events = [
        ("r1", r#"{"points":1.5}"#),
        (
            "r2",
            r#"{"points":2,"reliable":true,"seven_day":{"start_percent":1,"end_percent":1.5,"points":0.5}}"#,
        ),
        (
            "r3",
            r#"{"points":99,"reliable":false,"unreliable_reasons":["shared"],"seven_day":{"start_percent":0,"end_percent":9,"points":9}}"#,
        ),
        ("r4", r#"{"points":5,"reliable":false}"#),
        ("r4", r#"{"points":1,"reliable":true}"#),
    ];
    for (run, detail) in events {
        c.execute(
            "INSERT INTO events (run_id,type,detail) VALUES (?1,'run_quota',?2)",
            params![run, detail],
        )
        .unwrap();
    }
    drop(c);
    let groups = breakdown_of(&models_json(&f), "claude-a", "sonnet");
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    assert_eq!(g["quota_points"].as_f64(), Some(4.5), "{g}");
    assert_eq!(g["quota_runs"], 3, "{g}");
    assert_eq!(g["quota_points_weekly"].as_f64(), Some(0.5), "{g}");
    assert_eq!(g["quota_runs_weekly"], 1, "{g}");
    assert_eq!(g["quota_runs_unreliable"], 1, "{g}");
    assert_eq!(g["implementer_runs"], 4, "{g}");
}

// AC7 (m)
#[test]
fn a_repo_whose_state_db_is_still_v4_does_not_break_models() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (_dir, other) = second_repo(&f);
    for column in ["cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "total_tokens"] {
        other
            .execute_batch(&format!("ALTER TABLE runs DROP COLUMN {column}"))
            .unwrap();
    }
    other.execute("DELETE FROM schema_version WHERE version >= 5", []).unwrap();
    insert_task(&other, "OLD-T");
    other
        .execute(
            "INSERT INTO runs (run_id,task_id,role,account,model,status,spec,input_tokens,output_tokens)
             VALUES ('OLDRUN','OLD-T','implementer','claude-a','sonnet','completed','{}',7,8)",
            [],
        )
        .unwrap();
    drop(other);

    let groups = breakdown_of(&models_json(&f), "claude-a", "sonnet");
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    assert_eq!(g["input_tokens"], 7, "{g}");
    assert_eq!(g["output_tokens"], 8, "{g}");
    for key in ["cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "total_tokens"] {
        assert_eq!(g[key], 0, "{key}: {g}");
    }
}
