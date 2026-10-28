//! RD-1 (RD-1a): secrets leave masked through `evidence show`, `review
//! packet` and the review input a codex reviewer reads, while what is stored
//! stays as it was. Every fake secret is built by concatenation, so no full
//! key format appears in this file.

mod common;

use common::Fixture;
use std::path::{Path, PathBuf};

/// The exact value the tests put in `RD_TEST_TOKEN`, on single commands only.
const RD_VALUE: &str = "dummy-rd-env-secret-1";
const ALNUM36: &str = "dummyDUMMY0123456789dummyDUMMY012345";
const B64_LINES: [&str; 2] = [
    "QUFBQUFBQUFBQUFBQUFBQUFBdummyKeyLineOne",
    "QkJCQkJCQkJCQkJCQkJCQkJCdummyKeyLineTwo",
];

fn github_token() -> String {
    format!("{}{ALNUM36}", ["gh", "p_"].concat())
}

fn openai_key() -> String {
    format!("{}{}", ["s", "k-"].concat(), "dummyproj0123456789dummy")
}

/// A private key block: BEGIN line, `body` lines, END line, joined by `\n`.
fn private_key_block(body: &[String]) -> String {
    let mut lines = vec![["-----BEGIN ", "RSA PRIVATE KEY-----"].concat()];
    lines.extend(body.iter().cloned());
    lines.push(["-----END ", "RSA PRIVATE KEY-----"].concat());
    lines.join("\n")
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

/// The first command row with a stored blob.
fn command_row(f: &Fixture, task: &str) -> serde_json::Value {
    f.evidence_rows(task)
        .into_iter()
        .find(|r| r["kind"] == "command" && r["blob_path"].is_string())
        .expect("a command row with a blob")
}

fn code_of(row: &serde_json::Value) -> String {
    row["code"].as_str().unwrap().to_string()
}

fn blob_of(row: &serde_json::Value) -> PathBuf {
    PathBuf::from(row["blob_path"].as_str().unwrap())
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

/// `no_secret` for every secret these tests plant.
fn no_secrets(text: &str) {
    no_secret(text, &github_token(), 4);
    no_secret(text, &openai_key(), 3);
    no_secret(text, RD_VALUE, 0);
    for line in B64_LINES {
        no_secret(text, line, 0);
    }
}

fn secret_events(f: &Fixture, task: &str) -> Vec<serde_json::Value> {
    f.events_json(task)
        .into_iter()
        .filter(|e| e["type"] == "secret_redacted")
        .collect()
}

fn opened_count(f: &Fixture, task: &str, code: &str) -> usize {
    f.events_json(task)
        .iter()
        .filter(|e| e["type"] == "evidence_opened" && e["detail"]["code"] == code)
        .count()
}

fn kinds_of(e: &serde_json::Value) -> Vec<String> {
    e["detail"]["kinds"].as_object().unwrap().keys().cloned().collect()
}

/// (exit code, stdout, stderr) of a command.
fn run(mut c: assert_cmd::Command) -> (i32, String, String) {
    let out = c.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn show(f: &Fixture, task: &str, code: &str, extra: &[&str]) -> String {
    let mut c = clean(f);
    c.env("RD_TEST_TOKEN", RD_VALUE).args(["evidence", "show", code, "--task", task]).args(extra);
    let (status, stdout, stderr) = run(c);
    assert_eq!(status, 0, "evidence show failed: {stderr}");
    stdout
}

/// AC7 (t1): content is masked before it is cut, label too; the stored blob
/// and label are unchanged; one `secret_redacted` per show that masked.
#[test]
fn redact_evidence_show_masks_before_cutting_and_records_the_event() {
    let f = common::fixture();
    let (task, _) = candidate(&f);
    let row = command_row(&f, &task);
    let code = code_of(&row);
    let blob = blob_of(&row);
    let lines: Vec<String> = B64_LINES.iter().map(|s| s.to_string()).collect();
    let body = format!(
        "{} {}\nenv: {RD_VALUE}\n{}",
        "x".repeat(3989),
        github_token(),
        private_key_block(&lines)
    );
    std::fs::write(&blob, &body).unwrap();
    set_label(&f, &task, &code, &format!("curl -H x {}", openai_key()));
    let stored_blob = std::fs::read(&blob).unwrap();
    let label_of = || {
        f.evidence_rows(&task).into_iter().find(|r| r["code"] == code.as_str()).unwrap()["label"].clone()
    };
    let stored_label = label_of();

    let v: serde_json::Value = serde_json::from_str(show(&f, &task, &code, &["--json"]).trim()).unwrap();
    let content = v["content"].as_str().unwrap();
    let label = v["label"].as_str().unwrap();
    no_secrets(content);
    no_secrets(label);
    // masked first, then cut: the 4000 bytes end inside the marker
    assert!(content.ends_with(" [redacted:"), "cut at the wrong place");
    assert_eq!(v["content_truncated"], true);
    assert!(label.contains("[redacted:openai-key]"), "{label}");

    let v: serde_json::Value =
        serde_json::from_str(show(&f, &task, &code, &["--json", "--bytes", "1000000"]).trim()).unwrap();
    let content = v["content"].as_str().unwrap();
    for marker in ["[redacted:github-token]", "[redacted:env:RD_TEST_TOKEN]", "[redacted:private-key]"] {
        assert!(content.contains(marker), "{marker} missing");
    }
    for line in B64_LINES {
        assert!(!content.contains(line));
    }
    assert_eq!(v["content_truncated"], false);

    no_secrets(&show(&f, &task, &code, &[]));

    let events = secret_events(&f, &task);
    assert_eq!(events.len(), 3, "{events:#?}");
    for e in &events {
        assert_eq!(e["detail"]["exit"], "evidence_show");
        assert_eq!(e["detail"]["evidence"], serde_json::json!([code]));
        assert_eq!(kinds_of(e), ["env:RD_TEST_TOKEN", "github-token", "openai-key", "private-key"]);
        no_secrets(&e["detail"].to_string());
    }
    assert_eq!(opened_count(&f, &task, &code), 3);
    assert_eq!(std::fs::read(&blob).unwrap(), stored_blob, "the stored blob is unchanged");
    assert_eq!(label_of(), stored_label, "the stored label is unchanged");

    let other = f
        .evidence_rows(&task)
        .into_iter()
        .find(|r| r["code"] != code.as_str())
        .expect("a second evidence row");
    show(&f, &task, &code_of(&other), &[]);
    assert_eq!(secret_events(&f, &task).len(), 3, "a clean row writes no secret_redacted");
}

fn packet(f: &Fixture, task: &str, extra: &[String]) -> String {
    let mut c = clean(f);
    c.env("RD_TEST_TOKEN", RD_VALUE).args(["review", "packet", task]).args(extra);
    let (status, stdout, stderr) = run(c);
    assert_eq!(status, 0, "review packet failed: {stderr}");
    stdout
}

/// The 30 lines of `line-NN` whose lines 10 to 20 are a private key block.
fn thirty_lines() -> String {
    let mut lines: Vec<String> = (1..=30).map(|i| format!("line-{i:02}")).collect();
    let body: Vec<String> = (1..=9).map(|i| format!("{}{i}", B64_LINES[i % 2])).collect();
    let block: Vec<String> = private_key_block(&body).lines().map(str::to_string).collect();
    lines.splice(9..20, block);
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// AC7 (t2): `review packet` in all three forms, and the files a codex
/// reviewer reads, carry no secret; the events name the exit and the codes.
#[test]
fn redact_review_packet_and_the_codex_review_input_carry_no_secret() {
    let f = common::fixture();
    let (task, _) = candidate(&f);
    f.add_codex_profile("codex-a");
    let command = command_row(&f, &task);
    let command_code = code_of(&command);
    std::fs::write(blob_of(&command), thirty_lines()).unwrap();
    let diff = f
        .evidence_rows(&task)
        .into_iter()
        .find(|r| r["kind"] == "diff")
        .expect("a diff row");
    let diff_code = code_of(&diff);
    let diff_text = format!(
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n pub fn added() {{}}\n+const T: &str = \"{}\";\n",
        github_token()
    );
    std::fs::write(blob_of(&diff), diff_text).unwrap();
    f.state_conn()
        .execute(
            "UPDATE events SET detail = json_set(detail, '$.result_json.claims[0].text', ?1)
             WHERE seq = (SELECT MAX(seq) FROM events WHERE type = 'result_ingested' AND task_id = ?2)",
            [format!("fake suite green with {RD_VALUE}").as_str(), task.as_str()],
        )
        .unwrap();

    let json_out = packet(&f, &task, &["--json".to_string()]);
    let markdown = packet(&f, &task, &[]);
    let file = f.home_path().join("packet-out.md");
    let out_stdout = packet(&f, &task, &["--out".to_string(), file.display().to_string()]);
    let written = std::fs::read(&file).unwrap();
    for text in [&json_out, &markdown, &String::from_utf8(written.clone()).unwrap()] {
        no_secrets(text);
        assert!(text.contains("[redacted:"));
    }
    assert!(out_stdout.is_empty(), "{out_stdout}");
    assert_eq!(written, markdown.as_bytes());
    let v: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap();
    let tail = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == command_code.as_str())
        .unwrap()["output_tail"]
        .as_str()
        .unwrap()
        .to_string();
    for line in B64_LINES {
        assert!(!tail.contains(line), "{tail}");
    }
    assert!(tail.contains("[redacted:private-key]"), "{tail}");

    let number = |c: &str| c.trim_start_matches('E').parse::<u64>().unwrap();
    let events = secret_events(&f, &task);
    assert_eq!(events.len(), 3, "{events:#?}");
    for e in &events {
        assert_eq!(e["detail"]["exit"], "review_packet");
        assert!(e["run_id"].is_null(), "{e}");
        let codes: Vec<String> = e["detail"]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        let mut sorted = codes.clone();
        sorted.sort_by_key(|c| number(c));
        assert_eq!(codes, sorted);
        assert!(codes.contains(&command_code) && codes.contains(&diff_code), "{codes:?}");
    }

    let out = clean(&f)
        .env("RD_TEST_TOKEN", RD_VALUE)
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task)
        .args(["review", "start", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let review_run = r["run_id"].as_str().unwrap().to_string();
    clean(&f)
        .args(["wait", "--run", &review_run, "--timeout", "180s", "--json"])
        .assert()
        .success();

    let wt = PathBuf::from(r["worktree"].as_str().unwrap());
    let agent = wt.join(".agent");
    let packet_md = std::fs::read_to_string(agent.join("review-input").join("packet.md")).unwrap();
    assert!(packet_md.contains("[redacted:"));
    let mut inputs = files_under(&agent.join("review-input").join("evidence"));
    inputs.push(agent.join("review-input").join("packet.md"));
    inputs.push(agent.join("evidence-index.json"));
    for path in inputs {
        no_secrets(&String::from_utf8_lossy(&std::fs::read(&path).unwrap()));
    }
    let input_events: Vec<serde_json::Value> = secret_events(&f, &task)
        .into_iter()
        .filter(|e| e["run_id"] == review_run.as_str())
        .collect();
    assert_eq!(input_events.len(), 1, "{input_events:#?}");
    assert_eq!(input_events[0]["detail"]["exit"], "review_input");
    assert_eq!(
        f.wait_for_task_status(&task, &["pass", "reject", "unverified"], 60),
        "pass",
        "{}",
        f.explain_task(&task)
    );
}

/// AC2: a detail that is not JSON is masked in the human output, counted
/// there, and stays null under --json.
#[test]
fn redact_evidence_show_masks_invalid_detail() {
    let f = common::fixture();
    let (task, _) = candidate(&f);
    let code = code_of(&command_row(&f, &task));
    f.state_conn()
        .execute(
            "UPDATE evidence SET detail = ?1 WHERE task_id = ?2 AND code = ?3",
            [format!("not json: {RD_VALUE}").as_str(), task.as_str(), code.as_str()],
        )
        .unwrap();

    let human = show(&f, &task, &code, &[]);
    no_secret(&human, RD_VALUE, 0);
    assert!(human.contains("[redacted:env:RD_TEST_TOKEN]"), "{human}");
    let events = secret_events(&f, &task);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["detail"]["exit"], "evidence_show");
    assert_eq!(events[0]["detail"]["evidence"], serde_json::json!([code]));

    let json_out = show(&f, &task, &code, &["--json"]);
    let v: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap();
    assert!(v["detail"].is_null(), "{v}");
    no_secret(&json_out, RD_VALUE, 0);
    assert_eq!(secret_events(&f, &task).len(), 1, "--json hands no detail over, so nothing is counted");
}

/// AC5: when `secret_redacted` cannot be written, the masked output is still
/// printed with exit 0, and stderr has one warning line.
#[test]
fn redact_event_record_failure_keeps_readouts_usable() {
    let f = common::fixture();
    let (task, _) = candidate(&f);
    let row = command_row(&f, &task);
    let code = code_of(&row);
    std::fs::write(blob_of(&row), format!("some output {}\n", github_token())).unwrap();
    f.state_conn()
        .execute_batch(
            "CREATE TRIGGER rd_block_secret_redacted BEFORE INSERT ON events
             WHEN NEW.type = 'secret_redacted'
             BEGIN SELECT RAISE(ABORT, 'rd test blocks secret_redacted'); END;",
        )
        .unwrap();
    let file = f.home_path().join("packet-out.md");
    let commands: Vec<(&str, Vec<String>)> = vec![
        ("show", vec!["evidence".into(), "show".into(), code.clone(), "--task".into(), task.clone()]),
        ("show --json", vec!["evidence".into(), "show".into(), code.clone(), "--task".into(), task.clone(), "--json".into()]),
        ("packet", vec!["review".into(), "packet".into(), task.clone()]),
        ("packet --json", vec!["review".into(), "packet".into(), task.clone(), "--json".into()]),
        ("packet --out", vec!["review".into(), "packet".into(), task.clone(), "--out".into(), file.display().to_string()]),
    ];
    let warning = "warning: could not record secret_redacted: ";
    for (what, args) in commands {
        let mut c = clean(&f);
        c.args(&args);
        let (status, stdout, stderr) = run(c);
        assert_eq!(status, 0, "{what}: {stderr}");
        let output = if what == "packet --out" {
            assert!(stdout.is_empty(), "{what}: {stdout}");
            assert!(stderr.lines().any(|l| l.starts_with("wrote ")), "{what}: {stderr}");
            std::fs::read_to_string(&file).unwrap()
        } else {
            stdout
        };
        assert!(output.contains("[redacted:github-token]"), "{what}");
        no_secret(&output, &github_token(), 4);
        let warnings = stderr.lines().filter(|l| l.starts_with(warning)).count();
        assert_eq!(warnings, 1, "{what}: {stderr}");
    }
    assert!(secret_events(&f, &task).is_empty());
    assert_eq!(opened_count(&f, &task, &code), 2);
}
