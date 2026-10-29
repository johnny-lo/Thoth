//! TIER-1: the subscription tier next to each account. Thoth stores the tier a
//! CLI reports (claude's `subscriptionType`, codex's `planType`) on the
//! profile row and writes the account as `claude-a (Max)` in what a person
//! reads; `--json` only gains keys. Always the fakes and a temporary
//! THOTH_HOME.
//!
//! The report helpers are copies of tests/report_table.rs's, the decision
//! table helpers copies of tests/decision_table.rs's, and `login` a copy of
//! tests/profile_login.rs's, because those files are not modules.

mod common;

use common::Fixture;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

const OLD: &str = "2000-01-01T00:00:00Z";

fn init(f: &Fixture) {
    f.cmd().arg("init").assert().success();
}

/// `plan` and `plan_seen_at` of one profile row.
fn plan_row(f: &Fixture, id: &str) -> (Option<String>, Option<String>) {
    f.global_conn()
        .query_row(
            "SELECT plan, plan_seen_at FROM profiles WHERE profile_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

fn set_plan(f: &Fixture, id: &str, plan: Option<&str>, seen: Option<&str>) {
    f.global_conn()
        .execute(
            "UPDATE profiles SET plan = ?2, plan_seen_at = ?3 WHERE profile_id = ?1",
            params![id, plan, seen],
        )
        .unwrap();
}

fn set_account_label(f: &Fixture, id: &str, label: &str) {
    f.global_conn()
        .execute(
            "UPDATE profiles SET account_label = ?2 WHERE profile_id = ?1",
            params![id, label],
        )
        .unwrap();
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// Exit code, stdout and stderr.
fn run(c: &mut assert_cmd::Command) -> (i32, String, String) {
    let out = c.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn parse(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
}

/// stdout of a command that must exit 0.
fn ok_text(f: &Fixture, c: assert_cmd::Command, what: &str) -> String {
    String::from_utf8(f.run_ok(c, what)).unwrap()
}

fn ok_json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    parse(&ok_text(f, c, &args.join(" ")))
}

/// The entry of `arr` whose `profile_id` is `id`.
fn entry<'a>(arr: &'a Value, id: &str) -> &'a Value {
    arr.as_array()
        .unwrap_or_else(|| panic!("not an array: {arr}"))
        .iter()
        .find(|p| p["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {arr}"))
}

fn list_json(f: &Fixture) -> Value {
    ok_json(f, &["profile", "list", "--json"])
}

fn list_text(f: &Fixture) -> String {
    let mut c = f.cmd();
    c.args(["profile", "list"]);
    ok_text(f, c, "profile list")
}

fn has_line_starting(text: &str, prefix: &str) -> bool {
    text.lines().any(|l| l.starts_with(prefix))
}

fn line_starting<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?}:\n{text}"))
}

fn line_containing<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?}:\n{text}"))
}

fn quota_rows(f: &Fixture, kind: Option<&str>) -> i64 {
    let conn = f.global_conn();
    match kind {
        None => conn.query_row("SELECT COUNT(*) FROM quota_events", [], |r| r.get(0)),
        Some(k) => conn.query_row("SELECT COUNT(*) FROM quota_events WHERE kind = ?1", [k], |r| r.get(0)),
    }
    .unwrap()
}

fn is_rfc3339_utc(s: &str) -> bool {
    OffsetDateTime::parse(s, &Rfc3339).is_ok() && s.ends_with('Z')
}

fn word(line: &str, n: usize) -> String {
    line.split_whitespace().nth(n).unwrap_or_else(|| panic!("no word {n} in {line:?}")).to_string()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn wait_run(f: &Fixture, run_id: &str) {
    let mut c = f.cmd();
    c.args(["wait", "--run", run_id, "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");
}

fn enable_live_quota(f: &Fixture) {
    f.cmd().args(["config", "set", "live-quota", "on"]).assert().success();
}

// --- copied from tests/profile_login.rs ----------------------------------------

fn login(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_LOGIN_FAIL")
        .args(["profile", "login"]);
    c
}

// --- copied from tests/report_table.rs -----------------------------------------

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

/// The every-launch part of the decision table section.
fn choice_part(page: &str) -> String {
    let start = page.find("<h3>每次的選擇與結果</h3>").expect("choices heading");
    let rest = &page[start..];
    rest[..rest.find("</section>").unwrap()].to_string()
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap()
}

// --- copied from tests/decision_table.rs ---------------------------------------

/// A task spec with the two labels, and two criteria for the fakes.
fn labelled_spec(task_type: &str, difficulty: &str) -> String {
    json!({"goal":"table","acceptance_criteria":["AC1: works","AC2: exits zero"],"constraints":[],
        "non_goals":[],"excluded_alternatives":[],"risk":"normal","task_type":task_type,
        "difficulty":difficulty})
    .to_string()
}

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
        params![task_id, json!({"task_type":task_type,"difficulty":difficulty}).to_string()],
    )
    .unwrap();
    let spec = match effort {
        Some(e) => json!({"launch":{"effort_requested":e,"effort":e}}).to_string(),
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
                params![run_id, json!({"points":p,"reliable":true}).to_string()],
            )
            .unwrap();
        }
        ids.push(run_id);
    }
    ids
}

/// The seeds of (c) and (j): sonnet on code/high, proven, medium backed.
fn seed_c(f: &Fixture) {
    seed_cell(f, "claude-a", "anthropic", "sonnet", Some("medium"), "code", "high", 5, 1, None);
    seed_cell(f, "claude-a", "anthropic", "sonnet", Some("high"), "code", "high", 3, 0, None);
    seed_cell(f, "claude-a", "anthropic", "sonnet", Some("low"), "code", "high", 2, 3, None);
}

fn create_labelled(f: &Fixture, task_type: &str, difficulty: &str) -> String {
    f.create_task(&labelled_spec(task_type, difficulty)).0
}

// --- the tests -----------------------------------------------------------------

#[test]
fn plan_login_stores_the_claude_plan_and_profile_list_shows_it() {
    let f = common::fixture();
    init(&f);
    let (code, stdout, stderr) = run(login(&f).args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{stdout}\n{stderr}");

    let (plan, seen) = plan_row(&f, "claude-a");
    assert_eq!(plan, some("pro"));
    let seen = seen.expect("plan_seen_at is set");
    assert!(is_rfc3339_utc(&seen), "{seen}");

    let v = list_json(&f);
    let a = entry(&v["profiles"], "claude-a");
    assert_eq!(a["plan"], "pro", "{v}");
    assert_eq!(a["plan_label"], "Pro", "{v}");
    assert_eq!(a["plan_seen_at"], seen.as_str(), "{v}");
    assert_eq!(a["account_label"], "pro", "{v}");
    assert_eq!(a["profile_id"], "claude-a", "{v}");

    let text = list_text(&f);
    assert!(has_line_starting(&text, "claude-a (Pro) "), "{text}");
    let mut c = f.cmd();
    c.env("THOTH_LANG", "zh-TW").args(["profile", "list"]);
    let zh = ok_text(&f, c, "profile list");
    assert!(has_line_starting(&zh, "claude-a (Pro) "), "{zh}");

    assert_eq!(quota_rows(&f, None), 1);
}

#[test]
fn plan_check_overwrites_a_seen_plan_and_keeps_it_when_none_is_seen() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    set_plan(&f, "claude-a", Some("max"), Some(OLD));
    let logout = f.home_path().join("logout.marker");
    fs::write(&logout, "x").unwrap();
    let auth_fail = f.home_path().join("auth-fail.marker");
    fs::write(&auth_fail, "x").unwrap();

    let check = |k: &str, v: &str| -> i32 {
        let mut c = f.cmd();
        c.env(k, v).args(["profile", "check", "claude-a", "--json"]);
        run(&mut c).0
    };
    let kept = (some("max"), some(OLD));
    assert_eq!(check("FAKE_SUBSCRIPTION_TYPE", "none"), 0);
    assert_eq!(plan_row(&f, "claude-a"), kept);
    assert_eq!(check("FAKE_SUBSCRIPTION_TYPE", "max ultra"), 0);
    assert_eq!(plan_row(&f, "claude-a"), kept);
    assert_eq!(check("FAKE_SUBSCRIPTION_TYPE", "ultra/max"), 0);
    assert_eq!(plan_row(&f, "claude-a"), kept);
    assert_eq!(check("FAKE_SUBSCRIPTION_TYPE", "   "), 0);
    assert_eq!(plan_row(&f, "claude-a"), kept);
    assert_eq!(check("FAKE_LOGOUT_MARKER", logout.to_str().unwrap()), 1);
    assert_eq!(plan_row(&f, "claude-a"), kept);
    assert_ne!(check("FAKE_AUTH_FAIL_MARKER", auth_fail.to_str().unwrap()), 0);
    assert_eq!(plan_row(&f, "claude-a"), kept);

    let mut c = f.cmd();
    c.args(["profile", "check", "claude-a", "--json"]);
    assert_eq!(run(&mut c).0, 0);
    let (plan, seen) = plan_row(&f, "claude-a");
    assert_eq!(plan, some("pro"));
    assert_ne!(seen, some(OLD));

    assert_eq!(check("FAKE_SUBSCRIPTION_TYPE", " Team "), 0);
    assert_eq!(plan_row(&f, "claude-a").0, some("team"));
    let v = list_json(&f);
    assert_eq!(entry(&v["profiles"], "claude-a")["plan_label"], "Team", "{v}");
}

#[test]
fn plan_codex_refresh_stores_the_plan_type_and_keeps_it_when_absent() {
    let f = common::fixture();
    init(&f);
    f.add_codex_profile("codex-a");
    enable_live_quota(&f);

    let v = list_json(&f);
    let a = entry(&v["profiles"], "codex-a");
    assert_eq!(a["plan"], Value::Null, "{v}");
    assert_eq!(a["plan_label"], Value::Null, "{v}");

    let refresh = |env: Option<(&str, &str)>| -> Value {
        let mut c = f.cmd();
        c.args(["profile", "usage", "--refresh", "--json"]);
        if let Some((k, val)) = env {
            c.env(k, val);
        }
        parse(&ok_text(&f, c, "profile usage --refresh"))
    };
    let v = refresh(None);
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    let p = entry(&v["profiles"], "codex-a");
    assert_eq!(p["plan"], "plus", "{v}");
    assert_eq!(p["plan_label"], "Plus", "{v}");
    assert_eq!(plan_row(&f, "codex-a").0, some("plus"));

    let detail: String = f
        .global_conn()
        .query_row(
            "SELECT detail FROM quota_events WHERE kind='live_quota' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let parsed: Value = serde_json::from_str(&detail).unwrap();
    let keys: BTreeSet<&str> = parsed.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, BTreeSet::from(["source", "windows", "ordinary_usage_allowed"]));
    assert!(!detail.contains("plus"), "{detail}");

    let mut c = f.cmd();
    c.args(["profile", "usage"]);
    let text = ok_text(&f, c, "profile usage");
    assert!(has_line_starting(&text, "codex-a (Plus)  openai / codex  status=active"), "{text}");

    set_plan(&f, "codex-a", Some("plus"), Some(OLD));
    let kept = (some("plus"), some(OLD));
    let v = refresh(Some(("FAKE_CODEX_APP_SERVER", "error")));
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "failed", "{v}");
    assert_eq!(plan_row(&f, "codex-a"), kept);
    let v = refresh(Some(("FAKE_CODEX_PLAN_TYPE", "none")));
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    assert_eq!(plan_row(&f, "codex-a"), kept);
    let v = refresh(Some(("FAKE_CODEX_PLAN_TYPE", "pro plan")));
    assert_eq!(entry(&v["refresh"], "codex-a")["status"], "refreshed", "{v}");
    assert_eq!(plan_row(&f, "codex-a"), kept);

    refresh(Some(("FAKE_CODEX_PLAN_TYPE", "pro")));
    let (plan, seen) = plan_row(&f, "codex-a");
    assert_eq!(plan, some("pro"));
    assert_ne!(seen, some(OLD));

    let mut c = f.cmd();
    c.args(["profile", "usage", "--refresh"]);
    let text = ok_text(&f, c, "profile usage --refresh");
    assert_eq!(text.trim().lines().last(), Some("refresh codex-a: refreshed"), "{text}");
}

#[test]
fn plan_label_falls_back_to_the_account_label_and_ids_stay_bare() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-b");
    let agy_dir = f.home_path().join("profiles").join("agy-a");
    fs::create_dir_all(&agy_dir).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&agy_dir)
        .args(["--label", " work laptop "])
        .assert()
        .success();

    let v = list_json(&f);
    let agy = entry(&v["profiles"], "agy-a");
    assert_eq!(agy["plan"], Value::Null, "{v}");
    assert_eq!(agy["plan_label"], "work laptop", "{v}");
    assert_eq!(agy["account_label"], " work laptop ", "{v}");
    let b = entry(&v["profiles"], "claude-b");
    assert_eq!(b["plan"], Value::Null, "{v}");
    assert_eq!(b["plan_label"], Value::Null, "{v}");

    let text = list_text(&f);
    let agy_line = line_starting(&text, "agy-a (work laptop) ");
    let b_line = line_starting(&text, "claude-b");
    assert!(b_line["claude-b".len()..].trim_start().starts_with("anthropic"), "{text}");
    assert_eq!(agy_line.find("google"), b_line.find("anthropic"), "{text}");

    let v = ok_json(&f, &["recommend", "--role", "implementer", "--json"]);
    assert_eq!(entry(&v["candidates"], "agy-a")["plan_label"], "work laptop", "{v}");
    assert_eq!(entry(&v["candidates"], "claude-b")["plan_label"], Value::Null, "{v}");
    let ids: BTreeSet<&str> = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["profile_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, BTreeSet::from(["agy-a", "claude-b"]));
    // RJ-1: no `recommended` key; the first eligible candidate, if any, is
    // one of the bare ids
    let first_eligible = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["eligible"] == true)
        .map(|c| c["profile_id"].clone());
    assert!(
        first_eligible.is_none()
            || first_eligible == Some(json!("agy-a"))
            || first_eligible == Some(json!("claude-b")),
        "{v}"
    );

    set_plan(&f, "agy-a", Some("ultra"), Some(OLD));
    let v = list_json(&f);
    assert_eq!(entry(&v["profiles"], "agy-a")["plan_label"], "Ultra", "{v}");
    let v = ok_json(&f, &["profile", "usage", "--json"]);
    assert_eq!(entry(&v["profiles"], "agy-a")["plan"], "ultra", "{v}");
    assert_eq!(entry(&v["profiles"], "agy-a")["plan_label"], "Ultra", "{v}");

    set_account_label(&f, "claude-b", "   ");
    let v = list_json(&f);
    assert_eq!(entry(&v["profiles"], "claude-b")["plan_label"], Value::Null, "{v}");

    set_account_label(&f, "claude-b", "lap\u{1b}top");
    let v = list_json(&f);
    assert_eq!(entry(&v["profiles"], "claude-b")["plan_label"], "laptop", "{v}");
    let text = list_text(&f);
    assert!(has_line_starting(&text, "claude-b (laptop) "), "{text}");
    assert!(!text.contains('\u{1b}'), "{text:?}");

    set_account_label(&f, "claude-b", &"x".repeat(40));
    let v = list_json(&f);
    let cut = format!("{}…", "x".repeat(32));
    assert_eq!(entry(&v["profiles"], "claude-b")["plan_label"], cut.as_str(), "{v}");
    let text = list_text(&f);
    assert!(has_line_starting(&text, &format!("claude-b ({cut}) ")), "{text}");
}

#[test]
fn plan_dispatch_review_and_spec_review_store_and_show_the_plan() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-b");
    f.add_profile("claude-a");

    // dispatch
    let (t1, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &t1)
        .args(["dispatch", &t1, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"]);
    let out = ok_text(&f, c, "dispatch");
    let line = first_line(&out);
    assert!(line.starts_with("dispatched "), "{out}");
    assert!(line.contains("profile claude-b (Pro), runner pid"), "{out}");
    let impl_run = word(line, 1);
    assert_eq!(plan_row(&f, "claude-b").0, some("pro"));
    assert_eq!(f.wait_for(&t1, "180s").1, "in_review");

    let mut c = f.cmd();
    c.args(["status", &t1]);
    let status = ok_text(&f, c, "status");
    assert!(line_containing(&status, &impl_run).contains("claude-b (Pro)"), "{status}");
    let v = ok_json(&f, &["status", &t1, "--json"]);
    for r in v["runs"].as_array().unwrap() {
        assert_eq!(r["profile"], "claude-b", "{v}");
    }

    // review start
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &t1)
        .args(["review", "start", &t1, "--profile", "claude-a", "--model", "haiku"]);
    let out = ok_text(&f, c, "review start");
    let line = first_line(&out);
    assert!(line.starts_with("review "), "{out}");
    assert!(line.contains("profile claude-a (Pro), runner pid"), "{out}");
    let review_run = word(line, 1);
    wait_run(&f, &review_run);
    assert_eq!(plan_row(&f, "claude-a").0, some("pro"));

    // spec review start, with the stored tier cleared first
    let (t2, _) = f.create_task(Fixture::SPEC);
    set_plan(&f, "claude-a", None, None);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &t2)
        .args(["spec", "review", "start", &t2, "--profile", "claude-a", "--model", "haiku"]);
    let out = ok_text(&f, c, "spec review start");
    let line = first_line(&out);
    assert!(line.starts_with("spec review "), "{out}");
    assert!(line.contains("profile claude-a (Pro), runner pid"), "{out}");
    let spec_run = word(line, 2);
    wait_run(&f, &spec_run);
    assert_eq!(plan_row(&f, "claude-a").0, some("pro"));

    // no tier seen: the stored one stays
    set_plan(&f, "claude-b", Some("max"), Some(OLD));
    let (t3, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &t3)
        .env("FAKE_SUBSCRIPTION_TYPE", "none")
        .args(["dispatch", &t3, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let v = parse(&ok_text(&f, c, "dispatch --json"));
    assert_eq!(v["profile"], "claude-b", "{v}");
    assert_eq!(plan_row(&f, "claude-b"), (some("max"), some(OLD)));
    f.wait_for(&t3, "180s");

    // the account column is padded to one width
    set_plan(&f, "claude-a", Some("enterprise"), Some(OLD));
    let mut c = f.cmd();
    c.args(["status", &t1]);
    let status = ok_text(&f, c, "status");
    let impl_line = line_containing(&status, &impl_run);
    let review_line = line_containing(&status, &review_run);
    assert!(impl_line.contains("claude-b (Max)"), "{status}");
    assert!(review_line.contains("claude-a (Enterprise)"), "{status}");
    assert_eq!(impl_line.find("sonnet"), review_line.find("haiku"), "{status}");
}

#[test]
fn plan_report_and_doctor_show_the_stored_plan() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    let mut c = f.cmd();
    c.args(["profile", "check", "claude-a"]);
    f.run_ok(c, "profile check");
    assert_eq!(plan_row(&f, "claude-a").0, some("pro"));
    let before = plan_row(&f, "claude-a");

    let now = OffsetDateTime::now_utc();
    let recent = rfc3339(now - Duration::hours(1));
    {
        let conn = f.state_conn();
        insert_task(&conn, "T-P", "plan pass", "pass", "normal", &recent);
        set_labels(&conn, "T-P", "code", "high");
        insert_run(&conn, "R-P", "T-P", 1, "implementer", "claude-a", &recent, Some(&recent));
        set_launch(&conn, "R-P", "sonnet", Some("high"));
        insert_review(&conn, "V-P", "R-P", "PASS", json!({"criteria": []}), &recent);
        insert_task(&conn, "T-D", "plan dev", "in_review", "normal", &recent);
        insert_run(&conn, "R-D", "T-D", 1, "implementer", "claude-a", &recent, None);
        set_launch(&conn, "R-D", "sonnet", Some("high"));
        insert_event(
            &conn,
            "R-D",
            "launch_choice",
            choice(
                Some(("codex-a", "gpt-6-astra", "medium")),
                ("claude-a", "sonnet", Some("high")),
                true,
                None,
                Some("codex busy"),
            ),
            &recent,
        );
    }
    let page = report_page(&f);
    // PANEL-3: the account block also names the login the check stored
    // (fake-claude's default email is fake@example.com); the decision table
    // does not.
    assert!(page.contains("<span class=\"acct-name\">claude-a（fake，Pro）</span>"), "{page}");
    assert!(page.contains("<td>sonnet</td><td>claude-a（Pro）</td>"), "{page}");
    // RJ-3: the choice cell names the profile with its plan; there is no
    // recommended column any more
    let part = choice_part(&page);
    assert!(part.contains("<td>claude-a（Pro） sonnet high</td>"), "{part}");

    let mut c = f.cmd();
    c.env("FAKE_SUBSCRIPTION_TYPE", "max").env_remove("THOTH_LANG").arg("doctor");
    let (_, en, _) = run(&mut c);
    // PANEL-3: the heading names the login doctor's own check saw.
    let line = line_starting(&en, "Profile claude-a (fake, Pro) … ");
    assert!(line.contains("max"), "{en}");
    let mut c = f.cmd();
    c.env("FAKE_SUBSCRIPTION_TYPE", "max").env("THOTH_LANG", "zh-TW").arg("doctor");
    let (_, zh, _) = run(&mut c);
    assert!(has_line_starting(&zh, "帳號 claude-a（fake，Pro） … "), "{zh}");
    let mut c = f.cmd();
    c.args(["doctor", "--json"]);
    let (_, doc, _) = run(&mut c);
    let doc = parse(&doc);
    assert!(
        doc["items"].as_array().unwrap().iter().any(|i| i["id"] == "profile:claude-a"),
        "{doc}"
    );
    assert_eq!(plan_row(&f, "claude-a"), before, "doctor never writes the tier");
}

#[test]
fn plan_recommend_launch_line_names_the_plan() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    seed_c(&f);
    let task = create_labelled(&f, "code", "high");
    set_plan(&f, "claude-a", Some("max"), Some(OLD));

    let mut c = f.cmd();
    c.args(["recommend", "--role", "implementer", "--task", &task]);
    let text = ok_text(&f, c, "recommend");
    // RJ-1: no `launch:` line; the candidate line names the plan
    assert!(!text.contains("launch:"), "{text}");
    assert!(has_line_starting(&text, "claude-a (Max)  anthropic / claude-code"), "{text}");

    let v = ok_json(&f, &["recommend", "--role", "implementer", "--task", &task, "--json"]);
    assert!(v.get("recommended_launch").is_none(), "{v}");
    let a = entry(&v["candidates"], "claude-a");
    assert_eq!(a["plan"], "max", "{v}");
    assert_eq!(a["plan_label"], "Max", "{v}");
}

#[test]
fn plan_an_old_global_db_is_read_without_migrating() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    let b_dir = f.home_path().join("profiles").join("claude-b");
    fs::create_dir_all(&b_dir).unwrap();
    f.cmd()
        .args(["profile", "add", "claude-b", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&b_dir)
        .args(["--label", "second"])
        .assert()
        .success();
    {
        let conn = f.global_conn();
        conn.execute_batch("ALTER TABLE profiles DROP COLUMN plan").unwrap();
        conn.execute_batch("ALTER TABLE profiles DROP COLUMN plan_seen_at").unwrap();
        conn.execute_batch("DELETE FROM schema_version WHERE version >= 3").unwrap();
        conn.execute_batch("INSERT INTO schema_version (version) VALUES (2)").unwrap();
    }
    let columns = |f: &Fixture| -> Vec<String> {
        let conn = f.global_conn();
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('profiles')").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap()
    };
    let version = |f: &Fixture| -> i64 {
        f.global_conn().query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0)).unwrap()
    };

    let v = ok_json(&f, &["profile", "usage", "--json"]);
    let b = entry(&v["profiles"], "claude-b");
    assert_eq!(b["plan"], Value::Null, "{v}");
    assert_eq!(b["plan_label"], "second", "{v}");
    let a = entry(&v["profiles"], "claude-a");
    assert_eq!(a["plan"], Value::Null, "{v}");
    assert_eq!(a["plan_label"], Value::Null, "{v}");

    let v = ok_json(&f, &["recommend", "--role", "implementer", "--json"]);
    assert_eq!(entry(&v["candidates"], "claude-b")["plan_label"], "second", "{v}");

    let page = report_page(&f);
    assert!(page.contains("<span class=\"acct-name\">claude-b（second）</span>"), "{page}");

    let mut c = f.cmd();
    c.args(["doctor", "--json"]);
    let (_, doc, _) = run(&mut c);
    parse(&doc);

    let cols = columns(&f);
    assert!(!cols.iter().any(|c| c == "plan"), "{cols:?}");
    assert_eq!(version(&f), 2);

    let v = list_json(&f);
    assert_eq!(entry(&v["profiles"], "claude-b")["plan_label"], "second", "{v}");
    let cols = columns(&f);
    assert!(cols.iter().any(|c| c == "plan") && cols.iter().any(|c| c == "plan_seen_at"), "{cols:?}");
    // MODELS-1 made it v4; PANEL-3 (login names) makes the migration end at v5.
    assert_eq!(version(&f), 5);
}

#[test]
fn plan_a_failed_plan_write_only_warns() {
    let f = common::fixture();
    init(&f);
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    enable_live_quota(&f);
    f.global_conn()
        .execute_batch(
            "CREATE TRIGGER block_plan BEFORE UPDATE OF plan ON profiles \
             BEGIN SELECT RAISE(ABORT, 'plan blocked'); END",
        )
        .unwrap();
    let warned = |stderr: &str, id: &str, needle: Option<&str>| {
        let prefix = format!("warning: could not store the plan of {id}: ");
        stderr.lines().any(|l| l.starts_with(&prefix) && needle.is_none_or(|n| l.contains(n)))
    };

    // (a) profile check
    let mut c = f.cmd();
    c.args(["profile", "check", "claude-a", "--json"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(parse(&stdout)["logged_in"], true);
    assert!(warned(&stderr, "claude-a", Some("plan blocked")), "{stderr}");
    assert_eq!(plan_row(&f, "claude-a"), (None, None));

    // (b) profile usage --refresh
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_USAGE", "exit").args(["profile", "usage", "--refresh", "--json"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let v = parse(&stdout);
    let r = entry(&v["refresh"], "codex-a");
    assert_eq!(r["status"], "refreshed", "{v}");
    assert_eq!(r["error"], Value::Null, "{v}");
    assert_eq!(quota_rows(&f, Some("live_quota")), 1);
    assert!(warned(&stderr, "codex-a", None), "{stderr}");
    assert_eq!(plan_row(&f, "codex-a"), (None, None));

    // (c) dispatch
    let (t, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &t)
        .args(["dispatch", &t, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--json"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(parse(&stdout)["ok"], true, "{stdout}");
    assert!(warned(&stderr, "claude-a", None), "{stderr}");
    f.wait_for(&t, "180s");
}
