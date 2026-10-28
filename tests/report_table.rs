//! DT-7: the dispatch decision table on the report page and in
//! `thoth models --json`.

mod common;

use common::Fixture;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

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
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
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

fn insert_event(conn: &Connection, run_id: &str, ty: &str, detail: Value, ts: &str) {
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, ?3, ?4)",
        params![ts, run_id, ty, detail.to_string()],
    )
    .unwrap();
}

fn set_launch(conn: &Connection, run_id: &str, model: &str, effort: Option<&str>) {
    let spec = match effort {
        Some(e) => json!({"launch": {"effort_requested": e, "effort": e}}),
        None => json!({}),
    };
    conn.execute(
        "UPDATE runs SET model=?2, spec=?3 WHERE run_id=?1",
        params![run_id, model, spec.to_string()],
    )
    .unwrap();
}

fn set_labels(conn: &Connection, task_id: &str, task_type: &str, difficulty: &str) {
    conn.execute(
        "UPDATE tasks SET spec=?2 WHERE task_id=?1",
        params![task_id, json!({"task_type": task_type, "difficulty": difficulty}).to_string()],
    )
    .unwrap();
}

fn set_tokens(conn: &Connection, run_id: &str, input: i64, output: i64, total: i64) {
    conn.execute(
        "UPDATE runs SET input_tokens=?2, output_tokens=?3, total_tokens=?4 WHERE run_id=?1",
        params![run_id, input, output, total],
    )
    .unwrap();
}

fn models_json(f: &Fixture) -> Value {
    let out = f
        .cmd()
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
        .args(["models", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).expect("models --json prints JSON")
}

fn machine(f: &Fixture, name: &str) {
    f.cmd()
        .args(["config", "set", "machine-name", name, "--json"])
        .assert()
        .success();
}

/// The detail of a `launch_choice` event as DT-2 wrote it, with keys about a
/// recommendation that RJ-3 no longer reads.
fn choice(
    recommended: Option<(&str, &str, &str)>,
    actual: (&str, &str, Option<&str>),
    deviates: bool,
    exempt: Option<&str>,
    reason: Option<&str>,
) -> Value {
    json!({
        "role": "implementer",
        "source": "dispatch",
        "recommended": recommended.map(|(p, m, e)| json!({"profile": p, "model": m, "effort_requested": e, "effort": e})),
        "actual": {"profile": actual.0, "model": actual.1, "effort_requested": actual.2, "effort": actual.2},
        "differs": {"profile": false, "model": false, "effort": false},
        "deviates": deviates,
        "exempt": exempt,
        "continuation": false,
        "previous_run": null,
        "reason": reason,
        "require_reason": false,
    })
}

/// The text of the `n`th (from 1) table cell after `after`.
fn td(page: &str, after: &str, n: usize) -> String {
    let start = page.find(after).unwrap_or_else(|| panic!("missing {after}")) + after.len();
    let mut rest = &page[start..];
    for _ in 0..n {
        let at = rest.find("<td").unwrap_or_else(|| panic!("fewer than {n} cells after {after}"));
        rest = &rest[at + 3..];
    }
    let open = rest.find('>').unwrap();
    let rest = &rest[open + 1..];
    rest[..rest.find("</td>").unwrap()].to_string()
}

/// The paragraph whose `id` is `id`, from `<p` to `</p>` inclusive.
fn para(page: &str, id: &str) -> String {
    let at = page.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("missing {id}"));
    let start = page[..at].rfind("<p").unwrap();
    let end = at + page[at..].find("</p>").unwrap() + "</p>".len();
    page[start..end].to_string()
}

/// The every-launch part of the decision table section.
fn choice_part(page: &str) -> String {
    let start = page.find("<h3>每次的選擇與結果</h3>").expect("choices heading");
    let rest = &page[start..];
    rest[..rest.find("</section>").unwrap()].to_string()
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap()
}

#[test]
fn the_table_has_one_row_per_model_account_type_and_effort() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    machine(&f, "alpha");
    let conn = f.state_conn();
    let ts = "2026-09-28T00:00:00Z";
    insert_task(&conn, "T-1", "table one", "pass", "normal", ts);
    insert_task(&conn, "T-2", "table two", "pass", "normal", ts);
    insert_task(&conn, "T-3", "table three", "pass", "normal", ts);
    set_labels(&conn, "T-1", "code", "high");
    set_labels(&conn, "T-2", "code", "normal");
    set_labels(&conn, "T-3", "docs", "low");
    for (run, task, account, model, effort) in [
        ("R-1", "T-1", "claude-a", "sonnet", Some("high")),
        ("R-2", "T-1", "claude-a", "sonnet", Some("high")),
        ("R-3", "T-2", "claude-a", "sonnet", Some("high")),
        ("R-4", "T-1", "claude-b", "sonnet", Some("medium")),
        ("R-5", "T-3", "claude-a", "haiku", None),
        ("R-6", "T-1", "claude-b", "sonnet", Some("high")),
    ] {
        insert_run(&conn, run, task, 1, "implementer", account, ts, None);
        set_launch(&conn, run, model, effort);
    }
    insert_review(&conn, "V-1", "R-1", "PASS", json!({"criteria": []}), ts);
    insert_review(&conn, "V-2", "R-2", "REJECT", json!({"criteria": []}), ts);
    insert_review(&conn, "V-3", "R-3", "PASS", json!({"criteria": []}), ts);
    insert_event(&conn, "R-1", "run_quota", json!({"points": 48, "seven_day": {"points": 9.5}, "reliable": true}), ts);
    insert_event(&conn, "R-2", "run_quota", json!({"points": 50, "reliable": true}), ts);
    insert_event(&conn, "R-3", "run_quota", json!({"points": 99, "reliable": false, "unreliable_reasons": ["shared"]}), ts);
    insert_event(&conn, "R-1", "run_usage", json!({"models": {}}), ts);
    insert_event(&conn, "R-1", "run_usage", json!({"models": {}}), ts);
    set_tokens(&conn, "R-1", 10, 20, 1000);
    set_tokens(&conn, "R-2", 0, 0, 0);
    set_tokens(&conn, "R-3", 300, 400, 0);

    let page = report_page(&f);
    assert!(page.contains("<section id=\"decision-table\">"));
    assert!(page.contains("<h2>派工決策表</h2>"));
    assert!(!page.contains("<h2>模型</h2>"));
    assert!(!page.contains("class=\"breakdown\""));
    assert!(page.contains("<th>平均額度點數（5 小時／一週）</th>"));
    assert!(page.contains("<th>模型</th><th>帳號</th><th>類型</th><th>effort</th>"));
    assert!(!page.contains("<td>sonnet<span"));
    assert!(!page.contains("<td>haiku<span"));
    let haiku = page.find("<td>haiku</td><td>claude-a</td>").unwrap();
    let a_sonnet = page.find("<td>sonnet</td><td>claude-a</td>").unwrap();
    let b_medium = page.find("<td>sonnet</td><td>claude-b</td><td>寫程式</td><td>medium</td>").unwrap();
    let b_high = page.find("<td>sonnet</td><td>claude-b</td><td>寫程式</td><td>high</td>").unwrap();
    assert!(haiku < a_sonnet);
    assert!(a_sonnet < b_medium);
    assert!(b_medium < b_high);

    let row = "<td>sonnet</td><td>claude-a</td>";
    assert_eq!(td(&page, row, 1), "寫程式");
    assert_eq!(td(&page, row, 2), "high");
    assert_eq!(td(&page, row, 3), "3");
    assert_eq!(td(&page, row, 4), "3");
    assert_eq!(td(&page, row, 5), "67%（2/3）");
    assert_eq!(td(&page, row, 6), "567");
    assert_eq!(td(&page, row, 7), "49.0（2 次）／9.5（1 次）；1 次不準");
    assert_eq!(td(&page, row, 8), "alpha");
    let row = "<td>haiku</td><td>claude-a</td>";
    assert_eq!(td(&page, row, 1), "文件");
    assert_eq!(td(&page, row, 2), "沒記");
    assert_eq!(td(&page, row, 5), "—");
    assert_eq!(td(&page, row, 6), "沒量到");
    assert_eq!(td(&page, row, 7), "沒量到／沒量到");
    let row = "<td>sonnet</td><td>claude-b</td><td>寫程式</td><td>medium</td>";
    assert_eq!(td(&page, row, 1), "1");
    assert_eq!(td(&page, row, 2), "0");
    let row = "<td>sonnet</td><td>claude-b</td><td>寫程式</td><td>high</td>";
    assert_eq!(td(&page, row, 1), "1");
    assert_eq!(td(&page, row, 2), "0");
    assert_eq!(td(&page, row, 3), "—");
    assert_eq!(td(&page, row, 4), "沒量到");
    assert_eq!(td(&page, row, 5), "沒量到／沒量到");
    assert_eq!(td(&page, row, 6), "alpha");

    let v = models_json(&f);
    let table = &v["decision_table"];
    // RJ-3: the summary counts every launch (none is recent here), and the
    // rows carry no pass or reject counts (AC10); the page above still shows
    // the pass rate to the human
    assert_eq!(
        table["summary"],
        json!({"runs":6,"machines":1,"rows":4,"choices":6,"choices_recent":0,"choices_recent_without_reason":0,"recent_days":7})
    );
    assert_eq!(
        table["rows"][1],
        json!({"model":"sonnet","profile":"claude-a","task_type":"code","effort":"high","machines":["alpha"],"implementer_runs":3,"reviewed":3,"tokens":1700,"tokens_avg":1700.0_f64 / 3.0,"quota_points":98.0,"quota_runs":2,"quota_points_avg":49.0,"quota_points_weekly":9.5,"quota_runs_weekly":1,"quota_points_weekly_avg":9.5,"quota_runs_unreliable":1})
    );
    assert_eq!(
        table["rows"][3],
        json!({"model":"sonnet","profile":"claude-b","task_type":"code","effort":"high","machines":["alpha"],"implementer_runs":1,"reviewed":0,"tokens":0,"tokens_avg":null,"quota_points":null,"quota_runs":0,"quota_points_avg":null,"quota_points_weekly":null,"quota_runs_weekly":0,"quota_points_weekly_avg":null,"quota_runs_unreliable":0})
    );
    assert_eq!(table["rows"][0]["model"], "haiku");
    assert_eq!(table["rows"][0]["profile"], "claude-a");
    assert!(table["rows"][0]["effort"].is_null());
    assert_eq!(table["rows"][2]["profile"], "claude-b");
    assert_eq!(table["rows"][2]["effort"], "medium");
    assert!(table["rows"].as_array().unwrap().iter().all(|r| r.get("profiles").is_none()));
    assert_eq!(table["choices"].as_array().unwrap().len(), 6);
    assert!(table.get("deviations").is_none());
    assert!(v.get("profiles").is_some());
    assert!(v.get("records").is_some());
    assert!(v.get("auto_refresh").is_some());
    assert!(v.get("cache").is_some());
}

#[test]
fn a_model_outside_the_discover_list_is_in_the_table() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    let conn = f.state_conn();
    let ts = "2026-09-28T00:00:00Z";
    insert_task(&conn, "T-S", "outside", "pass", "normal", ts);
    set_labels(&conn, "T-S", "code", "high");
    insert_run(&conn, "R-S", "T-S", 1, "implementer", "claude-a", ts, None);
    set_launch(&conn, "R-S", "claude-sonnet-4-6", Some("high"));
    insert_review(&conn, "V-S", "R-S", "PASS", json!({"criteria": []}), ts);

    let page = report_page(&f);
    assert!(page.contains("<td>claude-sonnet-4-6</td><td>claude-a</td>"));
    let v = models_json(&f);
    assert_eq!(v["decision_table"]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(v["decision_table"]["rows"][0]["model"], "claude-sonnet-4-6");
    assert_eq!(v["decision_table"]["rows"][0]["profile"], "claude-a");
    let listed = v["profiles"][0]["models"].as_array().unwrap();
    assert!(!listed.iter().any(|m| m["model"] == "claude-sonnet-4-6"));
}

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

fn write_jsonl(path: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(path, text).unwrap();
}

#[test]
fn remote_lines_add_their_computer_and_every_choice() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    let dir = records_dir(&f);
    set_records(&f, &dir, "alpha");
    let mut first = remote_line("R-B1", "beta", "claude-a");
    first["verdict"] = json!("PASS");
    first["task_title"] = json!("remote first");
    first["started_at"] = json!("2026-01-01T00:00:00Z");
    let mut second = remote_line("R-B2", "beta", "claude-a");
    second["verdict"] = json!("REJECT");
    second["task_title"] = json!("remote second");
    second["started_at"] = json!("2026-01-02T00:00:00Z");
    second["reason"] = json!("codex is busy");
    write_jsonl(&dir.join("beta.jsonl"), &[first.to_string(), second.to_string()]);
    let conn = f.state_conn();
    let ts = "2026-09-28T00:00:00Z";
    insert_task(&conn, "T-L", "local one", "pass", "normal", ts);
    set_labels(&conn, "T-L", "code", "high");
    insert_run(&conn, "R-L", "T-L", 1, "implementer", "claude-a", ts, None);
    set_launch(&conn, "R-L", "sonnet", Some("high"));
    insert_review(&conn, "V-L", "R-L", "PASS", json!({"criteria": []}), ts);

    let page = report_page(&f);
    let row = "<td>sonnet</td><td>claude-a</td>";
    assert_eq!(td(&page, row, 8), "alpha、beta");
    assert_eq!(td(&page, row, 3), "3");
    // RJ-3: the section lists every launch, so both shared lines (not only
    // the one that used to deviate) and the local run are there
    let part = choice_part(&page);
    let rows: Vec<&str> = part.split("<tr class=\"dev\">").skip(1).collect();
    assert_eq!(rows.len(), 3, "{part}");
    assert!(rows[0].starts_with("<td>local one<span class=\"meta\">實作者・alpha・"), "{part}");
    assert!(rows[1].starts_with("<td>remote second<span class=\"meta\">實作者・beta・"), "{part}");
    assert!(rows[2].starts_with("<td>remote first<span class=\"meta\">實作者・beta・"), "{part}");
    assert!(rows[1].contains("<td>claude-a sonnet high</td><td>寫程式・high</td><td>codex is busy</td><td>退件</td>"), "{part}");
    assert!(rows[2].contains("<td>claude-a sonnet high</td><td>寫程式・high</td><td>沒寫</td><td>通過</td>"), "{part}");
    assert!(rows[0].contains("<td>claude-a sonnet high</td><td>寫程式・high</td><td>沒寫</td><td>通過</td>"), "{part}");
    let v = models_json(&f);
    assert_eq!(v["decision_table"]["summary"]["machines"], 2);
    assert_eq!(v["decision_table"]["summary"]["choices"], 3);
}

#[test]
fn every_choice_shows_what_was_chosen_its_labels_reason_and_outcome() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    machine(&f, "alpha");
    let now = OffsetDateTime::now_utc();
    let recent1 = rfc3339(now - Duration::hours(1));
    let recent = rfc3339(now - Duration::days(1));
    let old = "2026-01-01T00:00:00Z".to_string();
    let conn = f.state_conn();
    let recommended = Some(("codex-a", "gpt-6-astra", "medium"));
    let actual = ("claude-a", "sonnet", Some("high"));
    for (task, title, run, role, started) in [
        ("T-D1", "dev one", "R-D1", "implementer", &recent1),
        ("T-D2", "dev two", "R-D2", "implementer", &recent),
        ("T-D3", "dev three", "R-D3", "implementer", &old),
        ("T-D4", "dev four", "R-D4", "implementer", &old),
        ("T-D5", "dev five", "R-D5", "reviewer", &old),
    ] {
        insert_task(&conn, task, title, "in_review", "normal", started);
        insert_run(&conn, run, task, 1, role, "claude-a", started, None);
        set_launch(&conn, run, "sonnet", Some("high"));
    }
    set_labels(&conn, "T-D1", "code", "high");
    // An older choice of R-D1 with another reason: only the newest one counts.
    insert_event(&conn, "R-D1", "launch_choice", choice(recommended, actual, false, None, Some("older reason")), "2000-01-01T00:00:00Z");
    insert_event(&conn, "R-D1", "launch_choice", choice(recommended, actual, true, None, Some("codex 額度剩 5%")), &recent1);
    insert_review(&conn, "V-D1", "R-D1", "PASS", json!({"criteria": []}), &recent1);
    insert_event(&conn, "R-D2", "launch_choice", choice(recommended, actual, true, None, None), &recent);
    insert_event(&conn, "R-D3", "launch_choice", choice(recommended, actual, true, Some("continuation"), None), &old);
    conn.execute("UPDATE runs SET status='interrupted' WHERE run_id='R-D3'", []).unwrap();
    insert_event(&conn, "R-D4", "launch_choice", choice(recommended, actual, false, None, None), &old);
    let mut reviewer = choice(recommended, actual, true, None, Some("same provider"));
    reviewer["role"] = json!("reviewer");
    insert_event(&conn, "R-D5", "launch_choice", reviewer, &old);
    insert_review(&conn, "V-D5", "R-D4", "REJECT", json!({"criteria": []}), &old);
    conn.execute("UPDATE reviews SET reviewer_run_id='R-D5' WHERE review_id='V-D5'", []).unwrap();

    // RJ-3: every launch is listed (R-D4 used to be left out because it
    // followed the recommendation), newest first, the same start time by run
    // id; there is no recommended column and no "exempt" reason any more
    let page = report_page(&f);
    assert_eq!(page.matches("<tr class=\"dev\">").count(), 5);
    let part = choice_part(&page);
    assert!(part.contains("<thead><tr><th>任務</th><th>選擇</th><th>類型・難度</th><th>理由</th><th>結果</th></tr></thead>"), "{part}");
    assert!(!part.contains("推薦"), "{part}");
    assert!(!part.contains("免寫"), "{part}");
    assert!(!part.contains("older reason"), "{part}");
    let rows: Vec<&str> = part.split("<tr class=\"dev\">").skip(1).collect();
    let titles: Vec<&str> = rows.iter().map(|r| &r[4..r.find("<span").unwrap()]).collect();
    assert_eq!(titles, ["dev one", "dev two", "dev five", "dev four", "dev three"]);
    assert!(rows[0].contains("<span class=\"meta\">實作者・alpha・"), "{part}");
    assert!(rows[0].contains("<td>claude-a sonnet high</td><td>寫程式・high</td><td>codex 額度剩 5%</td><td>通過</td>"), "{part}");
    assert!(rows[1].contains("<td>claude-a sonnet high</td><td>未標・未標</td><td>沒寫</td><td>做完，還沒審</td>"), "{part}");
    assert!(rows[2].contains("審查者・alpha・"), "{part}");
    assert!(rows[2].contains("<td>same provider</td><td>退件</td>"), "{part}");
    assert!(rows[3].contains("<td>沒寫</td><td>退件</td>"), "{part}");
    assert!(rows[4].contains("<td>沒寫</td><td>中斷</td>"), "{part}");

    let v = models_json(&f);
    let table = &v["decision_table"];
    assert_eq!(table["summary"]["choices"], 5);
    assert_eq!(table["summary"]["choices_recent"], 2);
    assert_eq!(table["summary"]["choices_recent_without_reason"], 1);
    assert_eq!(
        table["choices"][0],
        json!({"run_id":"R-D1","machine":"alpha","task_id":"T-D1","task_title":"dev one","attempt":1,"role":"implementer","started_at":recent1,"profile":"claude-a","model":"sonnet","effort_requested":"high","effort":"high","task_type":"code","difficulty":"high","risk":"normal","previous_run":null,"reason":"codex 額度剩 5%","status":"completed"})
    );
}

#[test]
fn a_finished_spec_review_is_not_waiting_for_a_review() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    machine(&f, "alpha");
    let page = report_page(&f);
    assert!(choice_part(&page).contains("<p class=\"empty\">還沒有派工。</p>"));

    let conn = f.state_conn();
    let ts = "2026-03-01T00:00:00Z";
    insert_task(&conn, "T-SR", "spec under review", "draft", "normal", ts);
    insert_run(&conn, "R-SR", "T-SR", 1, "spec_reviewer", "claude-a", ts, Some(ts));
    set_launch(&conn, "R-SR", "opus", Some("high"));
    let page = report_page(&f);
    let part = choice_part(&page);
    assert_eq!(part.matches("<tr class=\"dev\">").count(), 1, "{part}");
    assert!(part.contains("<td>claude-a opus high</td>"), "{part}");
    assert!(part.contains("<td>沒寫</td><td>做完</td>"), "{part}");
    assert!(!part.contains("還沒審"), "{part}");
}

#[test]
fn thirty_choices_are_listed_and_the_rest_counted() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    let conn = f.state_conn();
    let base = OffsetDateTime::parse("2026-03-01T00:00:00Z", &Rfc3339).unwrap();
    for i in 0..32 {
        let started = rfc3339(base + Duration::minutes(i));
        let task = format!("T-X{i:02}");
        let run = format!("R-X{i:02}");
        insert_task(&conn, &task, &format!("deviation {i:02}"), "in_review", "normal", &started);
        insert_run(&conn, &run, &task, 1, "implementer", "claude-a", &started, None);
        set_launch(&conn, &run, "sonnet", Some("high"));
        insert_event(
            &conn,
            &run,
            "launch_choice",
            choice(Some(("claude-a", "haiku", "low")), ("claude-a", "sonnet", Some("high")), true, None, Some("why")),
            &started,
        );
    }

    let page = report_page(&f);
    assert_eq!(page.matches("<tr class=\"dev\">").count(), 30);
    assert!(page.contains("另外還有 2 次沒有列出"));
    let part = choice_part(&page);
    assert!(part.contains("deviation 31"));
    assert!(!part.contains("deviation 00"));
    assert!(!part.contains("deviation 01"));
    let v = models_json(&f);
    assert_eq!(v["decision_table"]["choices"].as_array().unwrap().len(), 32);
}

#[test]
fn score_dates_staleness_and_the_refresh_switch_are_shown() {
    let f = common::fixture();
    init(&f);
    let page = report_page(&f);
    let asl = para(&page, "scores-asl");
    assert!(asl.contains("沒有跑分資料"));
    assert!(asl.contains("過期"));
    assert_eq!(
        para(&page, "scores-refresh"),
        "<p class=\"note\" id=\"scores-refresh\">跑分自動更新：關（thoth config set scores-auto-refresh on）</p>"
    );
    assert!(!page.contains("http://"));
    assert!(!page.contains("https://"));

    let now = OffsetDateTime::now_utc();
    fs::write(
        f.home_path().join("model-scores.json"),
        json!({"schema": "aistupidlevel-v1", "fetched_at": rfc3339(now - Duration::hours(72)), "models": {"success": true, "data": []}}).to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        json!({"schema": "artificialanalysis-v2-free", "fetched_at": rfc3339(now - Duration::hours(1)), "data": []}).to_string(),
    )
    .unwrap();
    f.cmd()
        .args(["config", "set", "scores-auto-refresh", "on", "--json"])
        .assert()
        .success();
    let page = report_page(&f);
    let asl = para(&page, "scores-asl");
    assert!(asl.contains("取得於"));
    assert!(asl.contains("（72 小時前）"));
    assert!(asl.contains("過期"));
    assert!(asl.contains("href=\"https://aistupidlevel.info\""));
    let aa = para(&page, "scores-aa");
    assert!(aa.contains("取得於"));
    assert!(aa.contains("（1 小時前）"));
    assert!(!aa.contains("過期"));
    assert!(para(&page, "scores-refresh").contains("跑分自動更新：開"));
}

#[cfg(target_os = "linux")]
#[test]
fn report_open_section_builds_a_file_url() {
    let f = common::fixture();
    init(&f);
    let empty = tempfile::tempdir().unwrap();

    let out = f
        .cmd()
        .env("PATH", empty.path())
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
        .args(["report", "--open", "--section", "decision-table"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains("report.html#decision-table"), "{stderr}");
    assert!(stderr.contains("file://"), "{stderr}");

    let out = f
        .cmd()
        .env("PATH", empty.path())
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
        .args(["report", "--section", "decision-table"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = f
        .cmd()
        .env("PATH", empty.path())
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
        .args(["report", "--open", "--section", "Decision Table"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--section"));
    assert!(f.home_path().join("report.html").is_file());
}

#[test]
fn the_runner_written_page_also_has_the_table() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-b");
    let spec_file = f.home_path().join("table-spec.json");
    fs::write(&spec_file, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec_file)
        .args(["--title", "runner table title", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let task_id = serde_json::from_slice::<Value>(&out).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    report_json(&f);

    f.dispatch(&task_id, "claude-b", "ok", &[]);
    let (_, status) = f.wait_for(&task_id, "60s");
    assert_eq!(status, "in_review");

    let path = f.home_path().join("report.html");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let page = fs::read_to_string(&path).unwrap_or_default();
        if page.contains("<h2>派工決策表</h2>") && page.contains("<td>sonnet") {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("report.html never showed the table row\n{page}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
