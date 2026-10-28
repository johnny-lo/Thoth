mod common;

/// brief §13's first integration scenario, minus the claim linkage (that is
/// Task 5): the `hooks` fake drives the real `thoth hook` receiver for three
/// shell commands, and the runner turns the inbox into `evidence` rows, blobs
/// and a published index.
#[test]
fn the_hooks_scenario_captures_three_commands_and_publishes_the_index() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worktree = std::path::PathBuf::from(d["worktree"].as_str().unwrap());

    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    // --- the rows -------------------------------------------------------
    let rows = f.evidence_rows(&task_id);
    let cmds: Vec<&serde_json::Value> =
        rows.iter().filter(|r| r["kind"] == "command").collect();
    assert_eq!(cmds.len(), 3, "one row per Bash call, got {rows:#?}");
    assert_eq!(cmds[0]["code"], "E001");
    assert_eq!(cmds[1]["code"], "E002");
    assert_eq!(cmds[2]["code"], "E003");
    for c in &cmds {
        // v6 §2.2: the control plane collected these, not the claimant
        assert_eq!(c["collected_by"], "control_plane");
        assert_eq!(c["run_id"], serde_json::json!(run_id.clone()));
    }

    assert_eq!(cmds[0]["label"], "echo hello-stdout");
    assert_eq!(cmds[0]["detail"]["exit_code"], 0);
    assert_eq!(cmds[0]["detail"]["cmd"], "echo hello-stdout");
    assert_eq!(cmds[0]["bytes"], 12, "\"hello-stdout\" is 12 bytes");

    assert_eq!(cmds[1]["label"], "sh -c 'exit 3'");
    assert_eq!(
        cmds[1]["detail"]["exit_code"], 3,
        "the exit code has to come out of the error string"
    );
    assert_eq!(cmds[1]["detail"]["error"], "Exit code 3\noops-stderr");
    assert_eq!(
        cmds[1]["detail"]["stdout_sha"], cmds[1]["sha256"],
        "the failure's output is the primary blob"
    );
    let shown = f
        .cmd()
        .args(["evidence", "show", "E002", "--task", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown: serde_json::Value =
        serde_json::from_str(String::from_utf8(shown).unwrap().trim()).unwrap();
    let content = shown["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("oops-stderr"),
        "`evidence show` must print the failed command's output, got {shown:#?}"
    );

    assert_eq!(cmds[2]["label"], "seq 1 20000");
    assert_eq!(
        cmds[2]["bytes"], 108_894,
        "the complete persisted bytes, not the truncated inline preview"
    );
    assert_eq!(cmds[2]["detail"]["truncated_inline"], true);
    assert_eq!(cmds[2]["detail"]["blob_source"], "persisted");
    assert_eq!(cmds[2]["detail"]["duration_ms"], 83);

    // --- the blobs ------------------------------------------------------
    let big = std::path::PathBuf::from(cmds[2]["blob_path"].as_str().unwrap());
    assert!(big.is_file(), "the blob must be promoted into the repo store");
    assert_eq!(std::fs::metadata(&big).unwrap().len(), 108_894);
    let blobs_dir = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("blobs");
    assert_eq!(
        big.parent().unwrap(),
        blobs_dir.as_path(),
        "blobs live in the repo store, not in the run's inbox"
    );
    assert_eq!(
        big.file_name().unwrap().to_string_lossy(),
        cmds[2]["sha256"].as_str().unwrap(),
        "content addressing: the file name is the digest"
    );

    // --- the inbox ------------------------------------------------------
    let inbox = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("runs")
        .join(&run_id)
        .join("inbox");
    let jsonl = std::fs::read_to_string(inbox.join("hooks.jsonl")).unwrap();
    let inbox_lines: Vec<&str> = jsonl.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(inbox_lines.len(), 6, "three Pre plus three Post");
    for l in &inbox_lines {
        let v: serde_json::Value = serde_json::from_str(l).expect("every inbox line is JSON");
        assert!(v["received_at"].is_string());
        assert!(v["payload"]["tool_name"] == "Bash");
    }
    assert_eq!(
        std::fs::read_dir(inbox.join("blobs")).unwrap().count(),
        1,
        "only the persisted output needed an inbox blob"
    );
    assert!(
        !inbox.join("hook-errors.log").exists(),
        "the hook must not have had to report anything: {:?}",
        std::fs::read_to_string(inbox.join("hook-errors.log")).ok()
    );

    // --- the published index -------------------------------------------
    let index_text =
        std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap();
    let index: Vec<serde_json::Value> = serde_json::from_str(&index_text).unwrap();
    assert_eq!(index.len(), 5, "three commands plus the diff and commit rows: {index:#?}");
    let index_codes: Vec<&str> = index.iter().map(|r| r["code"].as_str().unwrap()).collect();
    for expected in ["E001", "E002", "E003"] {
        assert!(index_codes.contains(&expected), "{expected} missing from {index_codes:?}");
    }
    let first = &index[0];
    assert_eq!(first["kind"], "command");
    assert_eq!(first["label"], "echo hello-stdout");
    assert_eq!(first["exit_code"], 0);
    assert_eq!(index[1]["exit_code"], 3);

    // --- the events -----------------------------------------------------
    let kinds = f.events_of(&task_id);
    assert_eq!(
        kinds.iter().filter(|k| *k == "evidence_added").count(),
        rows.len(),
        "one evidence_added per row: {kinds:?}"
    );

    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'status_change'
             ORDER BY seq DESC LIMIT 1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(
        detail["evidence_recorded"], 3,
        "`evidence_recorded` counts what the inbox drain captured — the three \
         Bash calls. Task 4's diff and commit rows are added by the settle tail, \
         not by the drain, and deliberately do not appear here; the \
         `evidence_added` count above is the number that covers every row."
    );
    assert_eq!(detail["inbox_bad_lines"], 0);
    assert_eq!(detail["inbox_unpaired"], 0);
}

/// Fix round 1 / I1: `resume` reuses the same run row, the same `run_dir`,
/// and therefore the same inbox - but a fresh runner process means a fresh
/// `InboxTailer`. Before the offset-persistence fix, that fresh tailer
/// started at byte 0 and replayed every line the interrupted attempt already
/// turned into evidence, duplicating rows, `evidence_added` events, and index
/// entries. This drives the fake through two captured commands, a quota
/// interruption, and a resume with no hooks at all, and asserts the count
/// stayed at 2 all the way through.
#[test]
fn resuming_after_a_quota_interruption_does_not_duplicate_evidence() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_HOOKS_THEN_QUOTA", "1")
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let d: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worktree = std::path::PathBuf::from(d["worktree"].as_str().unwrap());

    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");

    // two commands captured before the interruption, nothing more - and,
    // since the interrupted attempt never reached a clean exit, no diff or
    // commit row either (Task 4 only records those on `clean_success`).
    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), 2, "got {rows:#?}");
    assert_eq!(rows[0]["code"], "E001");
    assert_eq!(rows[1]["code"], "E002");
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "command").count(),
        2,
        "got {rows:#?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["kind"] == "diff" || r["kind"] == "commit")
            .count(),
        0,
        "an interrupted attempt has no clean exit to record a diff/commit against: {rows:#?}"
    );
    let index_before: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index_before.len(), 2);

    // the operator decides the window has reset, same as the plain quota test
    f.cmd().args(["profile", "check", "claude-b"]).assert().success();

    // resume: no hooks fire this time (FAKE_SCENARIO=resume never calls them)
    f.cmd()
        .env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &run_id, "--json"])
        .assert()
        .success();

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    // Task 4 records diff+commit rows on every clean exit (controller ruling
    // 2026-09-21); this test counts by kind so it still proves resume does
    // not duplicate command evidence.
    //
    // command rows stay at exactly 2: the resumed run's fresh InboxTailer
    // must not have replayed the two lines the interrupted attempt already
    // drained. The resumed run's own clean exit adds exactly one diff row
    // and one commit row (candidate == base here, since the `resume` fake
    // makes no commit - an honest "the worker produced nothing" pair).
    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), 4, "duplicated evidence after resume, got {rows:#?}");
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "command").count(),
        2,
        "must not duplicate command evidence: {rows:#?}"
    );
    assert_eq!(rows[0]["code"], "E001");
    assert_eq!(rows[1]["code"], "E002");
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "diff").count(),
        1,
        "got {rows:#?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r["kind"] == "commit").count(),
        1,
        "got {rows:#?}"
    );

    let kinds = f.events_of(&task_id);
    assert_eq!(
        kinds.iter().filter(|k| *k == "evidence_added").count(),
        4,
        "2 commands + 1 diff + 1 commit, must not double up: {kinds:?}"
    );

    let index_after: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index_after.len(), 4, "the index must not gain phantom entries either");
    assert_eq!(
        index_after.iter().filter(|r| r["kind"] == "command").count(),
        2,
        "the two command entries from before the resume must still be the only ones: {index_after:#?}"
    );
}

/// brief §2: once the child exits cleanly the control plane adds its own two
/// rows — the candidate diff and the candidate commit — and republishes the
/// index. The `hooks` fake commits a `src/` file and a `tests/` file, so
/// `touches_tests` has something real to find.
#[test]
fn a_clean_run_adds_diff_and_commit_evidence_with_touches_tests_set() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, base_commit) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worktree = std::path::PathBuf::from(d["worktree"].as_str().unwrap());

    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let candidate: String = f
        .state_conn()
        .query_row(
            "SELECT candidate_commit FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(candidate, base_commit, "the fake worker commits its work");

    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), 5, "three commands, one diff, one commit: {rows:#?}");

    let diff = rows.iter().find(|r| r["kind"] == "diff").expect("a diff row");
    assert_eq!(diff["code"], "E004");
    assert_eq!(diff["collected_by"], "control_plane");
    assert_eq!(diff["detail"]["from_commit"], base_commit);
    assert_eq!(diff["detail"]["to_commit"], candidate);
    let mut files: Vec<String> = diff["detail"]["files_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    files.sort();
    assert_eq!(
        files,
        vec!["src/added.rs".to_string(), "tests/added_test.rs".to_string()]
    );
    assert_eq!(diff["detail"]["touches_tests"], true);
    let patch = std::fs::read_to_string(diff["blob_path"].as_str().unwrap()).unwrap();
    assert!(patch.contains("+pub fn added() -> u8 { 7 }"), "got {patch}");
    assert!(
        patch.contains("tests/added_test.rs"),
        "the reviewer's diff must include the test file: {patch}"
    );

    let commit = rows.iter().find(|r| r["kind"] == "commit").expect("a commit row");
    assert_eq!(commit["code"], "E005");
    assert_eq!(commit["detail"]["sha"], candidate);
    assert_eq!(commit["detail"]["subject"], "phase2 fake worker commit");
    assert!(commit["detail"]["author_date"].as_str().unwrap().contains('T'));

    // the republished index carries all five
    let index: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index.len(), 5);
    assert_eq!(index[3]["kind"], "diff");
    assert_eq!(index[3]["exit_code"], serde_json::Value::Null);
    assert_eq!(index[4]["kind"], "commit");
}

/// brief §13: the whole chain - hook → inbox → evidence → index → the worker
/// citing a code → `claim_evidence` - with the task landing `in_review`.
#[test]
fn a_cited_code_becomes_a_claim_evidence_link_and_the_task_reaches_in_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let conn = f.state_conn();
    // two claims: the cited test_result and the uncited completion
    let claims: Vec<(String, String, String)> = conn
        .prepare("SELECT claim_id, kind, text FROM claims WHERE run_id = ?1 ORDER BY kind")
        .unwrap()
        .query_map([&run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(claims.len(), 2, "{claims:?}");
    assert_eq!(claims[0].1, "completion");
    assert_eq!(claims[1].1, "test_result");

    // the test_result claim links to E001 through claim_evidence
    let linked: Vec<String> = conn
        .prepare(
            "SELECT e.code FROM claim_evidence ce
             JOIN evidence e ON e.evidence_id = ce.evidence_id
             WHERE ce.claim_id = ?1",
        )
        .unwrap()
        .query_map([&claims[1].0], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert_eq!(linked, vec!["E001".to_string()]);

    // and the completion claim links to nothing
    let none: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM claim_evidence WHERE claim_id = ?1",
            [&claims[0].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(none, 0);

    // brief §7: the ingest event names the codes each claim cited
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'result_ingested'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["evidence_refs"][&claims[1].0], serde_json::json!(["E001"]));
    assert_eq!(detail["evidence_refs"][&claims[0].0], serde_json::json!([]));

    let kinds = f.events_of(&task_id);
    assert!(kinds.contains(&"result_ingested".to_string()), "{kinds:?}");
    assert!(!kinds.contains(&"result_invalid".to_string()), "{kinds:?}");
}

/// v6 §2.5 rule 3, end to end: the worker cites a code the control plane never
/// allocated, so the run is `completed` but the task is `unverified` and no
/// claim survives.
#[test]
fn citing_a_code_that_does_not_exist_lands_the_task_in_unverified() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks_bad_ref", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "unverified");

    let conn = f.state_conn();
    let rstatus: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(rstatus, "completed", "the harness itself succeeded");

    let claims: i64 = conn
        .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(claims, 0, "a rejected document must write no claims");

    // the evidence itself is still there: capture is independent of the claim
    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), 5, "hook capture does not depend on the claims");

    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'result_invalid'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["rule"], 3);
    assert_eq!(detail["code"], "E999");
    assert!(detail["reason"].as_str().unwrap().contains("E999"));
}

/// v6 §2.3 and §10.3: the reviewer's read path. `evidence list` is the index,
/// `evidence show` is the raw bytes, and every open is recorded - with the run
/// id when the caller is a worker, without one when it is a human.
#[test]
fn evidence_list_and_show_report_the_rows_and_record_every_open() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worktree = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    let (code, _status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);

    // --- list -----------------------------------------------------------
    let out = f
        .cmd()
        .args(["evidence", "list", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let listed = v["evidence"].as_array().unwrap();
    assert_eq!(listed.len(), 5);
    assert_eq!(listed[0]["code"], "E001");
    assert_eq!(listed[0]["exit_code"], 0);
    assert_eq!(listed[1]["exit_code"], 3);
    assert_eq!(listed[3]["kind"], "diff");
    assert_eq!(listed[4]["kind"], "commit");

    // human mode prints one line per row
    let out = f
        .cmd()
        .args(["evidence", "list", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.trim().lines().count(), 5, "got {text}");
    assert!(text.contains("seq 1 20000"), "got {text}");

    // --- show, truncated, no run id -------------------------------------
    let out = f
        .cmd()
        .args(["evidence", "show", "E003", "--task", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["code"], "E003");
    assert_eq!(v["bytes"], 108_894);
    assert_eq!(v["blob_verified"], true);
    assert_eq!(v["content_truncated"], true);
    assert_eq!(v["content"].as_str().unwrap().len(), 4000, "DEFAULT_SHOW_BYTES");
    assert!(v["content"].as_str().unwrap().starts_with("1\n2\n3\n"));
    assert_eq!(v["opened_by_run"], serde_json::Value::Null);

    // --bytes raises the cap
    let out = f
        .cmd()
        .args([
            "evidence", "show", "E003", "--task", &task_id, "--bytes", "200000", "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["content_truncated"], false);
    assert_eq!(v["content"].as_str().unwrap().len(), 108_894);

    // --- show, attributed to a run, from inside the worktree -------------
    // Open questions 5 and 14 together: a worker runs thoth from its own
    // worktree with THOTH_RUN_ID set, and needs neither --task nor a
    // Supervisor's cwd.
    let out = f
        .cmd_in(&worktree)
        .env("THOTH_RUN_ID", &run_id)
        .args(["evidence", "show", "e001", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["code"], "E001", "a lowercase code is accepted");
    assert_eq!(v["task_id"], serde_json::json!(task_id.clone()));
    assert_eq!(v["content"], "hello-stdout");
    assert_eq!(v["opened_by_run"], serde_json::json!(run_id.clone()));

    // --- the evidence_opened events -------------------------------------
    let conn = f.state_conn();
    let opened: Vec<(String, Option<String>)> = conn
        .prepare("SELECT detail, run_id FROM events WHERE type = 'evidence_opened' ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(opened.len(), 3, "one per show, including the truncated ones");
    assert!(opened[0].0.contains("\"code\":\"E003\""));
    assert_eq!(opened[0].1, None, "a human read it");
    assert_eq!(opened[2].1.as_deref(), Some(run_id.as_str()));
    assert!(opened[2].0.contains("\"blob_verified\":true"));

    // --- failure modes ---------------------------------------------------
    f.cmd()
        .args(["evidence", "show", "E999", "--task", &task_id])
        .assert()
        .failure();
    f.cmd()
        .args(["evidence", "list", "01NOSUCHTASK"])
        .assert()
        .failure();
}
