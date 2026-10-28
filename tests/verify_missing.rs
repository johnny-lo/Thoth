mod common;

// CK-1: a verify lists the approved checks the previous verify had and this
// one does not (removed from the catalogue, or replaced and not re-approved),
// and `checks remove` says whether the check was approved.

use common::Fixture;
use serde_json::{json, Value};

/// `init`, a profile, a settled run. Returns (task_id, run_id).
fn setup(f: &Fixture) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _worktree) = f.seed_run("claude-b", Fixture::SPEC);
    f.state_conn()
        .execute("UPDATE runs SET status = 'completed' WHERE run_id = ?1", [&run_id])
        .unwrap();
    (task_id, run_id)
}

fn add_check(f: &Fixture, id: &str) {
    f.cmd()
        .args(["checks", "add", id, "--cmd", "git --version", "--shell", "argv", "--json"])
        .assert()
        .success();
}

fn approve(f: &Fixture) {
    f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
}

fn remove(f: &Fixture, id: &str) -> Value {
    let out = f.cmd().args(["checks", "remove", id, "--json"]).assert().success();
    serde_json::from_slice(&out.get_output().stdout).unwrap()
}

/// `verify RUN --json`: (exit code, parsed stdout).
fn verify_json(f: &Fixture, run: &str, extra: &[&str]) -> (i32, Value) {
    let out = f
        .cmd()
        .args(["verify", run, "--json"])
        .args(extra)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("verify --json is not JSON ({e}): {:?}", out));
    (out.status.code().unwrap(), v)
}

/// `verify RUN` without --json: (exit code, stdout).
fn verify_human(f: &Fixture, run: &str) -> (i32, String) {
    verify_human_args(f, run, &[])
}

fn verify_human_args(f: &Fixture, run: &str, extra: &[&str]) -> (i32, String) {
    let out = f.cmd().args(["verify", run]).args(extra).output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
}

/// (seq, detail) of every verify_ran event, oldest first.
fn verify_events(f: &Fixture) -> Vec<(i64, Value)> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT seq, detail FROM events WHERE type = 'verify_ran' ORDER BY seq")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows.into_iter()
        .map(|(s, d)| (s, serde_json::from_str(&d).unwrap()))
        .collect()
}

/// t1's steps up to and including the second verify; returns (task, run).
fn removed_then_verified_twice(f: &Fixture) -> (String, String) {
    let (task, run) = setup(f);
    add_check(f, "c-git");
    add_check(f, "c-git2");
    approve(f);
    let (code, v1) = verify_json(f, &run, &[]);
    assert_eq!(code, 0);
    assert_eq!(v1["approved_checks"], json!(["c-git", "c-git2"]));
    assert_eq!(v1["previous_verify"], Value::Null);
    assert_eq!(v1["missing_since_last_verify"], json!([]));
    remove(f, "c-git2");
    let (code, v2) = verify_json(f, &run, &[]);
    assert_eq!(code, 0);
    assert_eq!(v2["ok"], true);
    (task, run)
}

#[test]
fn a_removed_approved_check_is_listed_by_the_next_verify_and_recorded_in_its_event() {
    let f = common::fixture();
    let (_task, run) = setup(&f);
    add_check(&f, "c-git");
    add_check(&f, "c-git2");
    approve(&f);

    let (code, v1) = verify_json(&f, &run, &[]);
    assert_eq!(code, 0);
    assert_eq!(v1["approved_checks"], json!(["c-git", "c-git2"]));
    assert_eq!(v1["previous_verify"], Value::Null);
    assert_eq!(v1["missing_since_last_verify"], json!([]));

    remove(&f, "c-git2");

    // The human text, before the next verify becomes the "previous" one: a
    // dry run writes no event, so it still compares against the first verify.
    let (_, h2) = verify_human_args(&f, &run, &["--dry-run"]);
    assert!(
        h2.contains("approved checks missing since the previous verify (run "),
        "{h2}"
    );
    assert!(h2.contains("c-git2 (removed)"), "{h2}");
    assert!(h2.contains(&run), "{h2}");

    let (code, v2) = verify_json(&f, &run, &[]);
    assert_eq!(code, 0);
    assert_eq!(v2["ok"], true);
    assert_eq!(v2["results"].as_array().unwrap().len(), 1);
    assert_eq!(v2["approved_checks"], json!(["c-git"]));
    assert_eq!(v2["previous_verify"]["run_id"], json!(run));
    let events = verify_events(&f);
    assert_eq!(events.len(), 2);
    let first_seq = events[0].0;
    assert_eq!(v2["previous_verify"]["seq"], json!(first_seq));
    assert_eq!(v2["previous_verify"]["approved_checks"], json!(["c-git", "c-git2"]));
    let gone = json!([{"check_id": "c-git2", "now": "removed"}]);
    assert_eq!(v2["missing_since_last_verify"], gone);

    let detail = &events.last().unwrap().1;
    assert_eq!(detail["approved_checks"], json!(["c-git"]));
    assert_eq!(detail["missing_since_last_verify"], gone);
    assert_eq!(detail["previous_verify"]["seq"], json!(first_seq));

    let (_, v3) = verify_json(&f, &run, &[]);
    assert_eq!(v3["missing_since_last_verify"], json!([]));
    assert_eq!(v3["previous_verify"]["approved_checks"], json!(["c-git"]));
}

#[test]
fn a_plain_verify_prints_the_missing_line_between_the_header_and_the_warnings() {
    let f = common::fixture();
    let (_task, run) = setup(&f);
    add_check(&f, "c-git");
    add_check(&f, "c-git2");
    approve(&f);
    verify_json(&f, &run, &[]);
    remove(&f, "c-git2");

    let (code, h) = verify_human(&f, &run);
    assert_eq!(code, 0);
    let lines: Vec<&str> = h.lines().collect();
    assert!(lines[0].starts_with("1 check(s) from "), "{h}");
    assert!(
        lines[1].starts_with("  approved checks missing since the previous verify (run "),
        "{h}"
    );
    assert!(lines[1].ends_with("c-git2 (removed)"), "{h}");
    assert!(lines[2].starts_with("  warning:"), "{h}");
}

#[test]
fn an_overwritten_approved_check_is_listed_as_pending_until_a_human_re_approves_it() {
    let f = common::fixture();
    let (_task, run) = setup(&f);
    add_check(&f, "c-git");
    add_check(&f, "c-git2");
    approve(&f);
    let (code, _) = verify_json(&f, &run, &[]);
    assert_eq!(code, 0);

    add_check(&f, "c-git2");

    let (code, v2) = verify_json(&f, &run, &[]);
    assert_eq!(code, 1);
    assert_eq!(v2["ok"], false);
    let results = v2["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let row = results.iter().find(|r| r["check_id"] == "c-git2").unwrap();
    assert!(row["error"].as_str().unwrap().contains("not approved"), "{row}");
    assert_eq!(
        v2["missing_since_last_verify"],
        json!([{"check_id": "c-git2", "now": "pending"}])
    );
    assert_eq!(v2["approved_checks"], json!(["c-git"]));

    approve(&f);

    let (code, v3) = verify_json(&f, &run, &[]);
    assert_eq!(code, 0);
    assert_eq!(v3["missing_since_last_verify"], json!([]));
    assert_eq!(v3["approved_checks"], json!(["c-git", "c-git2"]));
}

#[test]
fn an_unchanged_catalogue_lists_nothing_and_a_dry_run_reports_but_writes_no_event() {
    let f = common::fixture();
    let (_task, run) = setup(&f);
    add_check(&f, "c-git");
    approve(&f);

    let (_, v1) = verify_json(&f, &run, &[]);
    assert_eq!(v1["previous_verify"], Value::Null);

    let (_, d) = verify_json(&f, &run, &["--dry-run"]);
    for key in ["approved_checks", "previous_verify", "missing_since_last_verify"] {
        assert!(d.get(key).is_some(), "dry run lacks {key}: {d}");
    }
    assert_eq!(d["previous_verify"]["run_id"], json!(run));
    assert_eq!(d["missing_since_last_verify"], json!([]));
    assert_eq!(verify_events(&f).len(), 1);

    let (_, h) = verify_human(&f, &run);
    assert!(!h.contains("missing since the previous verify"), "{h}");
}

#[test]
fn the_no_approved_checks_path_lists_every_approved_check_that_vanished() {
    let f = common::fixture();
    let (_task, run) = setup(&f);
    add_check(&f, "c-git");
    approve(&f);
    let (code, _) = verify_json(&f, &run, &[]);
    assert_eq!(code, 0);

    remove(&f, "c-git");

    let (code, v2) = verify_json(&f, &run, &[]);
    assert_eq!(code, 1);
    assert_eq!(v2["reason"], "no_approved_checks");
    assert_eq!(v2["approved_checks"], json!([]));
    assert_eq!(v2["previous_verify"]["run_id"], json!(run));
    assert_eq!(
        v2["missing_since_last_verify"],
        json!([{"check_id": "c-git", "now": "removed"}])
    );
    assert_eq!(verify_events(&f).len(), 1);

    let (code, h) = verify_human(&f, &run);
    assert_eq!(code, 1);
    assert!(h.contains("no approved checks for this repository"), "{h}");
    assert!(h.contains("c-git (removed)"), "{h}");
}

#[test]
fn a_previous_verify_event_without_approved_checks_compares_as_unknown() {
    let f = common::fixture();
    let (task, run) = setup(&f);
    add_check(&f, "c-git");
    approve(&f);
    let detail = r#"{"source":"approved checks (all)","count":1,"failures":0,"results":[],"warnings":[],"checks":["c-git","c-old"],"worker_declared":[]}"#;
    f.state_conn()
        .execute(
            "INSERT INTO events (ts, task_id, run_id, type, detail)
             VALUES ('2026-10-05T00:00:00Z', ?1, ?2, 'verify_ran', ?3)",
            rusqlite::params![task, run, detail],
        )
        .unwrap();
    remove(&f, "c-git");

    let (_, v) = verify_json(&f, &run, &[]);
    assert_eq!(v["previous_verify"]["run_id"], json!(run));
    assert_eq!(v["previous_verify"]["approved_checks"], Value::Null);
    assert_eq!(v["missing_since_last_verify"], json!([]));
}

#[test]
fn checks_remove_reports_whether_the_check_was_approved() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    add_check(&f, "c1");
    let r1 = remove(&f, "c1");
    assert_eq!(r1["ok"], true);
    assert_eq!(r1["was_approved"], false);

    add_check(&f, "c1");
    let out = f.cmd().args(["checks", "remove", "c1"]).output().unwrap();
    let h1 = String::from_utf8(out.stdout).unwrap();
    assert!(h1.contains("removed c1"), "{h1}");
    assert!(!h1.contains("it was approved"), "{h1}");

    add_check(&f, "c2");
    approve(&f);
    let r2 = remove(&f, "c2");
    assert_eq!(r2["was_approved"], true);

    add_check(&f, "c3");
    approve(&f);
    let out = f.cmd().args(["checks", "remove", "c3"]).output().unwrap();
    let h3 = String::from_utf8(out.stdout).unwrap();
    assert!(
        h3.contains(
            "removed c3 (it was approved; the next verify lists it under missing_since_last_verify)"
        ),
        "{h3}"
    );

    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE type = 'check_removed' ORDER BY seq")
        .unwrap();
    let details: Vec<Value> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect();
    assert_eq!(details.len(), 4);
    assert_eq!(details[0]["was_approved"], false);
    assert_eq!(details[0]["approved_at"], Value::Null);
    assert_eq!(details[2]["was_approved"], true);
    assert!(details[2]["approved_at"].is_string(), "{}", details[2]);
}

#[test]
fn status_shows_the_newest_verify_and_its_missing_checks_for_the_task_and_the_repo() {
    let f = common::fixture();
    let (task, run) = removed_then_verified_twice(&f);
    let gone = json!([{"check_id": "c-git2", "now": "removed"}]);

    let out = f.cmd().args(["status", &task, "--json"]).assert().success();
    let s: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(s["verify"]["run_id"], json!(run));
    assert_eq!(s["verify"]["ok"], true);
    assert_eq!(s["verify"]["count"], 1);
    assert_eq!(s["verify"]["failures"], 0);
    assert_eq!(s["verify"]["approved_checks"], json!(["c-git"]));
    assert_eq!(s["verify"]["missing_since_last_verify"], gone);
    assert_eq!(s["verify"]["previous_verify"]["run_id"], json!(run));

    let out = f.cmd().args(["status", "--json"]).assert().success();
    let a: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(a["last_verify"]["task_id"], json!(task));
    assert_eq!(a["last_verify"]["run_id"], json!(run));
    assert_eq!(a["last_verify"]["missing_since_last_verify"], gone);
    assert!(a["tasks"].is_array());

    let out = f.cmd().args(["status", &task]).assert().success();
    let h = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(h.contains(&format!("verify: {run}")), "{h}");
    assert!(h.contains("1 check(s), 0 failed or refused"), "{h}");
    assert!(h.contains("c-git2 (removed)"), "{h}");

    let out = f.cmd().arg("status").assert().success();
    let g = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        g.contains("found approved checks missing since the previous verify: c-git2 (removed)"),
        "{g}"
    );
}

#[test]
fn status_without_any_verify_has_null_fields_and_no_verify_line() {
    let f = common::fixture();
    let (task, _run) = setup(&f);

    let out = f.cmd().args(["status", &task, "--json"]).assert().success();
    let s: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(s["verify"], Value::Null);
    let out = f.cmd().args(["status", "--json"]).assert().success();
    let a: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(a["last_verify"], Value::Null);

    let out = f.cmd().args(["status", &task]).assert().success();
    let h = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!h.contains("verify:"), "{h}");
    let out = f.cmd().arg("status").assert().success();
    let g = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(!g.contains("missing since the previous verify"), "{g}");
}

fn report_page(f: &Fixture) -> String {
    let out = f
        .cmd()
        .env("TZ", "UTC")
        .args(["report", "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    std::fs::read_to_string(v["path"].as_str().unwrap()).unwrap()
}

fn insert_verify_event(f: &Fixture, ts: &str, detail: &str) {
    f.state_conn()
        .execute(
            "INSERT INTO events (ts, task_id, run_id, type, detail)
             VALUES (?1, 'T-MISS-1', 'R-MISS-0001', 'verify_ran', ?2)",
            rusqlite::params![ts, detail],
        )
        .unwrap();
}

fn missing_detail(missing: &str) -> String {
    format!(
        r#"{{"source":"approved checks (all)","count":1,"failures":0,"results":[],"warnings":[],"checks":["test"],"worker_declared":[],"approved_checks":["test"],"previous_verify":{{"run_id":"R-OLD","task_id":"T-MISS-1","ts":"2026-10-05T00:00:00Z","seq":1}},"missing_since_last_verify":{missing}}}"#
    )
}

#[test]
fn the_report_page_names_the_missing_checks_under_the_repo_and_stays_quiet_otherwise() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    {
        let conn = f.state_conn();
        conn.execute(
            "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
             VALUES ('T-MISS-1', 'missing one', '{}', 'normal', 'in_review', 'abc',
                     '2026-10-06T00:00:00Z', '2026-10-06T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, status)
             VALUES ('R-MISS-0001', 'T-MISS-1', 1, 'implementer', 'anthropic', 'claude-b', 'completed')",
            [],
        )
        .unwrap();
    }
    let marker = "上次驗證有、這次沒有";

    let page0 = report_page(&f);
    assert!(!page0.contains(marker));

    insert_verify_event(
        &f,
        "2026-10-06T01:02:03Z",
        &missing_detail(
            r#"[{"check_id":"clippy","now":"removed"},{"check_id":"plugin-test","now":"pending"}]"#,
        ),
    );
    let page1 = report_page(&f);
    assert!(
        page1.contains(
            "上次驗證有、這次沒有的已核准檢查：clippy（已移除）、plugin-test（等重新核准）。"
        ),
        "{page1}"
    );
    // The page shows the verify time in the machine's local zone; `TZ` only
    // pins it on Unix, so read the zone label back.
    let head = "（驗證 R-MISS-0，";
    let shown = &page1[page1.find(head).expect("the note names the verify run") + head.len()..];
    let len = common::shown_time_len(shown);
    assert_eq!(
        shown.get(..len + "）".len()),
        Some(format!("{}）", common::local_label_time(2026, 10, 6, 1, 2, shown)).as_str()),
        "{page1}"
    );
    let note = page1.find(marker).unwrap();
    let list = page1.find("<ul class=\"tasks\">").unwrap();
    assert!(note < list, "the note comes before the task list");

    insert_verify_event(&f, "2026-10-06T02:00:00Z", &missing_detail("[]"));
    let page2 = report_page(&f);
    assert!(!page2.contains(marker));

    insert_verify_event(
        &f,
        "2026-10-06T03:00:00Z",
        &missing_detail(r#"[{"check_id":"<b>x</b>","now":"removed"}]"#),
    );
    let page3 = report_page(&f);
    assert!(page3.contains("&lt;b&gt;x&lt;/b&gt;"), "{page3}");
    assert!(!page3.contains("<b>x</b>"));
}
