//! LESS-3 (H3): `checks add` reminds about `cargo test --no-fail-fast`
//! without refusing anything.

mod common;

use serde_json::{json, Value};

const HINT: &str = "cargo test stops at the first test target that fails, so later targets and their new tests never run; consider cargo test --no-fail-fast";

fn add_json(f: &common::Fixture, id: &str, cmd: &str) -> Value {
    let out = f
        .cmd()
        .args(["checks", "add", id, "--cmd", cmd, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

#[test]
fn checks_hint_names_no_fail_fast_only_for_cargo_test() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    for (i, cmd) in ["cargo test", "cargo test --workspace", "cargo test -- --no-fail-fast"].iter().enumerate() {
        let v = add_json(&f, &format!("h{i}"), cmd);
        assert_eq!(v["hints"], json!([HINT]), "{cmd}");
        assert!(v["hints"][0].as_str().unwrap().contains("--no-fail-fast"));
        assert_eq!(v["status"], "pending", "{cmd}");
    }
    for (i, cmd) in ["cargo test --no-fail-fast", "cargo build", "git --version"].iter().enumerate() {
        let v = add_json(&f, &format!("n{i}"), cmd);
        assert_eq!(v["hints"], json!([]), "{cmd}");
        assert_eq!(v["status"], "pending", "{cmd}");
    }

    let out = f
        .cmd()
        .args(["checks", "add", "human", "--cmd", "cargo test"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("hint: {HINT}")), "{text}");
    assert!(text.contains("hint: cargo test stops"), "{text}");

    let list = f.cmd().args(["checks", "list", "--json"]).assert().success().get_output().stdout.clone();
    let list: Value = serde_json::from_str(String::from_utf8(list).unwrap().trim()).unwrap();
    for c in list["checks"].as_array().unwrap() {
        assert_eq!(c["status"], "pending", "{c}");
    }
}
