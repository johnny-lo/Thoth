mod common;

/// Drives a task all the way to `reject`: implement with the `hooks` scenario,
/// then review it with `review_reject`. Returns
/// `(task_id, implementer_run_id, candidate_commit)`.
fn rejected_task(f: &common::Fixture, spec: &str) -> (String, String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &["--skip-spec-review", "rework fixture: the spec review gate is covered by tests/spec_review.rs"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review", "{}", f.explain_run(&run_id));

    let r = f.review_start(&task_id, "claude-a", "review_reject", &["--model", "haiku"]);
    let mut wait = f.cmd();
    wait.args([
        "wait",
        "--run",
        r["run_id"].as_str().unwrap(),
        "--timeout",
        "180s",
    ]);
    f.run_ok(wait, "wait --run");
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "reject",
        "{}",
        f.explain_task(&task_id)
    );

    let candidate: String = f
        .state_conn()
        .query_row(
            "SELECT candidate_commit FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    (task_id, run_id, candidate)
}

#[test]
fn stuck_rework_refuses_until_spec_is_updated_even_with_allow_attempt() {
    let f = common::fixture();
    let (task_id, first_run, _) = rejected_task(&f, common::Fixture::SPEC_TWO_AC);
    let conn = f.state_conn();
    for n in 1..=2 {
        let review_id = format!("SEEDED-STUCK-{n}");
        let findings = serde_json::json!({
            "criteria": [{"id": "AC1", "verdict": "unmet"}],
            "extra_findings": []
        }).to_string();
        conn.execute(
            "INSERT INTO reviews (review_id, target_run_id, reviewer_run_id, verdict, findings, created_at)
             VALUES (?1, ?2, NULL, 'REJECT', ?3, ?4)",
            rusqlite::params![review_id, first_run, findings, format!("2026-09-30T00:00:0{n}Z")],
        ).unwrap();
        conn.execute(
            "INSERT INTO events (ts, task_id, run_id, type, detail) VALUES (?1, ?2, ?3, 'verdict', ?4)",
            rusqlite::params![
                format!("2026-09-30T00:00:1{n}Z"),
                task_id,
                first_run,
                serde_json::json!({
                    "verdict": "REJECT",
                    "review_id": review_id,
                    "criteria": [{"id": "AC1", "verdict": "unmet"}]
                }).to_string(),
            ],
        ).unwrap();
    }
    drop(conn);

    let runs_before: i64 = f.state_conn().query_row(
        "SELECT COUNT(*) FROM runs WHERE task_id = ?1", [&task_id], |r| r.get(0)
    ).unwrap();
    let err = f.cmd().args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert().failure().get_output().stderr.clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("stuck") && err.contains("AC1"), "got {err}");
    let err = f.cmd().args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--allow-attempt", "9"])
        .assert().failure().get_output().stderr.clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("stuck") && err.contains("AC1"), "got {err}");
    let runs_after: i64 = f.state_conn().query_row(
        "SELECT COUNT(*) FROM runs WHERE task_id = ?1", [&task_id], |r| r.get(0)
    ).unwrap();
    assert_eq!(runs_after, runs_before);

    let new_spec = f.home_path().join("stuck-rewrite.json");
    std::fs::write(&new_spec, common::Fixture::SPEC_TWO_AC).unwrap();
    f.cmd().args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec).assert().success();
    let dispatch = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(dispatch["attempt"], 2);
}

/// brief §13: `reject` → `dispatch` gives attempt 2, a new worktree branched
/// from the previous candidate, and a run spec carrying the folded findings.
#[test]
fn a_rejected_task_reworks_as_attempt_two_from_the_previous_candidate() {
    let f = common::fixture();
    let (task_id, first_run, candidate) = rejected_task(&f, common::Fixture::SPEC_TWO_AC);

    // Task 8 Step 5: a rejected task is the Supervisor's next piece of work, so
    // `status` with no arguments must list it. Before `is_dead_end` dropped
    // `Reject`, the one task that needed attention was the one hidden.
    let out = f
        .cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let listed: Vec<&str> = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap())
        .collect();
    assert!(
        listed.contains(&task_id.as_str()),
        "a rejected task must stay in `status`: {listed:?}"
    );

    // `crash` so nothing commits: the new worktree's HEAD is then provably the
    // previous candidate rather than whatever a second worker did to it.
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    assert_eq!(d["rework"].as_bool(), Some(true));
    assert_eq!(
        d["worktree_reused"].as_bool(),
        Some(false),
        "a new attempt gets a new worktree"
    );
    assert_eq!(d["base_commit"].as_str(), Some(candidate.as_str()));
    assert_eq!(d["attempt_cap"].as_i64(), Some(2));
    assert!(
        d["warnings"].as_array().unwrap().is_empty(),
        "R3: findings were folded, so there is nothing to warn about: {d}"
    );
    assert!(
        d["folded_findings"].as_i64().unwrap() >= 2,
        "one unmet criterion plus one extra finding: {d}"
    );

    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert!(wt.to_string_lossy().ends_with("-a2"), "got {wt:?}");
    assert_eq!(
        common::git(&wt, &["rev-parse", "HEAD"]),
        candidate,
        "brief §9: the rework keeps the previous attempt's work"
    );
    assert_eq!(
        common::git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        format!("thoth/{task_id}/a2")
    );

    // the run's spec carries the addendum; the task's spec does not
    let second_run = d["run_id"].as_str().unwrap().to_string();
    let conn = f.state_conn();
    let run_spec: String = conn
        .query_row(
            "SELECT spec FROM runs WHERE run_id = ?1",
            [&second_run],
            |r| r.get(0),
        )
        .unwrap();
    let run_spec: serde_json::Value = serde_json::from_str(&run_spec).unwrap();
    assert_eq!(run_spec["rework"]["attempt"], 2);
    assert_eq!(
        run_spec["rework"]["previous_run"],
        serde_json::json!(first_run)
    );
    assert_eq!(
        run_spec["rework"]["previous_candidate"],
        serde_json::json!(candidate)
    );
    assert_eq!(
        run_spec["rework"]["previous_attempt"], 1,
        "R2: the findings and the candidate are attempt 1's"
    );
    let unmet = run_spec["rework"]["unmet"].as_array().unwrap();
    assert_eq!(unmet.len(), 1);
    assert!(unmet[0].as_str().unwrap().contains("no captured evidence"));
    assert_eq!(
        run_spec["rework"]["extra_findings"].as_array().unwrap().len(),
        1
    );

    let task_spec: String = conn
        .query_row(
            "SELECT spec FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !task_spec.contains("rework"),
        "the task's own spec must stay as the Supervisor wrote it"
    );

    // the evidence sequence keeps counting across attempts (brief §6)
    let (code, _status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    let codes: Vec<String> = f
        .evidence_rows(&task_id)
        .iter()
        .map(|r| r["code"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        codes,
        vec!["E001", "E002", "E003", "E004", "E005"],
        "attempt 2 crashed, so it added none - but the sequence is per task, not per attempt"
    );

    // and the prompt the runner built actually contains the findings. Read
    // after the wait above: `prompt.txt` is written by the detached runner, so
    // reading it while the run is still starting up is a race.
    let prompt = std::fs::read_to_string(
        f.home_path()
            .join("repos")
            .join(f.repo_id())
            .join("runs")
            .join(&second_run)
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(
        prompt.contains("## Previous attempt was rejected"),
        "got {prompt}"
    );
    assert!(prompt.contains("This is attempt 2"));
    assert!(prompt.contains("no captured evidence"));

    // R4: a task that has runs reports the attempt it is on and its cap
    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["attempt"], 2);
    assert_eq!(v["attempt_cap"], 2);
}

/// brief §13: the third attempt is refused by the cap, and `--allow-attempt`
/// lets exactly that one dispatch through.
#[test]
fn the_attempt_cap_refuses_a_third_attempt_unless_allow_attempt_is_passed() {
    let f = common::fixture();
    let (task_id, _first_run, _candidate) = rejected_task(&f, common::Fixture::SPEC_TWO_AC);

    // attempt 2 is within the default cap of 2
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "interrupted");

    // Put the task back to `reject` directly rather than running a second full
    // review: the cap logic is what this test is about, and a second review
    // would only re-prove Task 7.
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();

    // attempt 3 is past the cap
    let err = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("attempt 3"), "got {err}");
    assert!(err.contains("cap of 2"), "got {err}");
    assert!(err.contains("--allow-attempt 3"), "got {err}");
    assert_eq!(
        f.task_status(&task_id),
        "reject",
        "a refused dispatch changes nothing"
    );

    // and a run count that did not grow
    let runs: i64 = f
        .state_conn()
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND attempt = 3",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(runs, 0);

    // --allow-attempt 3 lets it through
    let d = f.dispatch(&task_id, "claude-b", "crash", &["--allow-attempt", "3"]);
    assert_eq!(d["attempt"].as_i64(), Some(3));
    assert_eq!(
        d["attempt_cap"].as_i64(),
        Some(2),
        "the cap itself is untouched"
    );

    // R2 (fix round 1): attempt 2 crashed, so it never completed - the
    // findings and the commits attempt 3 inherits are attempt 1's, and the
    // addendum has to say 1 rather than "the attempt before this one".
    let third_run = d["run_id"].as_str().unwrap().to_string();
    let run_spec: String = f
        .state_conn()
        .query_row(
            "SELECT spec FROM runs WHERE run_id = ?1",
            [&third_run],
            |r| r.get(0),
        )
        .unwrap();
    let run_spec: serde_json::Value = serde_json::from_str(&run_spec).unwrap();
    assert_eq!(run_spec["rework"]["attempt"], 3);
    assert_eq!(
        run_spec["rework"]["previous_attempt"], 1,
        "attempt 2 crashed, so attempt 1 is what this rework builds on: {run_spec}"
    );
    let (code, _status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);

    // ...and only that one dispatch: the cap gates the next attempt again
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    f.cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure();
}

/// v6 §6.2: at important/high risk the findings are not folded mechanically.
/// `dispatch` refuses and names the way forward; `task update-spec` is it.
#[test]
fn an_important_risk_rework_needs_the_spec_rewritten_first() {
    let f = common::fixture();
    let important = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;
    let (task_id, _first_run, _candidate) = rejected_task(&f, important);

    let err = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("risk 'important'"), "got {err}");
    assert!(err.contains("task update-spec"), "got {err}");

    // rewrite the spec, which is also allowed to change the risk
    let new_spec = f.home_path().join("rewritten.json");
    std::fs::write(
        &new_spec,
        r#"{"goal":"guard the ledger, rejecting a double spend before it is written","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection","AC3: the rejection is covered by a test"],"constraints":["no new dependencies"],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#,
    )
    .unwrap();
    let out = f
        .cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["acceptance_criteria"], 3);
    assert_eq!(v["risk"], "normal");

    let conn = f.state_conn();
    let (spec, risk): (String, String) = conn
        .query_row(
            "SELECT spec, risk FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(spec.contains("AC3"));
    assert_eq!(risk, "normal", "tasks.risk follows the new spec");
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'spec_updated'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["risk_from"], "important");
    assert_eq!(detail["risk_to"], "normal");
    assert_eq!(detail["goal_changed"], true);
    assert_eq!(detail["acceptance_criteria_from"], 2);
    assert_eq!(detail["acceptance_criteria_to"], 3);

    // now the rework goes through
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    assert_eq!(d["rework"].as_bool(), Some(true));

    // and update-spec is refused while the task is running
    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .assert()
        .failure();
}

/// trial 20's fix: an important-risk REJECT used to refuse `dispatch` forever,
/// because `task update-spec` never moves the task off `reject` and dispatch's
/// gate only looked at risk and status. Once a `spec_updated` postdates the
/// REJECT it answers, the rework proceeds like a low/normal one - new attempt,
/// new worktree from the previous candidate, cap and `--allow-attempt` still
/// apply - but folds nothing, because the Supervisor's rewrite replaces the
/// findings rather than sitting alongside them.
#[test]
fn an_important_risk_rework_proceeds_once_the_spec_is_rewritten_after_the_reject() {
    let f = common::fixture();
    let important = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;
    let (task_id, first_run, candidate) = rejected_task(&f, important);

    // still refused: no update-spec has happened yet
    f.cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure();

    // rewrite the spec, keeping risk important - the fold path this exercises
    // is the rewrite, not a risk downgrade (that is the older test above)
    let new_spec = f.home_path().join("rewritten-important.json");
    std::fs::write(
        &new_spec,
        r#"{"goal":"guard the ledger, rejecting a double spend before it is written","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection","AC3: covered by a test"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#,
    )
    .unwrap();
    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .assert()
        .success();

    // now the rework goes through, folding nothing
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    assert_eq!(d["rework"].as_bool(), Some(true));
    assert_eq!(d["folded_by"].as_str(), Some("spec_rewrite"));
    assert_eq!(d["folded_findings"].as_i64(), Some(0));
    assert!(
        d["warnings"].as_array().unwrap().is_empty(),
        "AC3: a spec-rewrite rework that folds zero findings does not warn, \
         it is expected there: {d}"
    );

    let second_run = d["run_id"].as_str().unwrap().to_string();
    let conn = f.state_conn();
    let run_spec: String = conn
        .query_row(
            "SELECT spec FROM runs WHERE run_id = ?1",
            [&second_run],
            |r| r.get(0),
        )
        .unwrap();
    let run_spec: serde_json::Value = serde_json::from_str(&run_spec).unwrap();
    assert_eq!(
        run_spec["rework"]["previous_run"],
        serde_json::json!(first_run)
    );
    assert_eq!(
        run_spec["rework"]["previous_candidate"],
        serde_json::json!(candidate)
    );
    assert_eq!(
        run_spec["rework"]["unmet"],
        serde_json::json!([]),
        "the rewrite replaces the findings, it does not fold them: {run_spec}"
    );
    assert_eq!(
        run_spec["rework"]["extra_findings"],
        serde_json::json!([]),
        "{run_spec}"
    );
    assert!(
        run_spec["acceptance_criteria"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().starts_with("AC3")),
        "the run's spec is the Supervisor's rewrite, not the original: {run_spec}"
    );

    // that attempt reaches review and is rejected again
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review", "{}", f.explain_task(&task_id));
    let r = f.review_start(&task_id, "claude-a", "review_reject", &["--model", "haiku"]);
    let mut wait = f.cmd();
    wait.args([
        "wait",
        "--run",
        r["run_id"].as_str().unwrap(),
        "--timeout",
        "180s",
    ]);
    f.run_ok(wait, "wait --run");
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "reject",
        "{}",
        f.explain_task(&task_id)
    );

    // the cap default is 2, and this task has already spent both attempts on
    // real reworks - raise it directly so the next check is about the risk
    // gate, not the cap (the cap has its own test above)
    f.state_conn()
        .execute(
            "UPDATE tasks SET attempt_cap = 5 WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();

    // a REJECT recorded after the only update-spec so far refuses again
    let err = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("risk 'important'"), "got {err}");
    assert!(err.contains("task update-spec"), "got {err}");

    // ...until the next update-spec answers this REJECT too
    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .assert()
        .success();
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(3));
    assert_eq!(d["folded_by"].as_str(), Some("spec_rewrite"));
}

/// Appends a `verdict` event straight into `events`, the way a real REJECT
/// (reviewer-run or `review record`) would, without needing a `risk: high`
/// task to also satisfy ingest rule 5 (a claim citing control_plane evidence)
/// just to reach `in_review` in the first place.
fn append_reject_verdict(f: &common::Fixture, task_id: &str) {
    f.state_conn()
        .execute(
            "INSERT INTO events (ts, task_id, run_id, type, detail) \
             VALUES ('2026-09-27T00:00:00Z', ?1, NULL, 'verdict', '{\"verdict\":\"REJECT\"}')",
            [task_id],
        )
        .unwrap();
}

/// The same escape hatch at `risk: high`, driven directly against the DB
/// (see `append_reject_verdict`) rather than through a real high-risk
/// implementer run, which `a_rejected_task_reworks_...`'s `hooks` scenario
/// cannot satisfy (ingest rule 5 needs a control_plane-evidence citation that
/// scenario never cites).
#[test]
fn a_high_risk_rework_also_proceeds_once_the_spec_is_rewritten_after_the_reject() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let high = r#"{"goal":"rotate the signing key","acceptance_criteria":["AC1: old key stops verifying"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"high"}"#;
    let (task_id, _) = f.create_task(high);
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    append_reject_verdict(&f, &task_id);

    f.cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .args(["--skip-spec-review", "rework fixture: the spec review gate is covered by tests/spec_review.rs"])
        .assert()
        .failure();

    let new_spec = f.home_path().join("rewritten-high.json");
    std::fs::write(
        &new_spec,
        r#"{"goal":"rotate the signing key, revoking the old one atomically","acceptance_criteria":["AC1: old key stops verifying","AC2: the rotation is logged"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"high"}"#,
    )
    .unwrap();
    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .assert()
        .success();

    let d = f.dispatch(&task_id, "claude-b", "crash", &["--skip-spec-review", "rework fixture: the spec review gate is covered by tests/spec_review.rs"]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    assert_eq!(d["rework"].as_bool(), Some(true));
    assert_eq!(d["folded_by"].as_str(), Some("spec_rewrite"));
    assert_eq!(d["folded_findings"].as_i64(), Some(0));
    assert!(d["warnings"].as_array().unwrap().is_empty(), "{d}");

    // attempt 2 crashed, so put the task back to `reject` and record a
    // second REJECT: it postdates the only update-spec so far, so dispatch
    // must refuse again until a fresh one answers it
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "interrupted");
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject', attempt_cap = 5 WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    append_reject_verdict(&f, &task_id);

    let err = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(err).unwrap();
    assert!(err.contains("risk 'high'"), "got {err}");

    f.cmd()
        .args(["task", "update-spec", &task_id, "-f"])
        .arg(&new_spec)
        .assert()
        .success();
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(3));
    assert_eq!(d["folded_by"].as_str(), Some("spec_rewrite"));
}

/// I1 (fix round 1): every decision a dispatch makes - the attempt number, the
/// worktree, the cap, the risk gate, the findings fold - comes from the task
/// status it read before it did any of them. If that status moves to a
/// *different* entry status underneath (`in_review` → `reject` is the one that
/// matters: a reviewer settling REJECT while `git worktree add` runs), the
/// transaction must refuse rather than commit a run that skipped the whole
/// rework path.
///
/// The interleave is real, not simulated: `dispatch` reads the task row before
/// it probes the profile's auth, so a parked auth probe (`FAKE_AUTH_MARKER` +
/// `FAKE_AUTH_DELAY_MS`) puts this test exactly in the gap. The marker proves
/// the read already happened, so there is no timing assumption.
#[test]
fn a_task_that_moves_to_another_entry_status_mid_dispatch_is_refused() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    // `in_review` is an entry status that reuses the attempt and folds nothing
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();

    let marker = f.home_path().join("auth-probe-started");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_thoth"))
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .current_dir(f.repo_path())
        .env("THOTH_HOME", f.home_path())
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_AUTH_MARKER", &marker)
        .env("FAKE_AUTH_DELAY_MS", "4000")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("dispatch must start");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the auth probe never started, so the task row was never read"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // the reviewer lands its REJECT verdict now, after the read, before the
    // transaction
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();

    let out = child.wait_with_output().expect("dispatch must finish");
    assert!(
        !out.status.success(),
        "dispatch must refuse a task whose entry status moved underneath it"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("state changed underneath"), "got {err}");
    assert!(
        err.contains("'in_review'") && err.contains("'reject'"),
        "the refusal must name both statuses: {err}"
    );

    assert_eq!(
        f.task_status(&task_id),
        "reject",
        "the concurrent writer's status stands; the dispatch wrote nothing"
    );
    let runs: i64 = f
        .state_conn()
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        runs, 0,
        "no run row may survive: before this fix a run at attempt 1 committed, \
         skipping the attempt bump, the cap, the risk gate and the findings fold"
    );
}

/// R3 (fix round 1): a rework that folds nothing still costs an attempt, so it
/// warns. R2: with no completed previous run there is no attempt to name and
/// no commits to build on, and the prompt must not pretend otherwise.
#[test]
fn a_rework_with_nothing_to_fold_warns_and_the_prompt_claims_no_commits() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    // straight to `reject` with no review and no run behind it
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'reject' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();

    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    assert_eq!(d["rework"].as_bool(), Some(true));
    assert_eq!(d["folded_findings"].as_i64(), Some(0));
    let warnings: Vec<String> = d["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("no reviewer findings could be folded")),
        "got {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("no REJECT review is recorded")),
        "the warning must say which of the two reasons it was: {warnings:?}"
    );

    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, _status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    let prompt = std::fs::read_to_string(
        f.home_path()
            .join("repos")
            .join(f.repo_id())
            .join("runs")
            .join(&run_id)
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("This is attempt 2"), "got {prompt}");
    assert!(
        prompt.contains("A previous attempt was reviewed and rejected"),
        "got {prompt}"
    );
    assert!(
        !prompt.contains("Attempt 0 was reviewed"),
        "there is no attempt 0: {prompt}"
    );
    assert!(
        prompt.contains("Its commits are not in this worktree"),
        "nothing completed, so there is nothing to build on: {prompt}"
    );
}

/// R4 (fix round 1): a task with no runs has spent no attempts. `--json` keeps
/// both keys (one schema) with `attempt: null`; the human line omits the
/// bracket rather than printing an attempt number that never existed.
#[test]
fn status_reports_no_attempt_at_all_before_a_task_has_run() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);

    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert!(
        v.get("attempt").is_some(),
        "the key stays present so the schema does not change: {v}"
    );
    assert!(v["attempt"].is_null(), "no runs, no attempt: {v}");
    assert_eq!(v["attempt_cap"], 2);

    let human = f
        .cmd()
        .args(["status", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert!(
        !human.contains("[attempt"),
        "no attempt bracket before the first run: {human}"
    );
}

/// `verify --at-base` on a rework run baselines the task's own base_commit,
/// not the previous attempt's candidate the rework's worktree started from.
#[test]
fn verify_at_base_on_a_rework_run_uses_the_tasks_base_commit() {
    let f = common::fixture();
    let (task_id, _first_run, candidate) = rejected_task(&f, common::Fixture::SPEC_TWO_AC);
    let base: String = f
        .state_conn()
        .query_row("SELECT base_commit FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_ne!(base, candidate, "the first attempt must have moved past the base");

    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(d["attempt"].as_i64(), Some(2));
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, _) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);

    // Passes only where HEAD is exactly `base` (`rev-parse` output compared by
    // `git diff --quiet` would also pass on an identical tree, so ask for the
    // id). AC14: a shell check is one program and plain arguments, so the
    // comparison is a script, on PATH because the baseline checkout holds only
    // what base_commit holds.
    let bin = tempfile::tempdir().unwrap();
    let shell = if cfg!(windows) {
        std::fs::write(
            bin.path().join("isbase.cmd"),
            "@echo off\r\nfor /f %%h in ('git rev-parse HEAD') do if \"%%h\"==\"%1\" exit /b 0\r\nexit /b 1\r\n",
        )
        .unwrap();
        "cmd"
    } else {
        let p = bin.path().join("isbase");
        std::fs::write(&p, "#!/bin/sh\ntest \"$(git rev-parse HEAD)\" = \"$1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        "sh"
    };
    let mut entries = vec![bin.path().to_path_buf()];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(entries).unwrap();
    let cmd = format!("isbase {base}");
    // P7-1b: verify runs approved checks, not what the worker declared.
    f.cmd()
        .args(["checks", "add", "at-base", "--cmd", &cmd, "--shell", shell, "--json"])
        .assert()
        .success();
    f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();

    let out = f
        .cmd()
        .env("PATH", &path)
        .args(["verify", &run_id, "--at-base", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["at_commit"], base.as_str());
    assert_eq!(v["failures"], 0, "got {v:#}");
}
