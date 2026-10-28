mod common;

use common::Fixture;

/// The `summary` the `hooks` fake worker writes into result.json
/// (src/bin/fake-claude.rs). The packet must never show it.
const WORKER_SUMMARY: &str = "fake worker completed the task with captured evidence";

/// Runs the `hooks` implementer scenario to completion and returns
/// `(task_id, implementer_run_id)` with the task sitting at `in_review`.
fn candidate_ready(f: &Fixture, spec: &str) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    (task_id, run_id)
}

fn packet_json(f: &Fixture, task_id: &str) -> serde_json::Value {
    let out = f
        .cmd()
        .args(["review", "packet", task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn packet_md(f: &Fixture, task_id: &str) -> String {
    let out = f
        .cmd()
        .args(["review", "packet", task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

fn record_pass(f: &Fixture, task_id: &str) {
    let review = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = f.home_path().join("review-pass.json");
    std::fs::write(&file, review).unwrap();
    f.cmd()
        .args(["review", "record", task_id, "-f"])
        .arg(&file)
        .assert()
        .success();
}

fn count(f: &Fixture, table: &str) -> i64 {
    f.state_conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// Byte offset of `needle` in `hay`, panicking with the packet attached.
fn pos(hay: &str, needle: &str) -> usize {
    hay.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found in:\n{hay}"))
}

/// AC2: the six sections, in order, with the content the brief lists.
#[test]
fn markdown_packet_has_the_six_sections_in_order() {
    let f = common::fixture();
    let (task_id, run_id) = candidate_ready(&f, Fixture::SPEC_TWO_AC);
    record_pass(&f, &task_id);

    let md = packet_md(&f, &task_id);

    // (1) header, (2) spec verbatim, (3) evidence index, (4) diffs, (5) claims, (6) reviews
    let p1 = pos(&md, &task_id);
    let p2 = pos(&md, "say hello twice");
    let p2b = pos(&md, "AC2: exits zero");
    let p3 = pos(&md, "E001");
    let p4 = pos(&md, "pub fn added()");
    let p5 = pos(&md, "fake suite green");
    let p6 = pos(&md, "PASS");
    assert!(p1 < p2 && p2 < p2b && p2b < p3 && p3 < p4 && p4 < p5 && p5 < p6, "order:\n{md}");

    assert!(md.contains("in_review") || md.contains("pass"), "status missing:\n{md}");
    assert!(md.contains("normal"), "risk missing:\n{md}");
    // the run the claims came from, and the run each diff belongs to
    assert!(md.contains(&run_id), "run id missing:\n{md}");
    // the evidence index shows the exit code of the failing command
    assert!(md.contains("hello-stdout"), "command output tail missing:\n{md}");
    // the omission is announced, without the summary itself
    assert!(md.to_lowercase().contains("summary"), "omission note missing:\n{md}");
    assert!(md.to_lowercase().contains("on purpose"), "omission note missing:\n{md}");
    // the recorded review is marked external
    assert!(md.to_lowercase().contains("external"), "external marker missing:\n{md}");
}

/// AC3: the exact top-level keys and item shapes.
#[test]
fn json_packet_has_the_specified_keys_and_shapes() {
    let f = common::fixture();
    let (task_id, run_id) = candidate_ready(&f, Fixture::SPEC_TWO_AC);
    record_pass(&f, &task_id);

    let v = packet_json(&f, &task_id);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "approvals", "claims", "claims_run_id", "diffs", "evidence", "protected_changes",
            "reviews", "risk", "runs", "spec", "status", "task_id", "title"
        ]
    );
    // P7-2: the hooks worker only adds files, so nothing protected changed
    assert_eq!(v["protected_changes"], serde_json::json!([]));
    assert_eq!(v["task_id"], task_id);
    assert_eq!(v["risk"], "normal");
    assert_eq!(v["spec"]["goal"], "say hello twice");
    assert_eq!(v["spec"]["acceptance_criteria"][1], "AC2: exits zero");
    for k in ["constraints", "non_goals", "excluded_alternatives"] {
        assert!(v["spec"][k].is_array(), "spec.{k}: {}", v["spec"]);
    }

    let evidence = v["evidence"].as_array().unwrap();
    assert!(evidence.len() >= 4, "3 commands + diff: {evidence:?}");
    for e in evidence {
        let mut ek: Vec<&str> = e.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        ek.sort();
        assert_eq!(
            ek,
            vec!["code", "exit_code", "kind", "label", "output_tail", "run_id"],
            "{e}"
        );
        if e["kind"] == "command" {
            assert!(e["output_tail"].is_string(), "{e}");
        } else {
            assert!(e["output_tail"].is_null(), "{e}");
        }
    }
    let first = &evidence[0];
    assert_eq!(first["code"], "E001");
    assert_eq!(first["exit_code"], 0);
    assert!(first["output_tail"].as_str().unwrap().contains("hello-stdout"));

    let diffs = v["diffs"].as_array().unwrap();
    assert_eq!(diffs.len(), 1, "{diffs:?}");
    let mut dk: Vec<&str> = diffs[0].as_object().unwrap().keys().map(|s| s.as_str()).collect();
    dk.sort();
    assert_eq!(dk, vec!["blob_verified", "code", "content", "run_id"]);
    assert_eq!(diffs[0]["blob_verified"], true);
    assert!(diffs[0]["content"].as_str().unwrap().contains("pub fn added()"));
    assert_eq!(diffs[0]["run_id"], run_id);

    assert_eq!(v["claims_run_id"], run_id);
    let claims = v["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2, "{claims:?}");
    assert_eq!(claims[0]["text"], "fake suite green");

    let reviews = v["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1);
    let mut rk: Vec<&str> = reviews[0].as_object().unwrap().keys().map(|s| s.as_str()).collect();
    rk.sort();
    assert_eq!(rk, vec!["external", "findings", "verdict"]);
    assert_eq!(reviews[0]["verdict"], "PASS");
    assert_eq!(reviews[0]["external"], true);
    assert!(reviews[0]["findings"]["criteria"].is_array());
}

/// AC2 + AC3 + AC6: the worker's own summary is in neither format.
#[test]
fn the_workers_summary_appears_in_neither_format() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    // Sanity: the summary really is stored, so its absence is the packet's doing.
    let events = f
        .cmd()
        .args(["events", "--task", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8(events).unwrap().contains(WORKER_SUMMARY));

    let md = packet_md(&f, &task_id);
    assert!(!md.contains(WORKER_SUMMARY), "summary leaked into markdown:\n{md}");

    let v = packet_json(&f, &task_id);
    let text = v.to_string();
    assert!(!text.contains(WORKER_SUMMARY), "summary leaked into json: {text}");
    assert!(!text.contains("\"summary\""), "a summary key exists: {text}");
}

/// AC2 (4): a diff is shown whole, and a blob that no longer matches its hash
/// is shown anyway with `blob_verified` false.
#[test]
fn diffs_are_never_truncated_and_report_blob_verified() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let diff_row = f
        .evidence_rows(&task_id)
        .into_iter()
        .find(|r| r["kind"] == "diff")
        .expect("a diff row");
    let blob = diff_row["blob_path"].as_str().unwrap().to_string();
    let mut bytes = std::fs::read(&blob).unwrap();
    // Well past the 4000-byte `evidence show` default, with a marker at the very end.
    for i in 0..600 {
        bytes.extend_from_slice(format!("+padding line {i:04}\n").as_bytes());
    }
    bytes.extend_from_slice(b"+LAST-LINE-MARKER\n");
    assert!(bytes.len() > 8000);
    std::fs::write(&blob, &bytes).unwrap();

    let v = packet_json(&f, &task_id);
    let d = &v["diffs"][0];
    let content = d["content"].as_str().unwrap();
    assert!(content.ends_with("+LAST-LINE-MARKER\n"), "cut short: {} bytes", content.len());
    assert_eq!(content.len(), bytes.len());
    assert_eq!(d["blob_verified"], false, "tampered blob must be flagged");

    let md = packet_md(&f, &task_id);
    assert!(md.contains("+LAST-LINE-MARKER"), "markdown diff cut short");
    assert!(md.contains("blob_verified"), "markdown must show blob_verified");
}

/// AC2 (3): a command row's tail is at most its last 15 lines.
#[test]
fn command_output_tail_is_the_last_fifteen_lines() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let row = f
        .evidence_rows(&task_id)
        .into_iter()
        .find(|r| r["kind"] == "command" && r["blob_path"].is_string())
        .expect("a command row with a blob");
    let blob = row["blob_path"].as_str().unwrap().to_string();
    let mut body = String::new();
    for i in 1..=40 {
        body.push_str(&format!("line {i}\n"));
    }
    std::fs::write(&blob, &body).unwrap();

    let v = packet_json(&f, &task_id);
    let code = row["code"].as_str().unwrap();
    let e = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == code)
        .unwrap();
    let tail = e["output_tail"].as_str().unwrap();
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(lines.len(), 15, "{tail:?}");
    assert_eq!(lines[0], "line 26");
    assert_eq!(lines[14], "line 40");
}

/// A failed command has an empty blob (PostToolUseFailure carries no
/// tool_response); its output lives in `detail.error`, which the packet falls
/// back to, keeping the last fifteen lines.
#[test]
fn a_failed_command_with_an_empty_blob_shows_detail_error() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let row = f
        .evidence_rows(&task_id)
        .into_iter()
        .find(|r| r["kind"] == "command" && r["blob_path"].is_string())
        .expect("a command row with a blob");
    let code = row["code"].as_str().unwrap().to_string();
    std::fs::write(row["blob_path"].as_str().unwrap(), "").unwrap();
    let mut error = String::from("Exit code 101\n");
    for i in 1..=30 {
        error.push_str(&format!("err {i}\n"));
    }
    f.state_conn()
        .execute(
            "UPDATE evidence SET detail = json_set(detail, '$.error', ?1) WHERE task_id = ?2 AND code = ?3",
            rusqlite::params![error, task_id, code],
        )
        .unwrap();

    let v = packet_json(&f, &task_id);
    let e = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == code.as_str())
        .unwrap();
    let tail = e["output_tail"].as_str().unwrap();
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(lines.len(), 15, "{tail:?}");
    assert_eq!(lines[0], "err 16");
    assert_eq!(lines[14], "err 30");

    let md = packet_md(&f, &task_id);
    assert!(md.contains("err 30"), "{md}");
}

/// AC4: `--out` writes exactly the stdout bytes, no BOM, stdout empty.
#[test]
fn out_writes_the_same_bytes_as_stdout_without_a_bom() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    for json in [false, true] {
        let mut plain = f.cmd();
        plain.args(["review", "packet", &task_id]);
        if json {
            plain.arg("--json");
        }
        let expected = plain.assert().success().get_output().stdout.clone();

        let path = f.home_path().join(if json { "packet.json" } else { "packet.md" });
        let mut c = f.cmd();
        c.args(["review", "packet", &task_id, "--out"]).arg(&path);
        if json {
            c.arg("--json");
        }
        let out = c.assert().success().get_output().clone();
        assert!(out.stdout.is_empty(), "stdout must stay empty: {:?}", out.stdout);
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains(&format!("wrote {} ({} bytes)", path.display(), expected.len())),
            "stderr: {stderr}"
        );

        let written = std::fs::read(&path).unwrap();
        assert_eq!(written, expected);
        assert!(!written.starts_with(&[0xEF, 0xBB, 0xBF]), "BOM written");
        assert!(String::from_utf8(written).is_ok());
    }
}

/// AC1: the command only reads.
#[test]
fn packet_writes_nothing_to_the_database() {
    let f = common::fixture();
    let (task_id, _run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let before = (count(&f, "events"), count(&f, "evidence"), count(&f, "reviews"), count(&f, "runs"));
    let status_before = f.task_status(&task_id);
    let _ = packet_md(&f, &task_id);
    let _ = packet_json(&f, &task_id);
    let after = (count(&f, "events"), count(&f, "evidence"), count(&f, "reviews"), count(&f, "runs"));
    assert_eq!(before, after);
    assert_eq!(status_before, f.task_status(&task_id));
}

/// AC5: nothing to show is still a packet, exit 0.
#[test]
fn a_task_with_nothing_recorded_still_produces_a_packet() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (task_id, _) = f.create_task(Fixture::SPEC);

    let md = packet_md(&f, &task_id);
    assert!(md.contains(&task_id));
    assert!(md.contains("say hello"));
    assert!(md.to_lowercase().contains("no evidence"), "{md}");
    assert!(md.to_lowercase().contains("no result"), "{md}");
    assert!(md.to_lowercase().contains("no review"), "{md}");

    let v = packet_json(&f, &task_id);
    assert_eq!(v["evidence"], serde_json::json!([]));
    assert_eq!(v["diffs"], serde_json::json!([]));
    assert_eq!(v["claims"], serde_json::json!([]));
    assert!(v["claims_run_id"].is_null());
    assert_eq!(v["reviews"], serde_json::json!([]));
}

/// AC5: an unknown id fails and the error names it.
#[test]
fn an_unknown_task_id_fails_naming_the_id() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();

    let unknown = "01ZZZZZZZZZZZZZZZZZZZZZZZZ";
    let out = f
        .cmd()
        .args(["review", "packet", unknown])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(String::from_utf8(out.stderr).unwrap().contains(unknown));
    assert!(out.stdout.is_empty());

    // a too-short id is refused too, and still named
    let out = f
        .cmd()
        .args(["review", "packet", "nope"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(String::from_utf8(out.stderr).unwrap().contains("nope"));
}

/// Like `candidate_ready`, but the fake worker reports `subagents` spawned.
fn candidate_ready_with_subagents(f: &Fixture, spec: &str, subagents: &str) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(spec);
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_SUBAGENTS", subagents)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    (task_id, run_id)
}

/// AC4: the packet says, per run, which models ran (with output tokens) and
/// how many subagents were spawned; no warning when there were none.
#[test]
fn the_packet_lists_models_and_subagents_per_run() {
    let f = common::fixture();
    let (task_id, run_id) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let v = packet_json(&f, &task_id);
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["run_id"], run_id);
    assert_eq!(runs[0]["role"], "implementer");
    assert_eq!(runs[0]["models_used"]["fake-model"], 610);
    assert_eq!(runs[0]["models_used"]["fake-haiku"], 12);
    assert_eq!(runs[0]["subagents_spawned"], 0);

    let md = packet_md(&f, &task_id);
    assert!(md.contains("## Runs"), "{md}");
    assert!(md.contains("fake-model 610"), "{md}");
    assert!(md.contains("fake-haiku 12"), "{md}");
    assert!(md.contains("subagents: 0"), "{md}");
    assert!(!md.contains("WARNING"), "no subagents, no warning:\n{md}");
}

/// AC4: spawned subagents produce a clearly marked warning line.
#[test]
fn the_packet_warns_when_subagents_were_spawned() {
    let f = common::fixture();
    let (task_id, run_id) = candidate_ready_with_subagents(&f, Fixture::SPEC_TWO_AC, "2");

    let v = packet_json(&f, &task_id);
    assert_eq!(v["runs"][0]["subagents_spawned"], 2);

    let md = packet_md(&f, &task_id);
    let warn = md
        .lines()
        .find(|l| l.contains("WARNING"))
        .unwrap_or_else(|| panic!("no warning line:\n{md}"));
    assert!(warn.contains(&run_id) && warn.contains("2 subagent"), "{warn}");
}

/// A diff touching a non-UTF-8 file is stored as git's raw bytes; showing it
/// (evidence show, review packet) still works, decoding lossily.
#[test]
fn a_non_utf8_diff_is_stored_raw_and_still_displays() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_COMMIT_NON_UTF8", "1")
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let _: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!((code, status.as_str()), (0, "in_review"));

    let diff_row = f
        .evidence_rows(&task_id)
        .into_iter()
        .find(|r| r["kind"] == "diff")
        .expect("a diff row");
    let to = diff_row["detail"]["to_commit"].as_str().unwrap().to_string();
    let from = diff_row["detail"]["from_commit"].as_str().unwrap().to_string();
    let raw = std::process::Command::new("git")
        .args(["diff", "--patch", "--stat", &format!("{from}..{to}")])
        .current_dir(f.repo_path())
        .output()
        .unwrap()
        .stdout;
    let stored = std::fs::read(diff_row["blob_path"].as_str().unwrap()).unwrap();
    let legacy: &[u8] = b"line \xa4\xa4\xa4\xe5 end";
    assert!(stored.windows(legacy.len()).any(|w| w == legacy), "original bytes kept");
    assert_eq!(stored, raw, "blob is git's exact stdout");

    let code = diff_row["code"].as_str().unwrap();
    f.cmd().args(["evidence", "show", code]).assert().success();
    let v = packet_json(&f, &task_id);
    assert_eq!(v["diffs"][0]["blob_verified"], true);
    packet_md(&f, &task_id);
}
