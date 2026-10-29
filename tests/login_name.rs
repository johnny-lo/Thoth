//! PANEL-3 AC1 and AC2: the login name (the part of the login email before
//! `@`) that the login checks store on a profile, and where a person reads
//! it: `thoth doctor`'s profile headings and the report page's account block.
//! `profile list --json` gains `login_name` and `login_name_seen_at`; nothing
//! else in `--json` changes, and the shared records never carry it.
//!
//! Every email here is made up (`FAKE_AUTH_EMAIL` sets the one fake-claude
//! reports). Always the fakes and a temporary THOTH_HOME; the database is
//! never opened: every row comes from the CLI.

mod common;

use common::Fixture;
use serde_json::Value;
use std::collections::BTreeSet;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The variable AC2's masking example sets, and its made-up value.
const SECRET_VAR: &str = "PANEL3_TEST_TOKEN";
const SECRET_VALUE: &str = "dummyLoginValue7";

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names only are read), without the two benchmark keys,
/// and in English.
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c.env_remove("AISTUPIDLEVEL_API_KEY")
        .env_remove("ARTIFICIALANALYSIS_API_KEY")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_AUTH_EMAIL");
    c
}

/// Exit code, stdout, stderr.
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

fn setup(ids: &[&str]) -> Fixture {
    let f = common::fixture();
    cmd(&f).arg("init").assert().success();
    for id in ids {
        let (provider, harness) = if id.starts_with("codex-") { ("openai", "codex") } else { ("anthropic", "claude-code") };
        let dir = f.home_path().join("profiles").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        cmd(&f)
            .args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"])
            .arg(&dir)
            .arg("--json")
            .assert()
            .success();
    }
    f
}

/// `profile check ID --json` with `vars` set; exit code and stdout.
fn check(f: &Fixture, id: &str, vars: &[(&str, &str)]) -> (i32, String) {
    let mut c = cmd(f);
    for (k, v) in vars {
        c.env(k, v);
    }
    c.args(["profile", "check", id, "--json"]);
    let (code, stdout, _) = run(&mut c);
    (code, stdout)
}

fn check_email(f: &Fixture, id: &str, email: &str) {
    let (code, stdout) = check(f, id, &[("FAKE_AUTH_EMAIL", email)]);
    assert_eq!(code, 0, "{stdout}");
}

/// `login_name` and `login_name_seen_at` of one profile in `profile list --json`.
fn stored(f: &Fixture, id: &str) -> (Value, Value) {
    let mut c = cmd(f);
    c.args(["profile", "list", "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "profile list")).unwrap());
    let p = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {v}"))
        .clone();
    let obj = p.as_object().unwrap();
    assert!(obj.contains_key("login_name") && obj.contains_key("login_name_seen_at"), "{p}");
    (p["login_name"].clone(), p["login_name_seen_at"].clone())
}

fn name(f: &Fixture, id: &str) -> Value {
    stored(f, id).0
}

fn is_rfc3339_utc(s: &str) -> bool {
    OffsetDateTime::parse(s, &Rfc3339).is_ok() && s.ends_with('Z')
}

fn line_starting<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?}:\n{text}"))
}

/// `thoth doctor` (any exit code) with `vars` set; stdout.
fn doctor(f: &Fixture, vars: &[(&str, &str)], json: bool) -> String {
    let mut c = cmd(f);
    for (k, v) in vars {
        c.env(k, v);
    }
    c.arg("doctor");
    if json {
        c.arg("--json");
    }
    run(&mut c).1
}

fn report_page(f: &Fixture, vars: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("status.html");
    let mut c = cmd(f);
    for (k, v) in vars {
        c.env(k, v);
    }
    c.args(["report", "--json", "--out"]).arg(&target);
    f.run_ok(c, "report --out");
    std::fs::read_to_string(&target).unwrap()
}

#[test]
fn login_name_check_stores_overwrites_and_keeps_the_old_one() {
    let f = setup(&["claude-a"]);
    assert_eq!(stored(&f, "claude-a"), (Value::Null, Value::Null));

    check_email(&f, "claude-a", "alice@example.org");
    let (n, seen) = stored(&f, "claude-a");
    assert_eq!(n, "alice");
    assert!(is_rfc3339_utc(seen.as_str().unwrap()), "{seen}");

    let (code, stdout) = check(&f, "claude-a", &[("FAKE_AUTH_EMAIL", "bob@example.org")]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(parse(&stdout)["email"], "bob@example.org", "profile check --json keeps the whole email");
    assert_eq!(name(&f, "claude-a"), "bob");
    let bob = stored(&f, "claude-a");

    // No email this time: the old name and its time stay.
    check_email(&f, "claude-a", "none");
    assert_eq!(stored(&f, "claude-a"), bob);

    // Logged out, or the check failed: the old name stays.
    let logout = f.home_path().join("logout.marker");
    std::fs::write(&logout, "x").unwrap();
    let (code, _) = check(&f, "claude-a", &[("FAKE_AUTH_EMAIL", "eve@example.org"), ("FAKE_LOGOUT_MARKER", logout.to_str().unwrap())]);
    assert_eq!(code, 1);
    assert_eq!(stored(&f, "claude-a"), bob);
    let fail = f.home_path().join("auth-fail.marker");
    std::fs::write(&fail, "x").unwrap();
    let (code, _) = check(&f, "claude-a", &[("FAKE_AUTH_EMAIL", "eve@example.org"), ("FAKE_AUTH_FAIL_MARKER", fail.to_str().unwrap())]);
    assert_ne!(code, 0);
    assert_eq!(stored(&f, "claude-a"), bob);
}

#[test]
fn login_name_takes_the_part_before_the_first_at() {
    let f = setup(&["claude-a", "claude-b", "claude-c", "claude-d", "claude-e"]);
    check_email(&f, "claude-a", "noatsign");
    assert_eq!(name(&f, "claude-a"), Value::Null);
    check_email(&f, "claude-b", "@example.org");
    assert_eq!(name(&f, "claude-b"), Value::Null);
    check_email(&f, "claude-c", &format!("{}@example.org", "x".repeat(40)));
    assert_eq!(name(&f, "claude-c"), "x".repeat(32));
    check_email(&f, "claude-d", "al\u{7}ice@example.org");
    assert_eq!(name(&f, "claude-d"), "alice");
    check_email(&f, "claude-e", "  carol@example.org  ");
    assert_eq!(name(&f, "claude-e"), "carol");

    // An email without a name keeps the one stored before.
    check_email(&f, "claude-d", "noatsign");
    assert_eq!(name(&f, "claude-d"), "alice");
}

#[test]
fn login_name_dispatch_stores_it_codex_has_none_and_records_never_carry_it() {
    let f = setup(&["claude-b", "codex-a"]);
    let spec = f.home_path().join("spec.json");
    std::fs::write(&spec, Fixture::SPEC).unwrap();
    let mut c = cmd(&f);
    c.args(["task", "create", "-f"]).arg(&spec).arg("--json");
    let task = parse(&String::from_utf8(f.run_ok(c, "task create")).unwrap())["task_id"].as_str().unwrap().to_string();

    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_AUTH_EMAIL", "dave@example.org")
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "dispatch")).unwrap());
    assert_eq!(v["profile"], "claude-b", "--json keeps the bare id: {v}");
    let run_id = v["run_id"].as_str().unwrap().to_string();
    assert_eq!(name(&f, "claude-b"), "dave");
    let mut c = cmd(&f);
    c.args(["wait", "--run", &run_id, "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");

    // codex reports no email: never a name.
    let _ = check(&f, "codex-a", &[]);
    assert_eq!(stored(&f, "codex-a"), (Value::Null, Value::Null));

    let dir = f.home_path().join("records");
    std::fs::create_dir_all(&dir).unwrap();
    cmd(&f).args(["config", "set", "records-dir"]).arg(&dir).arg("--json").assert().success();
    cmd(&f).args(["config", "set", "machine-name", "alpha", "--json"]).assert().success();
    let mut c = cmd(&f);
    c.args(["records", "export", "--json"]);
    let out = parse(&String::from_utf8(f.run_ok(c, "records export")).unwrap());
    assert!(out["lines"].as_u64().unwrap() > 0, "{out}");
    let text = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
    assert!(text.contains("claude-b"), "{text}");
    assert!(!text.contains("dave"), "{text}");
    assert!(!text.contains("login_name"), "{text}");
}

#[test]
fn login_name_doctor_heading_uses_its_own_check_then_the_stored_name() {
    let f = setup(&["claude-a"]);
    check_email(&f, "claude-a", "alice@example.org");

    let en = doctor(&f, &[("FAKE_AUTH_EMAIL", "alice@example.org")], false);
    line_starting(&en, "Profile claude-a (alice, Pro) … ");
    let zh = doctor(&f, &[("FAKE_AUTH_EMAIL", "alice@example.org"), ("THOTH_LANG", "zh-TW")], false);
    line_starting(&zh, "帳號 claude-a（alice，Pro） … ");

    // doctor's own check first; it never writes the name.
    let fresh = doctor(&f, &[("FAKE_AUTH_EMAIL", "carol@example.org")], false);
    line_starting(&fresh, "Profile claude-a (carol, Pro) … ");
    assert_eq!(name(&f, "claude-a"), "alice");
    // No email from its check: the stored name.
    let kept = doctor(&f, &[("FAKE_AUTH_EMAIL", "none")], false);
    line_starting(&kept, "Profile claude-a (alice, Pro) … ");

    // --json: the same keys in every item and in every item's data.
    let shape = |text: &str| -> Vec<(String, BTreeSet<String>, BTreeSet<String>)> {
        parse(text)["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let keys = i.as_object().unwrap().keys().cloned().collect();
                let data = i["data"].as_object().map(|d| d.keys().cloned().collect()).unwrap_or_default();
                (i["id"].as_str().unwrap().to_string(), keys, data)
            })
            .collect()
    };
    let with = doctor(&f, &[("FAKE_AUTH_EMAIL", "alice@example.org")], true);
    let without = doctor(&f, &[("FAKE_AUTH_EMAIL", "none")], true);
    assert_eq!(shape(&with), shape(&without));
    let item = parse(&with)["items"].as_array().unwrap().iter().find(|i| i["id"] == "profile:claude-a").cloned().unwrap();
    assert_eq!(item["data"]["email"], "alice@example.org", "{item}");
}

#[test]
fn login_name_report_page_account_block() {
    let f = setup(&["claude-a", "claude-b"]);
    check_email(&f, "claude-a", "alice@example.org");
    check_email(&f, "claude-b", "none");
    let page = report_page(&f, &[]);
    assert!(page.contains("<span class=\"acct-name\">claude-a（alice，Pro）</span>"), "{page}");
    assert!(page.contains("<span class=\"acct-name\">claude-b（Pro）</span>"), "{page}");
}

#[test]
fn login_name_equal_to_a_known_secret_is_masked() {
    let f = setup(&["claude-a"]);
    let email = format!("{SECRET_VALUE}@example.org");
    let (code, _) = check(&f, "claude-a", &[("FAKE_AUTH_EMAIL", &email), (SECRET_VAR, SECRET_VALUE)]);
    assert_eq!(code, 0);

    let masked = format!("[redacted:env:{SECRET_VAR}]");
    let out = doctor(&f, &[("FAKE_AUTH_EMAIL", "none"), (SECRET_VAR, SECRET_VALUE)], false);
    line_starting(&out, &format!("Profile claude-a ({masked}, Pro) … "));
    assert!(!out.contains(SECRET_VALUE), "{out}");

    let page = report_page(&f, &[(SECRET_VAR, SECRET_VALUE)]);
    assert!(page.contains(&format!("claude-a（{masked}，Pro）")), "{page}");
    assert!(!page.contains(SECRET_VALUE), "{page}");
}
