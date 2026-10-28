mod common;

use common::Fixture;
use std::path::{Path, PathBuf};

/// Runs the `hooks` implementer scenario to completion and returns
/// `(task_id, implementer_run_id)` with the task sitting at `in_review`.
/// Mirrors `tests/review.rs`'s private `candidate_ready` (each integration
/// test file is its own compilation unit, so there is no conflict).
fn candidate_ready(f: &Fixture, spec: &str) -> (String, String) {
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

/// Writes `contents` into the fixture's home dir at `name` and returns the path.
fn write_review_json(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let path = f.home_path().join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// Runs `thoth review record <task> -f <file> --json`, asserts a clean
/// exit, and returns the parsed stdout.
fn record_ok(f: &Fixture, task_id: &str, file: &Path) -> serde_json::Value {
    let out = f
        .cmd()
        .args(["review", "record", task_id, "-f"])
        .arg(file)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// Runs `thoth review record <task> -f <file>`, asserts a non-zero exit,
/// and returns stderr.
fn record_err(f: &Fixture, task_id: &str, file: &Path) -> String {
    let out = f
        .cmd()
        .args(["review", "record", task_id, "-f"])
        .arg(file)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(out).unwrap()
}

/// `COUNT(*)` of `task_id`'s events of exactly one `type`.
fn count_events(f: &Fixture, task_id: &str, event_type: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND type = ?2",
            [task_id, event_type],
            |r| r.get(0),
        )
        .unwrap()
}

/// `COUNT(*)` of every `reviews` row in the fixture's database. Each fixture
/// is its own fresh database, so this is equivalent to "this task's reviews"
/// for every test in this file (each uses exactly one task).
fn count_reviews(f: &Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM reviews", [], |r| r.get(0))
        .unwrap()
}

/// Fix round 1 (F4): the three event types a refusal must never add to,
/// snapshotted before the refusing call and compared after.
fn refusal_counts(f: &Fixture, task_id: &str) -> (i64, i64, i64) {
    (
        count_events(f, task_id, "review_invalid"),
        count_events(f, task_id, "verdict"),
        count_events(f, task_id, "status_change"),
    )
}

/// Case 1 + case 9 (brief): a valid PASS settles the task `pass`, is visible
/// through `review show` and `status`, the `verdict` event carries
/// `source: external`, and the `--json` output has exactly the six keys the
/// brief specifies.
#[test]
fn a_valid_pass_settles_the_task_pass_and_is_visible_everywhere() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-pass.json", review_json);

    let v = record_ok(&f, &task_id, &file);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["review_id", "source", "target_run_id", "task_id", "task_status", "verdict"],
        "the --json output must have exactly these keys: {v}"
    );
    assert_eq!(v["task_id"], task_id);
    assert_eq!(v["verdict"], "PASS");
    assert_eq!(v["task_status"], "pass");
    assert_eq!(v["source"], "external");
    assert!(v["review_id"].as_str().is_some());
    assert!(v["target_run_id"].as_str().is_some());

    assert_eq!(f.task_status(&task_id), "pass");

    // `review show --json` reports the external review's shape.
    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(shown["verdict"], "PASS");
    assert_eq!(shown["source"], "external");
    assert_eq!(shown["reviewer_run_id"], serde_json::Value::Null);
    assert_eq!(shown["opened"], serde_json::Value::Null);
    assert_eq!(shown["unopened_citation"], serde_json::Value::Null);

    // the verdict event carries source: external and a null reviewer_run_id
    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'verdict' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["source"], "external");
    assert_eq!(detail["reviewer_run_id"], serde_json::Value::Null);

    // `status <task>` surfaces the verdict too
    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_v: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(status_v["latest_verdict"], "PASS");
}

/// Case 2: a valid REJECT settles the task `reject`, and the fold `dispatch`
/// performs on rework sees the external review's unmet note - proving the
/// REJECT fold reads through `target_run_id`, not `reviewer_run_id`.
#[test]
fn a_valid_reject_settles_the_task_reject_and_folds_into_the_rework() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the retry never fires"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[{"severity":"minor","note":"a stray TODO","evidence":["E001"]}]}}"#;
    let file = write_review_json(&f, "review-reject.json", review_json);

    let v = record_ok(&f, &task_id, &file);
    assert_eq!(v["verdict"], "REJECT");
    assert_eq!(v["task_status"], "reject");
    assert_eq!(f.task_status(&task_id), "reject");

    // dispatch again: attempt 2, and the new prompt folds in the external
    // review's unmet note.
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(d["attempt"], 2);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, _status) = f.wait_for(&task_id, "60s");
    assert_eq!(code, 0);

    let prompt_path = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("runs")
        .join(&run_id)
        .join("prompt.txt");
    let prompt = std::fs::read_to_string(&prompt_path).unwrap();
    assert!(
        prompt.contains("## Previous attempt was rejected"),
        "got {prompt}"
    );
    assert!(prompt.contains("the retry never fires"), "got {prompt}");
}

/// `review show --json`'s stored findings.
fn shown_findings(f: &Fixture, task_id: &str) -> serde_json::Value {
    let out = f
        .cmd()
        .args(["review", "show", task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    shown["findings"].clone()
}

/// Whether `fix_size` or `category` appears as a key anywhere in `v`.
fn has_label_key(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => m
            .iter()
            .any(|(k, sub)| k == "fix_size" || k == "category" || has_label_key(sub)),
        serde_json::Value::Array(a) => a.iter().any(has_label_key),
        _ => false,
    }
}

/// H7a: `fix_size` and `category` are normalised on the way in and stored.
#[test]
fn fix_size_and_category_are_recorded_normalised() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the retry never fires","fix_size":" m ","category":"Docs"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[{"severity":"minor","note":"a stray TODO","evidence":["E001"],"fix_size":"L","category":"other"}]}}"#;
    let file = write_review_json(&f, "review-labelled.json", review_json);
    let v = record_ok(&f, &task_id, &file);
    assert_eq!(v["task_status"], "reject");

    let findings = shown_findings(&f, &task_id);
    assert_eq!(findings["criteria"][0]["fix_size"], "M", "{findings}");
    assert_eq!(findings["criteria"][0]["category"], "docs", "{findings}");
    assert_eq!(findings["extra_findings"][0]["fix_size"], "L", "{findings}");
    assert_eq!(findings["extra_findings"][0]["category"], "other", "{findings}");
}

/// H7a: the labels are optional, and a document without them is stored
/// without them.
#[test]
fn a_review_without_fix_size_or_category_is_recorded_without_them() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the retry never fires"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[{"severity":"minor","note":"a stray TODO","evidence":["E001"]}]}}"#;
    let file = write_review_json(&f, "review-unlabelled.json", review_json);
    record_ok(&f, &task_id, &file);

    let findings = shown_findings(&f, &task_id);
    assert_eq!(findings["criteria"].as_array().unwrap().len(), 2, "{findings}");
    assert!(!has_label_key(&findings), "{findings}");
}

/// H7a: a fix size outside S/M/L is refused at the schema stage, and the
/// task stays where it was.
#[test]
fn an_unknown_fix_size_is_refused_at_the_schema_stage() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"the retry never fires","fix_size":"XL"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-xl.json", review_json);
    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("stage schema"), "got {err}");
    assert!(err.contains("fix_size"), "got {err}");

    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(status["status"], "in_review", "{status}");
}

/// Case 3 (fix round 1 / F4): a `met` criterion with an empty `evidence`
/// array fails the shared validator's schema stage (the same `UncitedMet`
/// rule a reviewer-run report is held to) - non-zero exit, exactly **one**
/// `review_invalid` event with `source: external`, zero `reviews` rows, and
/// zero `verdict`/`status_change` events added by the refusal.
#[test]
fn a_met_criterion_with_no_evidence_is_refused_and_writes_nothing() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]},
        {"id":"AC2","verdict":"met","evidence":[]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-uncited.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("cites no evidence"), "got {err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews, "no reviews row must be written");

    assert_eq!(
        count_events(&f, &task_id, "review_invalid"),
        before_invalid + 1,
        "exactly one review_invalid event for this refusal"
    );
    assert_eq!(
        count_events(&f, &task_id, "verdict"),
        before_verdict,
        "an invalid document must not write a verdict event"
    );
    assert_eq!(
        count_events(&f, &task_id, "status_change"),
        before_status_change,
        "an invalid document must not change tasks.status"
    );

    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE task_id = ?1 AND type = 'review_invalid'
             ORDER BY seq DESC LIMIT 1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"source\":\"external\""), "got {detail}");
    assert!(detail.contains("\"stage\":\"schema\""), "got {detail}");
}

/// Case 4 (fix round 1 / F4): citing a code that is not evidence of this task
/// is refused at the citation stage, with the same exactly-one-event proof as
/// case 3.
#[test]
fn a_citation_to_evidence_that_does_not_exist_is_refused() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E999"]},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-badcode.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("E999"), "got {err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews);
    assert_eq!(count_events(&f, &task_id, "review_invalid"), before_invalid + 1);
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);

    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE task_id = ?1 AND type = 'review_invalid'
             ORDER BY seq DESC LIMIT 1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"stage\":\"citation\""), "got {detail}");
}

/// Case 5 (fix round 1 / F4): a missing acceptance criterion is refused at
/// the coverage stage, with the same exactly-one-event proof as case 3.
#[test]
fn a_missing_acceptance_criterion_is_refused() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);

    // SPEC_TWO_AC has AC1 and AC2; this document only judges AC1.
    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-missing-ac.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("AC2"), "got {err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews);
    assert_eq!(count_events(&f, &task_id, "review_invalid"), before_invalid + 1);
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);

    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE task_id = ?1 AND type = 'review_invalid'
             ORDER BY seq DESC LIMIT 1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("\"stage\":\"coverage\""), "got {detail}");
}

/// Case 6 (fix round 1 / F1, F4): a task that has never reached `in_review`
/// is refused, naming the status found, and this is a **precondition**
/// refusal - it must write no `reviews` row and no event of any of the three
/// kinds a successful record or an invalid document would write.
#[test]
fn a_task_that_is_not_in_review_is_refused_naming_the_status() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (task_id, _base_commit) = f.create_task(Fixture::SPEC);

    // Passes validation trivially (no citations to resolve), so the refusal
    // below is unambiguously about the task's status, not the document.
    let review_json = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"not started yet"}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-not-in-review.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("queued"), "got {err}");
    assert_eq!(f.task_status(&task_id), "queued");
    assert_eq!(count_reviews(&f), before_reviews);
    assert_eq!(
        count_events(&f, &task_id, "review_invalid"),
        before_invalid,
        "a precondition refusal must not write a review_invalid event"
    );
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);
}

/// Case 7 (fix round 1 / F4): a live run on the task is refused, naming the
/// run, and (being a precondition refusal) writes none of the three event
/// kinds either. A Thoth reviewer run (`slow` scenario) keeps the task
/// `in_review` with a live run behind it, which is exactly the state this
/// guard exists for.
#[test]
fn a_live_run_on_the_task_is_refused_naming_the_run() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC);

    let live = f.review_start(&task_id, "claude-a", "slow", &["--model", "haiku"]);
    let live_run = live["run_id"].as_str().unwrap().to_string();

    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-live.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(
        err.contains(&live_run),
        "the refusal must name the in-flight run: {err}"
    );

    assert_eq!(count_reviews(&f), before_reviews);
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_events(&f, &task_id, "review_invalid"), before_invalid);
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);

    // clean up the still-live reviewer run so the process does not leak
    // past this test.
    f.cmd().args(["cancel", &live_run]).assert().success();
    f.cmd()
        .args(["wait", "--run", &live_run, "--timeout", "180s"])
        .assert()
        .success();
}

/// Case 8: a file starting with a UTF-8 BOM (what `Set-Content -Encoding
/// utf8` writes on stock Windows PowerShell 5.1) is accepted.
#[test]
fn a_file_starting_with_a_utf8_bom_is_accepted() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC);

    let json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let mut bytes = vec![0xEFu8, 0xBB, 0xBF];
    bytes.extend_from_slice(json.as_bytes());
    let path = f.home_path().join("review-bom.json");
    std::fs::write(&path, &bytes).unwrap();

    let v = record_ok(&f, &task_id, &path);
    assert_eq!(v["verdict"], "PASS");
    assert_eq!(f.task_status(&task_id), "pass");
}

/// Case F5 (fix round 1): a target run with no `candidate_commit` is
/// refused, in the same style `review start` refuses one, and (being a
/// precondition refusal) writes nothing.
///
/// Not reachable through the fake harness's existing scenarios:
/// `candidate_commit` only comes up `None` when the runner's own
/// `rev_parse_head` on the worktree fails after an otherwise clean exit (a
/// missing or corrupted worktree), which none of them produce. This
/// constructs the state directly instead - dispatch normally to get a real
/// completed implementer run, then null out its own `candidate_commit` to
/// stand in for that failure, the same way `tests/review.rs`'s
/// `review_start_refuses_a_task_that_is_not_in_review_or_has_no_candidate`
/// forces `tasks.status` directly to reach a precondition `review start`
/// cannot otherwise be driven into.
#[test]
fn a_target_run_with_no_candidate_commit_is_refused() {
    let f = common::fixture();
    let (task_id, implementer_run) = candidate_ready(&f, Fixture::SPEC);

    f.state_conn()
        .execute(
            "UPDATE runs SET candidate_commit = NULL WHERE run_id = ?1",
            [&implementer_run],
        )
        .unwrap();

    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-no-candidate.json", review_json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let err = record_err(&f, &task_id, &file);
    assert!(err.contains("candidate_commit"), "got {err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews, "nothing must be written");
    assert_eq!(count_events(&f, &task_id, "review_invalid"), before_invalid);
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);
}

/// Case F6 (fix round 1): a UTF-16LE file (what stock Windows PowerShell
/// 5.1's `Out-File`/`>` write by default) is refused with a message naming
/// the encoding and saying to save as UTF-8, not a generic "invalid utf-8"
/// error - and, being a read-time refusal, writes nothing.
#[test]
fn a_utf16le_file_is_refused_with_a_clear_message() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC);

    let json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let mut bytes = vec![0xFFu8, 0xFE];
    for unit in json.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let path = f.home_path().join("review-utf16le.json");
    std::fs::write(&path, &bytes).unwrap();

    let before_reviews = count_reviews(&f);

    let err = record_err(&f, &task_id, &path);
    assert!(err.contains("UTF-16LE"), "must name the encoding: got {err}");
    assert!(
        err.to_lowercase().contains("utf-8"),
        "must say what to do instead: got {err}"
    );
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews);
}

/// Case F7 (fix round 1): invoking `record` with `THOTH_RUN_ID` set - as
/// it would be inside a thoth-launched worker or reviewer process - is
/// refused, and writes nothing. Not a security boundary (v6 §16: no sandbox,
/// and a process can unset the variable), just a guard against a worker or
/// reviewer running `review record` on its own task by mistake.
#[test]
fn a_record_invoked_with_thoth_run_id_set_is_refused() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC);

    let json = r#"{"verdict":"PASS","findings":{"criteria":[
        {"id":"AC1","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = write_review_json(&f, "review-run-id-guard.json", json);

    let before_reviews = count_reviews(&f);
    let (before_invalid, before_verdict, before_status_change) = refusal_counts(&f, &task_id);

    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01SOMERUNIDSOMERUNIDSOME1")
        .args(["review", "record", &task_id, "-f"])
        .arg(&file)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("THOTH_RUN_ID"), "got {err}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(count_reviews(&f), before_reviews, "nothing must be written");
    assert_eq!(count_events(&f, &task_id, "review_invalid"), before_invalid);
    assert_eq!(count_events(&f, &task_id, "verdict"), before_verdict);
    assert_eq!(count_events(&f, &task_id, "status_change"), before_status_change);
}
