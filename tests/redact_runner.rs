//! RD-1 (RD-1c): what the runner and dispatch hand a worker or reviewer -
//! the prompt, a spec reviewer's input files and the handoff file - leaves
//! masked, and a spec edit that copies a mask marker back is refused. Every
//! fake secret is built by concatenation, so no full key format appears in
//! this file.

mod common;

use common::Fixture;
use std::path::PathBuf;

/// The exact value the tests put in `RD_TEST_TOKEN`, on single commands only.
const RD_VALUE: &str = "dummy-rd-env-secret-1";
/// The exact value the tests put in `RD_TEST_PASSWORD`: it needs JSON escapes.
const RD_PASSWORD: &str = "dummy&<pass\"word\\1";
const ALNUM36: &str = "dummyDUMMY0123456789dummyDUMMY012345";
const CODEX_MODEL: &str = "gpt-5.6-luna";

fn github_token() -> String {
    format!("{}{ALNUM36}", ["gh", "p_"].concat())
}

fn openai_key() -> String {
    format!("{}{}", ["s", "k-"].concat(), "dummyproj0123456789dummy")
}

/// A private key BEGIN line with no END line after it.
fn key_header() -> String {
    ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat()
}

/// `f.cmd()` without any variable of the test process whose name holds KEY,
/// TOKEN, SECRET or PASSWORD (only the names are read), so the host's own
/// values never take part.
fn clean(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

/// init, a claude profile, a two-criterion task, and the `hooks` fake
/// worker's candidate at in_review. Returns (task_id, implementer run_id).
fn candidate(f: &Fixture) -> (String, String) {
    clean(f).arg("init").assert().success();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let out = clean(f)
        .env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    (task, run)
}

fn set_label(f: &Fixture, task: &str, code: &str, text: &str) {
    f.state_conn()
        .execute(
            "UPDATE evidence SET label = ?1 WHERE task_id = ?2 AND code = ?3",
            [text, task, code],
        )
        .unwrap();
}

/// `text` holds neither `secret` nor any 8 consecutive characters of it
/// from `prefix_len` on.
fn no_secret(text: &str, secret: &str, prefix_len: usize) {
    assert!(!text.contains(secret), "the whole secret is in the output");
    let tail = &secret[prefix_len..];
    for i in 0..=tail.len().saturating_sub(8) {
        let piece = &tail[i..(i + 8).min(tail.len())];
        assert!(!text.contains(piece), "8 characters of a secret, from offset {}, are in the output", prefix_len + i);
    }
}

/// `RD_PASSWORD` in its JSON-escaped form, without the quotes.
fn password_escaped() -> String {
    let quoted = serde_json::to_string(RD_PASSWORD).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

/// Neither form of `RD_PASSWORD`, nor 8 characters of either.
fn no_password(text: &str) {
    no_secret(text, RD_PASSWORD, 0);
    no_secret(text, &password_escaped(), 0);
}

/// The first command evidence row's code.
fn first_command_code(f: &Fixture, task: &str) -> String {
    f.evidence_rows(task)
        .into_iter()
        .find(|r| r["kind"] == "command")
        .expect("a command row")["code"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Replaces the task's stored spec.
fn set_spec(f: &Fixture, task: &str, spec: &str) {
    f.state_conn()
        .execute("UPDATE tasks SET spec = ?1 WHERE task_id = ?2", [spec, task])
        .unwrap();
}

/// `base` (a spec) with the openai test key appended to its goal.
fn spec_with_key_in_goal(base: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(base).unwrap();
    let goal = format!("{} {}", v["goal"].as_str().unwrap(), openai_key());
    v["goal"] = serde_json::Value::String(goal);
    serde_json::to_string(&v).unwrap()
}

fn json_of(out: Vec<u8>) -> serde_json::Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `wait --run RUN` until it settles.
fn wait_run(f: &Fixture, run: &str) {
    clean(f)
        .args(["wait", "--run", run, "--timeout", "180s", "--json"])
        .assert()
        .success();
}

fn run_dir(f: &Fixture, task: &str, run: &str) -> PathBuf {
    PathBuf::from(f.run_of(task, run)["run_dir"].as_str().unwrap())
}

fn prompt_of(f: &Fixture, task: &str, run: &str) -> String {
    std::fs::read_to_string(run_dir(f, task, run).join("prompt.txt")).unwrap()
}

/// The `secret_redacted` events of run `run` with exit `exit`.
fn redactions(f: &Fixture, task: &str, run: &str, exit: &str) -> Vec<serde_json::Value> {
    f.events_json(task)
        .into_iter()
        .filter(|e| e["type"] == "secret_redacted" && e["run_id"] == run && e["detail"]["exit"] == exit)
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// AC6 (t1): a Claude Code reviewer's prompt and a codex reviewer's prompt
/// carry markers instead of secrets; a clean prompt is not touched.
#[test]
fn redact_a_claude_reviewers_prompt_carries_no_secret() {
    let f = common::fixture();
    let (task, implementer) = candidate(&f);
    assert!(!prompt_of(&f, &task, &implementer).contains("[redacted:"));
    assert!(
        !f.events_json(&task).iter().any(|e| e["type"] == "secret_redacted"),
        "nothing to mask, nothing recorded"
    );
    let code = first_command_code(&f, &task);
    set_label(&f, &task, &code, &format!("curl -H x {}", openai_key()));

    let copy = f.home_path().join("reviewer-prompt.txt");
    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_PROMPT_COPY", &copy)
        .args(["review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let review = json_of(out)["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &review);
    let received = std::fs::read_to_string(&copy).unwrap();
    assert_eq!(received, prompt_of(&f, &task, &review), "prompt.txt is what was sent");
    no_secret(&received, &openai_key(), 3);
    assert!(received.contains("[redacted:openai-key]"));
    assert_eq!(redactions(&f, &task, &review, "prompt").len(), 1);

    // a codex reviewer's prompt carries only the goal and the criteria
    let f = common::fixture();
    let (task, _) = candidate(&f);
    let code = first_command_code(&f, &task);
    set_label(&f, &task, &code, &format!("curl -H x {}", openai_key()));
    f.add_codex_profile("codex-a");
    set_spec(&f, &task, &spec_with_key_in_goal(Fixture::SPEC_TWO_AC));
    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task)
        .args(["review", "start", &task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let review = json_of(out)["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &review);
    let prompt = prompt_of(&f, &task, &review);
    no_secret(&prompt, &openai_key(), 3);
    assert!(prompt.contains("[redacted:openai-key]"));
}

/// AC6 (t2): a rework prompt whose finding holds a key header with no END
/// still has every section after the findings.
#[test]
fn redact_a_rework_prompt_survives_an_unterminated_key_header() {
    let f = common::fixture();
    let (task, _) = candidate(&f);
    let doc = serde_json::json!({
        "verdict": "REJECT",
        "findings": {
            "criteria": [
                {"id": "AC1", "verdict": "unmet", "note": format!("pushes with {}", github_token()), "evidence": ["E001"]},
                {"id": "AC2", "verdict": "met", "evidence": ["E001"]},
            ],
            "extra_findings": [
                {"severity": "minor", "note": format!("see {}", key_header()), "evidence": ["E001"]},
            ],
        },
    });
    let file = f.home_path().join("review-reject.json");
    std::fs::write(&file, serde_json::to_string(&doc).unwrap()).unwrap();
    clean(&f).args(["review", "record", &task, "-f"]).arg(&file).assert().success();

    let copy = f.home_path().join("rework-prompt.txt");
    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_PROMPT_COPY", &copy)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rework = json_of(out)["run_id"].as_str().unwrap().to_string();
    f.wait_for(&task, "180s");
    let prompt = std::fs::read_to_string(&copy).unwrap();
    assert!(prompt.contains("## Previous attempt was rejected"));
    assert!(prompt.contains("[redacted:github-token]"));
    let marker = prompt.find("[redacted:private-key]").expect("the key header is masked");
    let checks = prompt.find("## Checks the control plane will re-run").expect("the checks section");
    let done = prompt.find("When done, commit your work").expect("the result.json section");
    assert!(marker < checks && checks < done);
    no_secret(&prompt, &github_token(), 4);
    assert!(!prompt.contains("PRIVATE KEY"));
    assert_eq!(redactions(&f, &task, &rework, "prompt").len(), 1);
}

/// AC6 (t3): a handoff to codex masks `.agent/handoff.md` and its run-dir
/// copy, hashes what it wrote, and records the event right after
/// `handoff_written`.
#[test]
fn redact_a_handoff_to_another_provider_masks_the_handoff_file() {
    let f = common::fixture();
    clean(&f).arg("init").assert().success();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let out = clean(&f)
        .env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_HOOKS_THEN_QUOTA", "1")
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    json_of(out);
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    set_label(&f, &task, "E001", &format!("git push {}", github_token()));
    f.add_codex_profile("codex-a");
    set_spec(&f, &task, &spec_with_key_in_goal(Fixture::SPEC));

    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d = json_of(out);
    let run = d["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &run);
    let in_worktree = std::fs::read(PathBuf::from(d["worktree"].as_str().unwrap()).join(".agent").join("handoff.md")).unwrap();
    let in_run_dir = std::fs::read(run_dir(&f, &task, &run).join("handoff.md")).unwrap();
    assert_eq!(in_worktree, in_run_dir);
    let text = String::from_utf8(in_worktree.clone()).unwrap();
    no_secret(&text, &github_token(), 4);
    assert!(text.contains("[redacted:github-token]"));

    let events: Vec<serde_json::Value> =
        f.events_json(&task).into_iter().filter(|e| e["run_id"] == run.as_str()).collect();
    let written = events.iter().find(|e| e["type"] == "handoff_written").expect("handoff_written");
    assert_eq!(written["detail"]["sha256"], sha256_hex(&in_worktree));
    let masked = redactions(&f, &task, &run, "handoff");
    assert_eq!(masked.len(), 1);
    assert_eq!(masked[0]["seq"].as_i64().unwrap(), written["seq"].as_i64().unwrap() + 1);

    let prompt = prompt_of(&f, &task, &run);
    no_secret(&prompt, &github_token(), 4);
    no_secret(&prompt, &openai_key(), 3);
    assert!(prompt.contains("[redacted:openai-key]"));
}

/// AC6 (t4): a spec reviewer reads valid, masked JSON, while the review is
/// still bound to the stored spec's fingerprint.
#[test]
fn redact_spec_review_input_keeps_json_and_original_fingerprint() {
    let f = common::fixture();
    clean(&f).arg("init").assert().success();
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let spec = serde_json::to_string(&serde_json::json!({
        "goal": format!("say hello with {RD_PASSWORD} set"),
        "acceptance_criteria": ["AC1: prints hello"],
        "constraints": [format!("never print {}\nAAAAdummyKeyLine", key_header())],
        "non_goals": [],
        "excluded_alternatives": [],
        "risk": "low",
    }))
    .unwrap();
    set_spec(&f, &task, &spec);
    let sha = sha256_hex(spec.as_bytes());

    let out = clean(&f)
        .env("RD_TEST_PASSWORD", RD_PASSWORD)
        .env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r = json_of(out);
    let run = r["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &run);
    let input = std::fs::read_to_string(
        PathBuf::from(r["worktree"].as_str().unwrap()).join(".agent").join("spec-review-input").join("spec.json"),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&input).unwrap();
    assert!(v["goal"].as_str().unwrap().contains("[redacted:env:RD_TEST_PASSWORD]"));
    no_password(&input);
    assert!(!input.contains("PRIVATE KEY"));
    no_password(&prompt_of(&f, &task, &run));

    let show = json_of(
        clean(&f).args(["spec", "review", "show", &task, "--json"]).assert().success().get_output().stdout.clone(),
    );
    assert_eq!(show["spec_sha256"], sha.as_str());
    assert_eq!(show["newest"]["spec_sha256"], sha.as_str());
    let review = f
        .events_json(&task)
        .into_iter()
        .rfind(|e| e["type"] == "spec_review" && e["run_id"] == run.as_str())
        .expect("a spec_review event");
    assert_eq!(review["detail"]["spec_sha256"], sha.as_str());
    assert_eq!(review["detail"]["verdict"], "APPROVE");
    assert_eq!(redactions(&f, &task, &run, "spec_review_input").len(), 1);
}

/// AC6 (t5): an edit that copies a mask marker from the masked spec is
/// refused at stage `edits`, and the stored spec stays as it was.
#[test]
fn redact_a_spec_edit_carrying_a_marker_is_refused() {
    let f = common::fixture();
    clean(&f).arg("init").assert().success();
    f.add_codex_profile("codex-a");
    let spec = serde_json::to_string(&serde_json::json!({
        "goal": "say hello",
        "acceptance_criteria": [format!("AC1: prints {RD_VALUE}")],
        "constraints": [],
        "non_goals": [],
        "excluded_alternatives": [],
        "risk": "low",
    }))
    .unwrap();
    let (task, _) = f.create_task(&spec);
    let spec_of = || {
        json_of(clean(&f).args(["task", "show", &task, "--json"]).assert().success().get_output().stdout.clone())
            ["spec"]
            .clone()
    };
    let before = spec_of();

    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "spec_review_edit")
        .env("FAKE_TASK_ID", &task)
        .args([
            "spec", "review", "start", &task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium",
            "--edit", "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let run = json_of(out)["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &run);
    let events: Vec<serde_json::Value> =
        f.events_json(&task).into_iter().filter(|e| e["run_id"] == run.as_str()).collect();
    let invalid: Vec<&serde_json::Value> = events.iter().filter(|e| e["type"] == "spec_review_invalid").collect();
    assert_eq!(invalid.len(), 1, "{events:#?}");
    assert_eq!(invalid[0]["detail"]["stage"], "edits");
    let reason = invalid[0]["detail"]["reason"].as_str().unwrap();
    assert!(reason.starts_with("edit 0 (acceptance_criteria): "), "{reason}");
    assert!(reason.contains("carries a mask marker the stored spec does not have"), "{reason}");
    no_secret(reason, RD_VALUE, 0);
    assert!(!events.iter().any(|e| e["type"] == "spec_review"));
    assert_eq!(spec_of(), before);
}
