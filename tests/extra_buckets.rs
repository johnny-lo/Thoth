mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::fs;

const FAR_FUTURE: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z

fn parse(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("valid json")
}

fn token_count_line(ts: &str, rate_limits: Value) -> String {
    json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": rate_limits}})
        .to_string()
        + "\n"
}

/// The codex-cli 0.154 shape: the general bucket, then two lines of a
/// per-model reserve bucket, the second newer than the first.
fn write_reserve_rollout(f: &Fixture, id: &str, general_used: f64, reserve_used: f64) {
    let cfg = f.add_codex_profile(id);
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let text = token_count_line("2026-09-28T04:54:11Z", json!({
        "limit_id": "codex", "limit_name": null,
        "primary": {"used_percent": general_used, "window_minutes": 300, "resets_at": FAR_FUTURE},
        "secondary": {"used_percent": 8.0, "window_minutes": 10080, "resets_at": FAR_FUTURE}
    })) + &token_count_line("2026-09-28T05:00:00Z", json!({
        "limit_id": "base_model_inference", "limit_name": "gpt-reserve",
        "primary": {"used_percent": 1.0, "window_minutes": 10080, "resets_at": FAR_FUTURE},
        "secondary": null
    })) + &token_count_line("2026-09-28T05:26:44Z", json!({
        "limit_id": "base_model_inference", "limit_name": "gpt-reserve",
        "primary": {"used_percent": reserve_used, "window_minutes": 10080, "resets_at": FAR_FUTURE},
        "secondary": null
    }));
    fs::write(sessions.join("rollout.jsonl"), text).unwrap();
}

fn usage(f: &Fixture, id: &str) -> Value {
    let out = f
        .cmd()
        .args(["profile", "usage", id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    parse(&out)["profiles"][0].clone()
}

#[test]
fn extra_buckets_hold_the_reserve_bucket_and_windows_only_the_general_one() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    write_reserve_rollout(&f, "codex-a", 16.0, 42.0);

    let p = usage(&f, "codex-a");
    let windows = p["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2, "{p}");
    assert_eq!(windows[0]["used_percent"], 16.0);
    assert_eq!(windows[1]["used_percent"], 8.0);

    let extra = p["extra_buckets"].as_array().expect("extra_buckets array");
    assert_eq!(extra.len(), 1, "{p}");
    assert_eq!(extra[0]["limit_id"], "base_model_inference");
    assert_eq!(extra[0]["limit_name"], "gpt-reserve");
    let ew = extra[0]["windows"].as_array().unwrap();
    assert_eq!(ew.len(), 1, "{p}");
    assert_eq!(ew[0]["name"], "seven_day");
    assert_eq!(ew[0]["used_percent"], 42.0);
    assert_eq!(ew[0]["observed_at"], "2026-09-28T05:26:44Z");
    assert_eq!(ew[0]["resets_at"], "2100-01-01T00:00:00Z");
    assert_eq!(ew[0]["source"], "codex_rollout");
    assert_eq!(ew[0]["reset_since_observed"], false);

    let out = f
        .cmd()
        .args(["profile", "usage", "codex-a"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("base_model_inference seven_day: 42.0%"), "{text}");
}

#[test]
fn a_rollout_without_limit_ids_has_no_extra_buckets_and_claude_always_empty() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-old");
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        token_count_line("2026-09-28T00:00:00Z", json!({
            "primary": {"used_percent": 30.0, "window_minutes": 300, "resets_at": FAR_FUTURE}
        })),
    )
    .unwrap();
    f.add_profile("claude-b");

    let p = usage(&f, "codex-old");
    assert_eq!(p["windows"].as_array().unwrap().len(), 1);
    assert_eq!(p["extra_buckets"], json!([]));
    let p = usage(&f, "claude-b");
    assert_eq!(p["extra_buckets"], json!([]));
}

#[test]
fn recommend_ignores_a_full_reserve_bucket() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    write_reserve_rollout(&f, "codex-a", 10.0, 100.0);

    let out = f
        .cmd()
        .args(["recommend", "--role", "implementer", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = parse(&out);
    // RJ-1: no `recommended` key; the first eligible candidate instead
    let first_eligible = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["eligible"] == true)
        .map(|c| c["profile_id"].clone());
    assert_eq!(first_eligible, Some(json!("codex-a")), "{v}");
    assert_eq!(v["candidates"][0]["eligible"], true, "{v}");
}

#[test]
fn report_shows_extra_buckets_muted_and_only_for_accounts_that_have_them() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    write_reserve_rollout(&f, "codex-a", 16.0, 42.0);
    f.add_profile("claude-b");

    let out = f
        .cmd()
        .args(["report", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let page = fs::read_to_string(parse(&out)["path"].as_str().unwrap()).unwrap();
    assert!(page.contains(".extra {"), "no .extra rule in STYLE");

    let accounts: Vec<&str> = page.split("<div class=\"acct\">").skip(1).collect();
    let codex = accounts
        .iter()
        .find(|a| a.contains("codex-a"))
        .expect("codex-a account block");
    let claude = accounts
        .iter()
        .find(|a| a.contains("claude-b"))
        .expect("claude-b account block");

    let extra_item = codex
        .split("<li class=\"extra\">")
        .nth(1)
        .expect("an li with class extra");
    let extra_item = extra_item.split("</li>").next().unwrap();
    assert!(extra_item.contains("gpt-reserve"), "{extra_item}");
    assert!(extra_item.contains("42%"), "{extra_item}");
    assert!(!claude.contains("class=\"extra\""), "{claude}");
}
