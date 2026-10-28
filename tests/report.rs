mod common;

use common::Fixture;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const FAR_FUTURE: i64 = 4_102_444_800; // 2100-01-01T00:00:00Z

fn init(f: &Fixture) {
    f.cmd().arg("init").assert().success();
}

fn insert_task(conn: &Connection, id: &str, title: &str, status: &str, risk: &str, updated_at: &str) {
    conn.execute(
        "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
         VALUES (?1, ?2, '{}', ?3, ?4, 'abc', ?5, ?5)",
        params![id, title, risk, status, updated_at],
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
    account: &str,
    started_at: &str,
    ended_at: Option<&str>,
) {
    conn.execute(
        "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, status, started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, 'anthropic', ?5, 'completed', ?6, ?7)",
        params![run_id, task_id, attempt, role, account, started_at, ended_at],
    )
    .unwrap();
}

fn insert_review(conn: &Connection, review_id: &str, target_run_id: &str, verdict: &str, findings: Value, created_at: &str) {
    conn.execute(
        "INSERT INTO reviews (review_id, target_run_id, reviewer_run_id, verdict, findings, created_at)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
        params![review_id, target_run_id, verdict, findings.to_string(), created_at],
    )
    .unwrap();
}

fn report_json(f: &Fixture) -> Value {
    let out = f
        .cmd()
        .env("TZ", "UTC")
        .args(["report", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("report --json prints JSON")
}

fn report_page(f: &Fixture) -> String {
    let v = report_json(f);
    fs::read_to_string(v["path"].as_str().unwrap()).unwrap()
}

fn snapshot(root: &Path, skip: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    fn walk(root: &Path, dir: &Path, skip: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            let name = path.to_string_lossy().to_string();
            if meta.is_dir() {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.push((rel + "/", 0, std::time::UNIX_EPOCH));
                walk(root, &path, skip, out);
            } else if path != skip && !name.ends_with("-wal") && !name.ends_with("-shm") {
                let rel = path.strip_prefix(root).unwrap().display().to_string();
                out.push((rel, meta.len(), meta.modified().unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, skip, &mut out);
    out.sort();
    out
}

#[test]
fn report_writes_a_self_contained_page_to_the_default_path() {
    let f = common::fixture();
    init(&f);

    let v = report_json(&f);
    let path = PathBuf::from(v["path"].as_str().unwrap());
    assert_eq!(path, f.home_path().join("report.html"));
    assert!(v["generated_at"].as_str().is_some_and(|s| !s.is_empty()), "{v}");

    let page = fs::read_to_string(&path).unwrap();
    assert!(page.starts_with("<!doctype html>"), "{}", &page[..40]);
    assert!(page.contains(r#"<html lang="zh-Hant">"#));
    assert!(page.contains(r#"<meta charset="utf-8">"#));
    assert!(page.contains(r#"<meta name="viewport""#));
    assert!(page.contains("prefers-color-scheme: dark"));
    assert!(!page.to_lowercase().contains("<script"));
    assert!(!page.contains("http://") && !page.contains("https://"));
    for heading in ["總覽", "帳號", "派工決策表", "任務", "審查抓到的問題"] {
        assert!(page.contains(&format!(">{heading}</h2>")), "missing section {heading}");
    }
}

#[test]
fn report_decision_table_rows_sit_in_the_new_section() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    fs::write(f.home_path().join("model-scores.json"), json!({"schema":"aistupidlevel-v1","fetched_at":"2026-01-01T00:00:00Z","models":{"success":true,"data":[]}}).to_string()).unwrap();
    let conn = f.state_conn();
    insert_task(&conn, "T-M-1", "model one", "pass", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-M-2", "model two", "pass", "normal", "2026-09-28T00:00:00Z");
    conn.execute("UPDATE tasks SET spec=?1 WHERE task_id='T-M-1'", [r#"{"task_type":"code","difficulty":"high"}"#]).unwrap();
    conn.execute("UPDATE tasks SET spec=?1 WHERE task_id='T-M-2'", [r#"{"task_type":"docs","difficulty":"low"}"#]).unwrap();
    insert_run(&conn, "R-M-1", "T-M-1", 1, "implementer", "claude-a", "2026-09-28T00:00:00Z", None);
    insert_run(&conn, "R-M-2", "T-M-2", 1, "implementer", "claude-a", "2026-09-28T00:00:00Z", None);
    conn.execute("UPDATE runs SET model='sonnet', spec=?1 WHERE run_id='R-M-1'", [r#"{"launch":{"effort":"high"}}"#]).unwrap();
    conn.execute("UPDATE runs SET model='sonnet', spec=?1 WHERE run_id='R-M-2'", [r#"{"launch":{"effort":"low"}}"#]).unwrap();
    let page = report_page(&f);
    assert!(page.contains("<h2>派工決策表</h2>"));
    assert!(page.contains("<th>effort</th>"));
    assert!(page.contains("<td>寫程式</td>"));
    assert!(page.contains("<td>文件</td>"));
    assert!(!page.contains("class=\"breakdown\""));
    conn.execute("DELETE FROM runs WHERE run_id='R-M-2'", []).unwrap();
    let page = report_page(&f);
    assert!(page.contains("<th>effort</th>"));
    assert!(page.contains("<td>寫程式</td>"));
    assert!(!page.contains("<td>文件</td>"));
}

#[test]
fn report_out_writes_the_page_where_asked_and_leaves_no_temp_file() {
    let f = common::fixture();
    init(&f);
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("status.html");

    let out = f
        .cmd()
        .args(["report", "--json", "--out"])
        .arg(&target)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(PathBuf::from(v["path"].as_str().unwrap()), target);
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, ["status.html"]);
    assert!(!f.home_path().join("report.html").exists());
}

#[test]
fn report_escapes_database_text() {
    let f = common::fixture();
    init(&f);
    let conn = f.state_conn();
    insert_task(&conn, "T-ESCAPE-0001", "<script>alert(1)</script> & \"quoted\" 'single'", "running", "normal", "2026-09-28T00:00:00Z");

    let page = report_page(&f);
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;quoted&quot; &#39;single&#39;"));
    assert!(!page.contains("<script>alert(1)"));
}

#[test]
fn report_translates_every_status_risk_and_verdict() {
    let f = common::fixture();
    init(&f);
    let conn = f.state_conn();
    let statuses = [
        ("draft", "草稿"),
        ("queued", "排隊中"),
        ("running", "進行中"),
        ("in_review", "等審查"),
        ("pass", "通過"),
        ("reject", "退件"),
        ("unverified", "無法驗證"),
        ("interrupted", "中斷"),
        ("cancelled", "已取消"),
        ("failed", "失敗"),
        ("accepted", "已收件"),
    ];
    let risks = ["low", "normal", "important", "high"];
    for (i, (status, _)) in statuses.iter().enumerate() {
        let id = format!("T-STATUS-{i:04}");
        insert_task(&conn, &id, &format!("task {status}"), status, risks[i % 4], "2026-09-28T00:00:00Z");
    }
    insert_run(&conn, "R-PASS", "T-STATUS-0004", 1, "implementer", "claude-b", "2026-09-28T00:00:00Z", None);
    insert_run(&conn, "R-REJ", "T-STATUS-0005", 1, "implementer", "claude-b", "2026-09-28T00:00:00Z", None);
    insert_run(&conn, "R-UNV", "T-STATUS-0006", 1, "implementer", "claude-b", "2026-09-28T00:00:00Z", None);
    insert_review(&conn, "V1", "R-PASS", "PASS", json!({"criteria": []}), "2026-09-28T01:00:00Z");
    insert_review(&conn, "V2", "R-REJ", "REJECT", json!({"criteria": []}), "2026-09-28T01:00:00Z");
    insert_review(&conn, "V3", "R-UNV", "UNVERIFIED", json!({"criteria": []}), "2026-09-28T01:00:00Z");

    let page = report_page(&f);
    for (_, label) in statuses {
        // A task row's pill is followed by its body `<div>`; the findings
        // section's REJECT / UNVERIFIED pills are not, so they cannot mask a
        // wrong task label.
        assert!(page.contains(&format!(">{label}</span><div>")), "missing task status label {label}");
    }
    for label in ["風險：低", "風險：一般", "風險：重要", "風險：高"] {
        assert!(page.contains(label), "missing {label}");
    }
    for label in ["審查：通過", "審查：退件", "審查：無法驗證", "審查：—"] {
        assert!(page.contains(label), "missing {label}");
    }
    assert!(page.contains("實作 1 次"), "implementer attempt count");
}

#[test]
fn report_overview_counts_tasks_by_what_they_need() {
    let f = common::fixture();
    init(&f);
    let conn = f.state_conn();
    let recent = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    let recent = recent.format(&time::format_description::well_known::Rfc3339).unwrap();
    insert_task(&conn, "T-O-1", "queued one", "queued", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-2", "running one", "running", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-3", "review one", "in_review", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-4", "failed one", "failed", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-5", "reject one", "reject", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-6", "unverified one", "unverified", "normal", "2026-09-28T00:00:00Z");
    insert_task(&conn, "T-O-7", "passed recently", "pass", "normal", &recent);
    insert_task(&conn, "T-O-8", "passed long ago", "pass", "normal", "2020-01-01T00:00:00Z");
    // A document task taken with `task accept`: finished, in none of the four counts.
    insert_task(&conn, "T-O-9", "accepted recently", "accepted", "normal", &recent);

    let page = report_page(&f);
    assert!(page.contains(r#"<b class="n run">2</b> 件進行中"#), "in progress");
    assert!(page.contains(r#"<b class="n wait">1</b> 件等審查"#), "waiting for review");
    assert!(page.contains(r#"<b class="n bad">3</b> 件需要處理"#), "needs attention");
    assert!(page.contains(r#"<b class="n ok">1</b> 件最近 7 天通過"#), "passed in 7 days");
    assert!(
        page.contains(
            r#"<li class="task closed"><span class="pill ok">已收件</span><div><p class="ttitle">accepted recently</p>"#
        ),
        "the accepted task is listed among the finished ones"
    );
}

#[test]
fn report_lists_open_tasks_first_and_caps_closed_tasks_at_30() {
    let f = common::fixture();
    init(&f);
    let conn = f.state_conn();
    insert_task(&conn, "T-OPEN-OLD", "open older", "in_review", "normal", "2026-09-01T00:00:00Z");
    insert_task(&conn, "T-OPEN-NEW", "open newer", "running", "normal", "2026-09-02T00:00:00Z");
    for i in 0..32 {
        let status = if i % 2 == 0 { "pass" } else { "cancelled" };
        let ts = format!("2026-09-27T{:02}:{:02}:00Z", i / 60, i % 60);
        insert_task(&conn, &format!("T-CLOSED-{i:02}"), &format!("closed number {i:02}"), status, "low", &ts);
    }

    let page = report_page(&f);
    let at = |needle: &str| page.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
    assert!(at("open newer") < at("open older"));
    assert!(at("open older") < at("closed number 31"));
    assert!(at("closed number 31") < at("closed number 02"));
    assert!(!page.contains("closed number 01") && !page.contains("closed number 00"));
    assert!(page.contains("另外還有 2 件已結束的任務沒有列出"));
    assert!(page.contains(">T-OPEN-N<"), "first 8 characters of the id");
}

#[test]
fn report_lists_what_rejected_reviews_caught() {
    let f = common::fixture();
    init(&f);
    let page = report_page(&f);
    assert!(page.contains("目前沒有"));

    let conn = f.state_conn();
    insert_task(&conn, "T-F-1", "task with findings", "reject", "normal", "2026-09-28T00:00:00Z");
    insert_run(&conn, "R-F-1", "T-F-1", 1, "implementer", "claude-b", "2026-09-28T00:00:00Z", None);
    insert_review(
        &conn,
        "V-F-1",
        "R-F-1",
        "REJECT",
        json!({
            "criteria": [
                {"id": "AC1", "verdict": "met", "evidence": ["E001"]},
                {"id": "AC2", "verdict": "unmet", "evidence": ["E002"], "note": "the sort order is reversed"},
                {"id": "AC3", "verdict": " unmet ", "evidence": ["E002"], "note": "padded verdict still counts"}
            ],
            "extra_findings": [{"severity": "important", "note": "reads miss rows still in the WAL", "evidence": ["E003"]}]
        }),
        "2026-09-28T01:00:00Z",
    );
    insert_review(&conn, "V-F-2", "R-F-1", "PASS", json!({"criteria": [{"id": "AC9", "verdict": "met", "note": "passing note"}]}), "2026-09-28T02:00:00Z");

    let page = report_page(&f);
    assert!(!page.contains("目前沒有"));
    assert!(page.contains("task with findings"));
    assert!(page.contains("AC2") && page.contains("the sort order is reversed"));
    assert!(page.contains("重要") && page.contains("reads miss rows still in the WAL"));
    assert!(!page.contains(">AC1<"), "met criteria are not listed");
    assert!(page.contains("AC3") && page.contains("padded verdict still counts"), "`review record` trims verdicts");
    assert!(!page.contains("passing note"), "PASS reviews are not listed");
}

#[test]
fn report_shows_quota_windows_and_account_work() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-b");
    f.add_codex_profile("codex-a");
    let conn = f.state_conn();
    insert_task(&conn, "T-W-1", "worked task", "pass", "normal", "2026-09-28T00:00:00Z");
    insert_run(&conn, "R-W-1", "T-W-1", 1, "implementer", "claude-b", "2026-09-28T00:00:00Z", Some("2026-09-28T00:10:00Z"));
    insert_run(&conn, "R-W-2", "T-W-1", 2, "implementer", "claude-b", "2026-09-28T01:00:00Z", Some("2026-09-28T01:05:00Z"));
    insert_run(&conn, "R-W-3", "T-W-1", 1, "reviewer", "claude-b", "2026-09-28T02:00:00Z", None);
    for (ts, kind, util, resets) in [
        ("2026-09-28T00:05:00Z", "five_hour", 0.52, 0_i64),
        ("2026-09-28T00:06:00Z", "seven_day", 0.61, FAR_FUTURE),
    ] {
        conn.execute(
            "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, 'R-W-1', 'rate_limit_warning', ?2)",
            params![ts, json!({"rate_limit_type": kind, "utilization": util, "resets_at": resets}).to_string()],
        )
        .unwrap();
    }

    let page = report_page(&f);
    assert!(page.contains(">可用<"), "active status label");
    assert!(page.contains("5 小時") && page.contains("一週"));
    assert!(page.contains("已重置"), "the five_hour window reset long ago");
    assert!(page.contains("61%"));
    assert!(page.contains("沒有額度資料"), "codex-a has no reading");
    assert!(page.contains("實作 2 次、審查 1 次"));
    assert!(page.contains("合計 15 分鐘"));
    let last = "最近一次 ";
    let shown = &page[page.find(last).expect("the account line shows its latest run") + last.len()..];
    assert_eq!(
        shown.get(..common::shown_time_len(shown)),
        Some(common::local_label_time(2026, 9, 28, 2, 0, shown).as_str()),
        "the latest run is R-W-3 at 02:00Z, shown in the report's local zone"
    );

    f.global_conn()
        .execute("UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-b'", [])
        .unwrap();
    assert!(report_page(&f).contains(">撞到額度<"));
}

#[test]
fn report_reads_rows_still_in_the_wal_and_skips_missing_repos() {
    let f = common::fixture();
    init(&f);
    f.global_conn()
        .execute("INSERT INTO repos (repo_id, root_path, created_at) VALUES ('gone-repo', '/nowhere', '')", [])
        .unwrap();
    let writer = f.state_conn();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    insert_task(&writer, "T-WAL-1", "only in the wal", "running", "normal", "2026-09-28T00:00:00Z");

    let page = report_page(&f);
    assert!(page.contains("only in the wal"));
    assert!(!f.home_path().join("repos").join("gone-repo").exists());
    assert!(!page.contains("/nowhere"));
    drop(writer);
}

#[test]
fn report_changes_nothing_but_the_report_file() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-b");
    let conn = f.state_conn();
    insert_task(&conn, "T-RO-1", "read only", "running", "normal", "2026-09-28T00:00:00Z");
    drop(conn);
    let report = f.home_path().join("report.html");

    let before = snapshot(&f.home_path(), &report);
    report_json(&f);
    report_json(&f);
    assert_eq!(before, snapshot(&f.home_path(), &report));
    assert!(report.is_file());
}

#[cfg(unix)]
#[test]
fn report_shows_times_in_the_local_time_zone() {
    let f = common::fixture();
    init(&f);
    let conn = f.state_conn();
    insert_task(&conn, "T-TZ-1", "timed task", "running", "normal", "2026-09-28T05:31:00Z");

    let out = f
        .cmd()
        .env("TZ", "Asia/Taipei")
        .args(["report", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let page = fs::read_to_string(v["path"].as_str().unwrap()).unwrap();
    assert!(page.contains("2026-09-28 13:31 +08:00"), "local time with offset");
}

#[cfg(target_os = "linux")]
#[test]
fn report_open_failure_is_a_warning_not_an_error() {
    let f = common::fixture();
    init(&f);
    let empty = tempfile::tempdir().unwrap();

    let assert = f
        .cmd()
        .env("PATH", empty.path())
        .args(["report", "--open"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains("report.html"), "{stderr}");
}

#[test]
fn report_out_relative_path_is_reported_as_absolute() {
    let f = common::fixture();
    init(&f);

    let out = f
        .cmd()
        .args(["report", "--json", "--out", "rel-status.html"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let path = PathBuf::from(v["path"].as_str().unwrap());
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(fs::canonicalize(&path).unwrap(), fs::canonicalize(f.repo_path().join("rel-status.html")).unwrap());
}
