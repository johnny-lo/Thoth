mod common;

/// Runs the `hooks` implementer scenario to completion and returns
/// `(task_id, implementer_run_id)` with the task sitting at `in_review`.
fn candidate_ready(f: &common::Fixture, spec: &str) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    (task_id, run_id)
}

/// brief §13: `review start` + `review_pass` → the task passes, a `reviews` row
/// and a `verdict` event land, and `review show` reports what the reviewer
/// opened.
#[test]
fn review_pass_settles_the_task_pass_and_review_show_reports_the_findings() {
    let f = common::fixture();
    let (task_id, implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    let r = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(r["target_run_id"], serde_json::json!(implementer_run.clone()));
    assert_eq!(r["allowed_tools"], "Bash,Read,Glob,Grep");
    assert!(
        !r["allowed_tools"].as_str().unwrap().contains("Write"),
        "brief §15: the reviewer may not have Write or Edit"
    );
    assert_eq!(r["worktree_reused"], false);

    // the review worktree is a detached checkout of the candidate
    let review_wt = std::path::PathBuf::from(r["worktree"].as_str().unwrap());
    assert!(
        review_wt.to_string_lossy().ends_with("-a1-review"),
        "got {review_wt:?}"
    );
    let head = common::git(&review_wt, &["rev-parse", "HEAD"]);
    assert_eq!(head, r["candidate_commit"].as_str().unwrap());
    let sym = std::process::Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(&review_wt)
        .status()
        .unwrap();
    assert!(!sym.success(), "the review worktree must be detached");
    // and the reviewer was handed the index
    let index: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(review_wt.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index.len(), 5);

    // --- wait --run (Open question 6's ruling) ---------------------------
    // The Supervisor's chain: `review start --json` hands back the reviewer
    // run_id, and `wait --run` blocks on that run rather than on the task -
    // which `wait <task>` cannot do, because a reviewed task sits at
    // `in_review` and that is already wait-terminal.
    let out = f
        .cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim()).unwrap();
    assert_eq!(v["timed_out"], false);
    assert_eq!(v["run_id"], serde_json::json!(reviewer_run.clone()));
    assert_eq!(v["role"], "reviewer");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["verdict"], "PASS", "a reviewer run's answer is its verdict");
    assert_eq!(v["task_status"], "pass");

    // human mode prints the verdict too
    let out = f
        .cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("PASS"), "got {text}");

    // a non-reviewer run reports no verdict, and says which role it was
    let out = f
        .cmd()
        .args(["wait", "--run", &implementer_run, "--timeout", "30s", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["role"], "implementer");
    assert_eq!(v["status"], "completed");
    assert_eq!(
        v["verdict"],
        serde_json::Value::Null,
        "only a reviewer run has a verdict"
    );

    // the two modes are mutually exclusive, and one of them is required
    f.cmd()
        .args(["wait", &task_id, "--run", &reviewer_run])
        .assert()
        .failure();
    f.cmd().arg("wait").assert().failure();

    let status = f.task_status(&task_id);
    assert_eq!(status, "pass");

    // --- H2: a reviewer run must never write `tasks.status` ---------------
    // Only the implementer dispatch may move the task to `running`. Before this
    // was fixed, `run_worker_inner`'s unconditional `set_task_status(Running)`
    // flipped the task `in_review -> running` for the whole review, which every
    // status poll saw and which a crashed reviewer left behind for good.
    let conn = f.state_conn();
    let task_running_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE task_id = ?1 AND type = 'status_change'
               AND detail LIKE '%\"entity\":\"task\"%'
               AND detail LIKE '%\"to\":\"running\"%'",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        task_running_events, 1,
        "only the implementer dispatch may move the task to running"
    );

    // --- the reviewer really did not touch the code under review ----------
    // `REVIEWER_ALLOWED_TOOLS` and the detached worktree are the mechanism; this
    // is the outcome. `.agent/` is the one thing that may appear, because the
    // control plane publishes the index there and the reviewer writes its report
    // there.
    let porcelain = common::git(&review_wt, &["status", "--porcelain"]);
    for line in porcelain.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.contains(".agent"),
            "the reviewer modified something other than .agent/: {line}\nfull status:\n{porcelain}"
        );
    }

    // the reviews row (`conn` is the connection opened above)
    let (verdict, target, reviewer, findings): (String, String, String, String) = conn
        .query_row(
            "SELECT verdict, target_run_id, reviewer_run_id, findings FROM reviews",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(verdict, "PASS");
    assert_eq!(target, implementer_run);
    assert_eq!(reviewer, reviewer_run);
    let findings: serde_json::Value = serde_json::from_str(&findings).unwrap();
    let criteria = findings["criteria"].as_array().unwrap();
    assert_eq!(criteria.len(), 2, "both acceptance criteria are judged");
    for c in criteria {
        assert_eq!(c["verdict"], "met");
    }

    // the reviewer run itself
    let (role, rstatus, tools): (String, String, String) = conn
        .query_row(
            "SELECT role, status, allowed_tools FROM runs WHERE run_id = ?1",
            [&reviewer_run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(role, "reviewer");
    assert_eq!(rstatus, "completed");
    assert_eq!(tools, "Bash,Read,Glob,Grep");

    // no new evidence: Open question 4, a reviewer's own commands are not
    // evidence about the implementation
    assert_eq!(f.evidence_rows(&task_id).len(), 5);
    let reviewer_evidence: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM evidence WHERE run_id = ?1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reviewer_evidence, 0);
    assert!(
        !f.home_path()
            .join("repos")
            .join(f.repo_id())
            .join("runs")
            .join(&reviewer_run)
            .join("inbox")
            .exists(),
        "a reviewer run is not hooked, so it has no inbox"
    );

    // the events
    let kinds = f.events_of(&task_id);
    assert!(kinds.contains(&"review_started".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"verdict".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"evidence_opened".to_string()), "{kinds:?}");
    assert!(!kinds.contains(&"review_invalid".to_string()), "{kinds:?}");

    // --- review show -----------------------------------------------------
    // BRIEF-1: the implementer's claims are shown only with --full.
    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--full", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["verdict"], "PASS");
    assert_eq!(v["review_count"], 1);
    assert_eq!(v["reviewer_run_id"], serde_json::json!(reviewer_run.clone()));
    let cited: Vec<String> = v["cited"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(cited, vec!["E001".to_string()]);
    // brief §8's indicator: the reviewer opened what it cited, so nothing is
    // flagged
    assert_eq!(
        v["opened"],
        serde_json::json!(["E001"]),
        "the fake reviewer opens the code it cites"
    );
    assert_eq!(v["unopened_citation"], serde_json::json!([]));
    // and the implementer's claims travel with the review
    let claims = v["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2);
    let cited_claim = claims
        .iter()
        .find(|c| c["kind"] == "test_result")
        .expect("the test_result claim");
    assert_eq!(cited_claim["evidence"], serde_json::json!(["E001"]));

    // `status <task>` surfaces the verdict too (brief §9)
    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["latest_verdict"], "PASS");
    assert_eq!(v["review_count"], 1);
    // Whole-branch review Minor 5: a Phase 2 task has both an implementer and
    // a reviewer run, and `status` used to print neither's role.
    let roles: Vec<&str> = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["role"].as_str().expect("every run row carries its role"))
        .collect();
    assert!(roles.contains(&"implementer"), "got {roles:?}");
    assert!(roles.contains(&"reviewer"), "got {roles:?}");
    // ...and the human line says so too
    let human = f
        .cmd()
        .args(["status", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("reviewer"), "got {human}");
}

/// brief §13: `review_reject` → the task is `reject`, with an unmet criterion
/// and an extra finding recorded for the rework to fold in (Task 8).
#[test]
fn review_reject_settles_the_task_reject_with_an_unmet_criterion() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    let r = f.review_start(&task_id, "claude-a", "review_reject", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    let status = f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30);
    assert_eq!(status, "reject");

    let conn = f.state_conn();
    let (verdict, findings): (String, String) = conn
        .query_row("SELECT verdict, findings FROM reviews", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(verdict, "REJECT");
    let findings: serde_json::Value = serde_json::from_str(&findings).unwrap();
    let criteria = findings["criteria"].as_array().unwrap();
    assert_eq!(criteria.len(), 2);
    assert_eq!(criteria[0]["verdict"], "unmet");
    assert!(criteria[0]["note"].as_str().unwrap().contains("no captured evidence"));
    assert_eq!(criteria[1]["verdict"], "met");
    assert_eq!(findings["extra_findings"].as_array().unwrap().len(), 1);

    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'verdict' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["verdict"], "REJECT");
    assert_eq!(detail["extra_findings"], 1);
}

/// H7a: a Claude reviewer labels its unmet criterion and extra finding with
/// a fix size and the prompt's first category, and `review show --json`
/// hands both back.
#[test]
fn a_claude_reviewers_fix_size_and_category_reach_review_show() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    let r = f.review_start(&task_id, "claude-a", "review_reject", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    let status = f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30);
    assert_eq!(status, "reject");

    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let unmet = &v["findings"]["criteria"][0];
    assert_eq!(unmet["verdict"], "unmet", "{v}");
    assert_eq!(unmet["fix_size"], "S", "{v}");
    assert_eq!(unmet["category"], "spec_mismatch", "{v}");
    let extra = &v["findings"]["extra_findings"][0];
    assert_eq!(extra["fix_size"], "S", "{v}");
    assert_eq!(extra["category"], "spec_mismatch", "{v}");
    let met = v["findings"]["criteria"][1].as_object().unwrap();
    assert!(!met.contains_key("fix_size") && !met.contains_key("category"), "{v}");
}

/// brief §5: a reviewer process that succeeds but writes an unusable report
/// leaves the task `unverified`, never `pass`, and says why.
#[test]
fn a_broken_review_json_settles_the_task_unverified() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC);

    let r = f.review_start(&task_id, "claude-a", "review_bad", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    let status = f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30);
    assert_eq!(status, "unverified");

    let conn = f.state_conn();
    let reviews: i64 = conn
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reviews, 0, "an unusable report is not a review");
    let rstatus: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&reviewer_run], |r| r.get(0))
        .unwrap();
    assert_eq!(rstatus, "completed", "the harness itself succeeded");

    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'review_invalid'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"stage\":\"schema\""), "got {detail}");
    assert!(detail.contains("not valid JSON"), "got {detail}");

    // `review show` says there is nothing yet rather than inventing a verdict
    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["review_count"], 0);
    assert_eq!(v["verdict"], serde_json::Value::Null);
    // L10: the same key set as the populated case, so a script needs one shape
    for key in [
        "task_id", "review_count", "review_id", "verdict", "target_run_id",
        "reviewer_run_id", "created_at", "findings", "cited", "opened",
        "unopened_citation", "claims",
    ] {
        assert!(v.get(key).is_some(), "missing key {key} in {v}");
    }
}

/// Whole-branch review Important 2: a reviewer that marks every criterion
/// `met` and cites nothing at all has demonstrated nothing. It used to settle
/// the task `pass` - terminal, trusted, hidden from `status` - because
/// `parse_review` only required a citation for an `unmet` criterion. It now
/// travels the `review_invalid` path like any other unusable report.
#[test]
fn an_all_met_review_that_cites_no_evidence_settles_the_task_unverified() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    let r = f.review_start(&task_id, "claude-a", "review_uncited", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();

    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "unverified",
        "an uncited PASS must never settle the task pass"
    );

    let conn = f.state_conn();
    let reviews: i64 = conn
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reviews, 0, "an unsupported verdict is not a review");
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'review_invalid'",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"stage\":\"schema\""), "got {detail}");
    assert!(
        detail.contains("is met but cites no evidence"),
        "the reason must say which rule the report broke: {detail}"
    );
}

/// H2's product failure, pinned: a review that never produced a verdict must
/// leave the task exactly where it was, so `thoth review start` can simply be
/// run again. Before the fix the task was left `running` — a status neither
/// `review start`, `dispatch` nor `task update-spec` accepts, i.e. stranded.
#[test]
fn a_crashed_reviewer_leaves_the_task_in_review_so_the_review_can_be_retried() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC);

    // `crash` emits no result event at all, so the run is classified interrupted
    let r = f.review_start(&task_id, "claude-a", "crash", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();

    let out = f
        .cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["status"], "interrupted");
    assert_eq!(v["interrupted_reason"], "crash");
    assert_eq!(v["verdict"], serde_json::Value::Null);
    assert_eq!(
        v["task_status"], "in_review",
        "the task must not have moved with the review"
    );

    assert_eq!(f.task_status(&task_id), "in_review");
    let conn = f.state_conn();
    let reviews: i64 = conn
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reviews, 0);
    // the run's own outcome is still on the record, even though the task's is not
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'status_change'
             ORDER BY seq DESC LIMIT 1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["to"], "interrupted");
    assert_eq!(
        detail["task_status_written"],
        serde_json::Value::Null,
        "the settle deliberately wrote no task status"
    );
    // and no task-level status_change came out of the review at all
    let task_changes_after_review: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND type = 'status_change'
             AND detail LIKE '%\"entity\":\"task\"%'",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(task_changes_after_review, 0);

    // ...so the review can just be started again, and this time it lands
    let r = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "pass"
    );
    // the retry reused the review worktree rather than failing on it
    assert_eq!(r["worktree_reused"], true);
}

/// Fix round 1 (I1): `resume` re-opened the hole this task closed. An
/// interrupted reviewer run satisfies every one of resume's preconditions, and
/// resuming it wrote `tasks.status = running` — moving the task out of
/// `in_review` for the whole review and, if that review then reached no verdict,
/// stranding it on a status both `review start` and `dispatch` refuse.
#[test]
fn resume_refuses_a_reviewer_run_and_writes_nothing() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC);

    let r = f.review_start(&task_id, "claude-a", "crash", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();

    // this run satisfies every other precondition `resume` checks...
    let conn = f.state_conn();
    let (rstatus, session, started): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, harness_session_id, started_at FROM runs WHERE run_id = ?1",
            [&reviewer_run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "interrupted");
    assert!(session.is_some(), "resume needs a session id, and this run has one");
    assert!(started.is_some(), "and a started_at, which it also has");
    let events_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();

    // ...and is refused anyway, because it is a review
    let out = f
        .cmd()
        .args(["resume", &reviewer_run])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_eq!(out.status.code(), Some(1), "the same exit code resume's other refusals use");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("reviewer run"), "got {err}");
    assert!(err.contains("review start"), "the refusal must say what to do instead: {err}");

    // --json refusals keep the machine-readable shape
    let out = f
        .cmd()
        .args(["resume", &reviewer_run, "--json"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("not resumable"), "got {v}");

    // and neither attempt wrote anything at all
    assert_eq!(f.task_status(&task_id), "in_review");
    let rstatus_now: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&reviewer_run], |r| r.get(0))
        .unwrap();
    assert_eq!(rstatus_now, "interrupted", "a refused resume must not re-queue the run");
    let events_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events_after, events_before, "no event either");
}

/// Fix round 1 (I3): the H2 rule keeps the task at `in_review` for the whole
/// review, which removes the mutex every other launch path leans on — so a
/// second `review start` used to succeed, share the first one's worktree and
/// `.agent/review.json`, and leave the task's verdict to whichever run settled
/// last.
#[test]
fn a_second_review_start_is_refused_while_one_is_in_flight() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    // `slow` holds the reviewer open for a minute, so the second attempt lands
    // while the first run row is genuinely still queued/running.
    let first = f.review_start(&task_id, "claude-a", "slow", &["--model", "haiku"]);
    let first_run = first["run_id"].as_str().unwrap().to_string();

    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .args(["review", "start", &task_id, "--profile", "claude-a", "--model", "haiku"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(
        err.contains(&first_run),
        "the refusal must name the in-flight review: {err}"
    );

    // no second run row, and the task did not move
    let conn = f.state_conn();
    let reviewer_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND role = 'reviewer'",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reviewer_runs, 1, "the refusal must not leave a run row behind");
    assert_eq!(f.task_status(&task_id), "in_review");

    // stop the in-flight review. Cancelling a *review* says nothing about the
    // task under review, which is the `cancel.rs` half of the same H2 rule.
    f.cmd().args(["cancel", &first_run]).assert().success();
    f.cmd()
        .args(["wait", "--run", &first_run, "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(
        f.task_status(&task_id),
        "in_review",
        "a cancelled review must leave the task exactly where it was"
    );

    // ...and with nothing in flight any more, a review starts again
    let second = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    f.cmd()
        .args(["wait", "--run", second["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "pass"
    );
}

/// Fix round 1 (I3's related check): the review worktree is reused across
/// retries of the same (task_id, attempt), so a `review.json` left by a reviewer
/// that crashed after writing it would still be on disk when the next reviewer
/// is spawned — and the runner would ingest it as the new reviewer's verdict.
#[test]
fn a_stale_review_json_is_cleared_before_the_next_reviewer_starts() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC);

    let first = f.review_start(&task_id, "claude-a", "crash", &["--model", "haiku"]);
    assert_eq!(
        first["cleared_stale_report"], false,
        "a worktree that has never held a report has nothing to clear"
    );
    f.cmd()
        .args(["wait", "--run", first["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(f.task_status(&task_id), "in_review");

    // Stand in for a reviewer that wrote its verdict and then died: the file is
    // on disk, and the retry reuses this very directory.
    let review_wt = std::path::PathBuf::from(first["worktree"].as_str().unwrap());
    let stale = review_wt.join(".agent").join("review.json");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(
        &stale,
        r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]}]}}"#,
    )
    .unwrap();

    // `ok` is a reviewer process that exits cleanly and writes no review.json at
    // all, so whatever is on disk when it finishes is what would be ingested.
    let second = f.review_start(&task_id, "claude-a", "ok", &["--model", "haiku"]);
    assert_eq!(second["worktree_reused"], true);
    assert_eq!(
        second["cleared_stale_report"], true,
        "the stale report must be cleared before the reviewer is spawned"
    );
    f.cmd()
        .args(["wait", "--run", second["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();

    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "unverified",
        "a reviewer that wrote nothing must not inherit an earlier one's PASS"
    );
    let conn = f.state_conn();
    let reviews: i64 = conn
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reviews, 0, "and no reviews row was written from the stale file");
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'review_invalid' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"stage\":\"read\""), "got {detail}");
    assert!(!stale.exists(), "and nothing put the stale report back");
}

/// Open question 6's remaining exit code: a run that nothing is going to settle
/// makes `wait --run` time out with exit 3, the same code the task form uses.
/// `seed_run` inserts a `queued` run row with no runner behind it, so this needs
/// no process and no waiting beyond the timeout itself.
#[test]
fn wait_run_times_out_with_exit_three_on_a_run_nothing_will_settle() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (_task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);

    let out = f
        .cmd()
        .args(["wait", "--run", &run_id, "--timeout", "2s", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["timed_out"], true);
    assert_eq!(v["status"], "queued");
    assert_eq!(v["verdict"], serde_json::Value::Null);
    // Whole-branch review Minor 1: the timeout branch used to omit three of
    // the keys `docs/phase2/usage.md` promises unconditionally, so a script
    // reading `task_status` had to know which branch produced the document.
    for key in [
        "ok", "run_id", "task_id", "role", "status", "interrupted_reason", "exit_code",
        "verdict", "task_status", "timed_out",
    ] {
        assert!(v.get(key).is_some(), "missing key {key} in {v}");
    }
    assert_eq!(v["interrupted_reason"], serde_json::Value::Null);
    assert_eq!(v["exit_code"], serde_json::Value::Null);
    assert_eq!(v["task_status"], serde_json::Value::Null);

    // an unknown run id is an error (exit 1), not a timeout
    f.cmd()
        .args(["wait", "--run", "01NOSUCHRUN", "--timeout", "2s"])
        .assert()
        .failure();
}

#[test]
fn review_start_refuses_a_task_that_is_not_in_review_or_has_no_candidate() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    // queued: nothing has been implemented yet
    f.cmd()
        .args(["review", "start", &task_id, "--profile", "claude-a"])
        .assert()
        .failure();
    assert_eq!(f.task_status(&task_id), "queued");

    // in_review but with no completed implementer run behind it
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    f.cmd()
        .args(["review", "start", &task_id, "--profile", "claude-a"])
        .assert()
        .failure();

    // and an unknown profile fails too - though this particular task still
    // fails at the missing candidate, which `start` checks first
    f.cmd()
        .args(["review", "start", &task_id, "--profile", "nope"])
        .assert()
        .failure();

    // no run row was created by any of the three refusals
    let runs: i64 = f
        .state_conn()
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(runs, 0);
}

#[test]
fn review_start_refuses_a_profile_whose_stored_harness_is_not_supported() {
    let f = common::fixture();
    let (task_id, _) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);
    // This stored harness is outside the registered values.
    f.global_conn()
        .execute("UPDATE profiles SET harness='gemini' WHERE profile_id='claude-a'", [])
        .unwrap();
    let runs_before: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();

    f.cmd()
        .args(["review", "start", &task_id, "--profile", "claude-a"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("gemini"))
        .stderr(predicates::str::contains("only claude-code, codex"))
        .stderr(predicates::str::contains("are supported"));

    let runs_after: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs_after, runs_before);
    assert_eq!(f.task_status(&task_id), "in_review");
}

/// Starts a gated `review_pass` reviewer, waits until it is running, lets
/// `sabotage` damage what the runner will read at settle time, releases the
/// reviewer and waits for it. Returns `(task_id, implementer_run, reviewer_run)`.
fn gated_review(
    f: &common::Fixture,
    sabotage: impl FnOnce(&rusqlite::Connection, &str, &str),
) -> (String, String, String) {
    let (task_id, implementer_run) = candidate_ready(f, common::Fixture::SPEC_TWO_AC);
    let gate = f.home_path().join("release-the-reviewer");
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_REVIEW_GATE", &gate)
        .args([
            "review", "start", &task_id, "--profile", "claude-a", "--model", "haiku", "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let s: String = f
            .state_conn()
            .query_row("SELECT status FROM runs WHERE run_id = ?1", [&reviewer_run], |r| r.get(0))
            .unwrap();
        if s == "running" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the reviewer never reached running");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    sabotage(&f.state_conn(), &implementer_run, &reviewer_run);

    std::fs::write(&gate, "go").unwrap();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    (task_id, implementer_run, reviewer_run)
}

/// The reviewer's own `review_started` event is unreadable at settle time, but
/// the task still has a completed implementer run: the verdict must land against
/// that run instead of dying on the `target_run_id` foreign key.
#[test]
fn an_unreadable_review_target_falls_back_to_the_latest_completed_implementer_run() {
    let f = common::fixture();
    let (task_id, implementer_run, reviewer_run) = gated_review(&f, |conn, _, reviewer| {
        conn.execute(
            "UPDATE events SET detail = 'not json' WHERE run_id = ?1 AND type = 'review_started'",
            [reviewer],
        )
        .unwrap();
    });
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "pass"
    );
    let conn = f.state_conn();
    let (verdict, target): (String, String) = conn
        .query_row(
            "SELECT verdict, target_run_id FROM reviews WHERE reviewer_run_id = ?1",
            [&reviewer_run],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(verdict, "PASS");
    assert_eq!(target, implementer_run);
}

/// Neither the review's own event nor a completed implementer run can name the
/// target: the review is invalid and says so, rather than reporting a
/// foreign-key failure.
#[test]
fn a_review_with_no_determinable_target_run_is_invalid_and_says_so() {
    let f = common::fixture();
    let (task_id, _implementer_run, _reviewer_run) = gated_review(&f, |conn, implementer, reviewer| {
        conn.execute(
            "DELETE FROM events WHERE run_id = ?1 AND type = 'review_started'",
            [reviewer],
        )
        .unwrap();
        conn.execute("UPDATE runs SET status = 'failed' WHERE run_id = ?1", [implementer])
            .unwrap();
    });
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "unverified"
    );
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE type = 'review_invalid'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("target run could not be determined"), "got {detail}");
    assert!(!detail.to_lowercase().contains("foreign key"), "got {detail}");
}

/// The diagnosis helper is what makes a flaky review-flow test name its own
/// cause: it must surface the stage of the latest `review_invalid` event.
#[test]
fn explain_task_names_the_review_invalid_stage() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, common::Fixture::SPEC_TWO_AC);

    let r = f.review_start(&task_id, "claude-a", "review_uncited", &["--model", "haiku"]);
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30);

    let text = f.explain_task(&task_id);
    assert!(text.contains("\"stage\":\"schema\""), "got {text}");
    assert!(text.contains("is met but cites no evidence"), "got {text}");
}

/// A helper-run thoth command that fails must say why in the panic: exit code,
/// stderr and the newest events, even when the process printed nothing.
#[test]
fn a_failing_helper_command_panics_with_its_stderr_and_the_latest_events() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut c = f.cmd();
        c.args(["wait", "NOSUCHTASK", "--timeout", "1s"]);
        f.run_ok(c, "wait");
    }));
    let payload = caught.expect_err("a failing helper command must panic");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("panic payload is a string");
    assert!(message.contains("exited Some(1)"), "exit code: {message}");
    assert!(
        message.contains("no id matches 'NOSUCHTASK'"),
        "stderr text: {message}"
    );
    assert!(
        message.contains("--- latest events ---"),
        "latest events marker: {message}"
    );
}
