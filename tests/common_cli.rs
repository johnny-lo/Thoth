//! TESTS-1 (AC1): the fixture's reading helpers (`task_status`, `events_of`,
//! `evidence_rows`, `explain_task`, `explain_run`, `explain_recent`,
//! `wait_for_task_status`) and the two new ones (`run_of`, `events_json`) go
//! through the CLI's `--json` documents. This file opens no SQLite: it checks
//! the helpers against the same CLI output the operator sees.

mod common;

use serde_json::Value;

/// The panic message of `f`, which must panic.
fn panic_message(f: impl FnOnce()) -> String {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .expect_err("the closure must panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .expect("panic payload is a string")
}

fn json_of(out: Vec<u8>) -> Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn cli_json(f: &common::Fixture, args: &[&str]) -> Value {
    json_of(f.cmd().args(args).assert().success().get_output().stdout.clone())
}

/// Before `init` nothing can be read: every explain helper still returns a
/// string that says what failed, and none of them panics.
#[test]
fn the_explain_helpers_do_not_panic_before_init() {
    let f = common::fixture();
    let recent = f.explain_recent();
    assert!(recent.starts_with("--- latest events ---\n"), "{recent}");
    assert!(recent.contains("exited 1"), "{recent}");
    let task = f.explain_task("01NOSUCHTASK00000000000000");
    assert!(task.starts_with("task 01NOSUCHTASK00000000000000: "), "{task}");
    assert!(task.contains("exited 1"), "{task}");
    let run = f.explain_run("01NOSUCHRUN000000000000000");
    assert!(run.starts_with("run 01NOSUCHRUN000000000000000: (not found"), "{run}");
    assert!(run.contains("exited 1"), "{run}");
}

/// TESTS-1 (review round 1, error_path): the views the explain helpers read a
/// CLI document through refuse a document that lacks a key or has it with the
/// wrong type, naming the command, the key and the response. A missing
/// `events` or `runs` is an error, never an empty list; a run row without a
/// string `run_dir` is an error, never the current directory.
#[test]
fn the_document_views_refuse_a_missing_or_mistyped_key() {
    use serde_json::json;

    let ok = json!({"events": [
        {"seq": 1, "ts": "2026-10-22T00:00:00Z", "task_id": "T", "run_id": null, "type": "task_created", "detail": null},
        {"seq": 2, "ts": "2026-10-22T00:00:01Z", "task_id": "T", "run_id": "R", "type": "status_change", "detail": {"from": "queued"}}
    ], "last_seq": 2});
    let events = common::events_view(&ok, "events --since 0 --json").unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].run_id, None);
    assert_eq!(events[0].ty, "task_created");
    assert!(events[0].detail.is_null());
    assert_eq!(events[1].seq, 2);
    assert_eq!(events[1].ts, "2026-10-22T00:00:01Z");
    assert_eq!(events[1].run_id.as_deref(), Some("R"));
    assert_eq!(events[1].detail["from"], "queued");
    assert_eq!(common::events_view(&json!({"events": []}), "events --json").unwrap(), vec![]);

    for (doc, key) in [
        (json!({"last_seq": 0}), "events"),
        (json!({"events": null}), "events"),
        (json!({"events": 5}), "events"),
        (json!({"events": {"seq": 1}}), "events"),
        (json!({"events": [{"seq": 1, "ts": "t", "run_id": null, "detail": null}]}), "type"),
        (json!({"events": [{"seq": 1, "ts": "t", "type": 4, "run_id": null, "detail": null}]}), "type"),
        (json!({"events": [{"ts": "t", "type": "x", "run_id": null, "detail": null}]}), "seq"),
        (json!({"events": [{"seq": "1", "ts": "t", "type": "x", "run_id": null, "detail": null}]}), "seq"),
        (json!({"events": [{"seq": 1, "type": "x", "run_id": null, "detail": null}]}), "ts"),
        (json!({"events": [{"seq": 1, "ts": null, "type": "x", "run_id": null, "detail": null}]}), "ts"),
        (json!({"events": [{"seq": 1, "ts": "t", "type": "x", "detail": null}]}), "run_id"),
        (json!({"events": [{"seq": 1, "ts": "t", "type": "x", "run_id": 7, "detail": null}]}), "run_id"),
        (json!({"events": [{"seq": 1, "ts": "t", "type": "x", "run_id": null}]}), "detail"),
    ] {
        let err = common::events_view(&doc, "events --since 0 --task T --json").unwrap_err();
        assert!(err.contains("events --since 0 --task T --json"), "{key}: {err}");
        assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        assert!(err.contains(&doc.to_string()), "{key}: {err}");
    }

    let ok = json!({"task_id": "T", "status": "in_review", "runs": [
        {"run_id": "R1", "role": "implementer", "status": "completed", "interrupted_reason": null,
         "error_text": null, "run_dir": "C:/some dir/runs/R1", "exit_code": 0, "worker_pid": null},
        {"run_id": "R2", "role": "reviewer", "status": "failed", "interrupted_reason": "timeout",
         "error_text": "boom", "run_dir": "/tmp/r2", "exit_code": null}
    ]});
    let runs = common::runs_view(&ok, "status T --json").unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].run_id, "R1");
    assert_eq!(runs[0].role, "implementer");
    assert_eq!(runs[0].status, "completed");
    assert_eq!(runs[0].interrupted_reason, None);
    assert_eq!(runs[0].error_text, None);
    assert_eq!(runs[0].run_dir, "C:/some dir/runs/R1");
    assert_eq!(runs[1].interrupted_reason.as_deref(), Some("timeout"));
    assert_eq!(runs[1].error_text.as_deref(), Some("boom"));
    assert_eq!(runs[1].run_dir, "/tmp/r2");
    assert_eq!(common::runs_view(&json!({"runs": []}), "status T --json").unwrap(), vec![]);

    let full = |drop: &str, replace: Option<Value>| -> Value {
        let mut row = json!({"run_id": "R", "role": "implementer", "status": "completed",
            "interrupted_reason": null, "error_text": null, "run_dir": "d"});
        match replace {
            Some(v) => row[drop] = v,
            None => {
                row.as_object_mut().unwrap().remove(drop);
            }
        }
        json!({"task_id": "T", "runs": [row]})
    };
    for (doc, key) in [
        (json!({"task_id": "T", "status": "queued"}), "runs"),
        (json!({"runs": null}), "runs"),
        (json!({"runs": {}}), "runs"),
        (full("run_dir", None), "run_dir"),
        (full("run_dir", Some(json!(null))), "run_dir"),
        (full("run_dir", Some(json!(5))), "run_dir"),
        (full("run_id", None), "run_id"),
        (full("run_id", Some(json!(null))), "run_id"),
        (full("status", None), "status"),
        (full("status", Some(json!(["completed"]))), "status"),
        (full("role", None), "role"),
        (full("interrupted_reason", None), "interrupted_reason"),
        (full("interrupted_reason", Some(json!(3))), "interrupted_reason"),
        (full("error_text", None), "error_text"),
        (full("error_text", Some(json!({}))), "error_text"),
    ] {
        let err = common::runs_view(&doc, "status T --json").unwrap_err();
        assert!(err.contains("status T --json"), "{key}: {err}");
        assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        assert!(err.contains(&doc.to_string()), "{key}: {err}");
    }
}

/// TESTS-1 (review round 1, error_path): the reading helpers' failure path.
/// A task the CLI does not know makes each helper panic with the command,
/// its exit code, its stderr and the newest events; before `init` the same,
/// so a wrong task id or a missing repo is never read as "no data".
#[test]
fn the_reading_helpers_panic_with_the_command_its_stderr_and_the_latest_events() {
    let f = common::fixture();
    let unknown = "01NOSUCHTASK00000000000000";

    let message = panic_message(|| {
        f.task_status(unknown);
    });
    assert!(message.contains("thoth status exited Some(1)"), "{message}");
    assert!(message.contains("--- latest events ---"), "{message}");

    f.cmd().arg("init").assert().success();
    let (task, _) = f.create_task(common::Fixture::SPEC);
    type Helper<'a> = Box<dyn Fn() + 'a>;
    let helpers: Vec<(&str, Helper)> = vec![
        ("status", Box::new(|| {
            f.task_status(unknown);
        })),
        ("events", Box::new(|| {
            f.events_of(unknown);
        })),
        ("events", Box::new(|| {
            f.events_json(unknown);
        })),
        ("evidence list", Box::new(|| {
            f.evidence_rows(unknown);
        })),
        ("status", Box::new(|| {
            f.run_of(unknown, "01NOSUCHRUN000000000000000");
        })),
        ("status", Box::new(|| {
            f.wait_for_task_status(unknown, &["pass"], 5);
        })),
    ];
    for (what, helper) in helpers {
        let message = panic_message(helper);
        assert!(message.contains(&format!("thoth {what} exited Some(1)")), "{what}: {message}");
        assert!(message.contains(&format!("no id matches '{unknown}'")), "{what}: {message}");
        assert!(message.contains("--- latest events ---"), "{what}: {message}");
        assert!(message.contains(" task_created  "), "the newest events are shown: {what}: {message}");
    }
    // the known task still reads
    assert_eq!(f.task_status(&task), "queued");
    assert_eq!(f.events_of(&task), vec!["task_created".to_string()]);
}

/// A freshly initialised repo has no event and no run: `explain_recent`
/// prints the marker and says so; an unknown run or task is reported, not
/// panicked on.
#[test]
fn an_empty_repo_explains_as_empty() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    assert_eq!(cli_json(&f, &["events", "--json"])["events"], serde_json::json!([]));
    let recent = f.explain_recent();
    assert_eq!(recent, "--- latest events ---\nnewest run: (no event names a run)\n");
    let run = f.explain_run("01NOSUCHRUN000000000000000");
    assert!(run.starts_with("run 01NOSUCHRUN000000000000000: (not found"), "{run}");
    assert!(run.contains("01NOSUCHRUN000000000000000"), "{run}");
    let task = f.explain_task("01NOSUCHTASK00000000000000");
    assert!(task.starts_with("task 01NOSUCHTASK00000000000000: "), "{task}");
}

/// A task that has not run yet: one `task_created` event, no evidence, and
/// the task's events are its own, not another task's.
#[test]
fn a_new_task_has_its_own_events_and_no_evidence() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (first, _) = f.create_task(common::Fixture::SPEC);
    let (second, _) = f.create_task(common::Fixture::SPEC_TWO_AC);

    assert_eq!(f.evidence_rows(&first), Vec::<Value>::new());
    assert_eq!(f.evidence_rows(&second), Vec::<Value>::new());
    assert_eq!(f.events_of(&first), vec!["task_created".to_string()]);
    assert_eq!(f.events_of(&second), vec!["task_created".to_string()]);
    let first_events = f.events_json(&first);
    let second_events = f.events_json(&second);
    assert_eq!(first_events.len(), 1);
    assert_eq!(first_events[0]["task_id"], first.as_str());
    assert_eq!(second_events[0]["task_id"], second.as_str());
    assert!(first_events[0]["seq"].as_i64().unwrap() < second_events[0]["seq"].as_i64().unwrap());
    assert_eq!(f.task_status(&first), "queued");
    assert_eq!(f.task_status(&second), "queued");
    assert_eq!(cli_json(&f, &["status", &first, "--json"])["status"], "queued");

    let text = f.explain_task(&first);
    assert_eq!(text, "review_invalid: (none)\nresult_invalid: (none)\nreviewer run: (none)\n");
    let recent = f.explain_recent();
    assert!(recent.contains(" task_created  "), "{recent}");
    assert!(recent.ends_with("newest run: (no event names a run)\n"), "{recent}");
}

/// The whole flow the helpers serve: a `hooks` run, then a `review_pass`
/// review, and after each step what the helpers return matches the CLI.
#[test]
fn the_helpers_follow_the_cli_through_a_run_and_a_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let (other, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "hooks", &[]);
    let implementer = d["run_id"].as_str().unwrap().to_string();

    // wait_for_task_status waits for the wanted set, sleeping between polls
    let started = std::time::Instant::now();
    assert_eq!(f.wait_for_task_status(&task, &["in_review", "unverified"], 180), "in_review");
    assert_eq!(f.task_status(&task), "in_review");
    assert_eq!(cli_json(&f, &["status", &task, "--json"])["status"], "in_review");

    // in_review is not returned when it is not wanted: the wait keeps going
    // until its deadline, then panics with the task's explanation
    let waited = std::time::Instant::now();
    let message = panic_message(|| {
        f.wait_for_task_status(&task, &["pass"], 1);
    });
    assert!(waited.elapsed() >= std::time::Duration::from_secs(1), "{message}");
    assert!(
        message.contains(&format!("task {task} never reached any of [\"pass\"] within 1s; it is 'in_review'")),
        "{message}"
    );
    assert!(message.contains("review_invalid: (none)\nresult_invalid: (none)\nreviewer run: (none)\n"), "{message}");
    assert!(started.elapsed() < std::time::Duration::from_secs(180));

    // events: the task's own, in seq order, repeats kept, the other task's absent
    let events = f.events_json(&task);
    assert!(events.len() > 3, "{events:?}");
    for e in &events {
        assert_eq!(e["task_id"], task.as_str(), "{e}");
    }
    let seqs: Vec<i64> = events.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    let types = f.events_of(&task);
    assert_eq!(types, events.iter().map(|e| e["type"].as_str().unwrap().to_string()).collect::<Vec<_>>());
    assert_eq!(types[0], "task_created");
    assert!(types.iter().filter(|t| *t == "status_change").count() >= 2, "repeats are kept: {types:?}");
    assert!(types.contains(&"evidence_added".to_string()), "{types:?}");
    assert_eq!(f.events_of(&other), vec!["task_created".to_string()]);
    assert!(f.events_json(&other).iter().all(|e| e["task_id"] == other.as_str()));

    // evidence: exactly the nine keys, the CLI's values (nulls included),
    // integer bytes, parsed detail, code order
    let rows = f.evidence_rows(&task);
    let listed = cli_json(&f, &["evidence", "list", &task, "--json"]);
    let listed = listed["evidence"].as_array().unwrap();
    assert_eq!(rows.len(), listed.len());
    assert_eq!(rows.len(), 5, "three commands, the diff and the commit: {rows:#?}");
    let mut keys_wanted = vec!["code", "kind", "collected_by", "label", "sha256", "bytes", "blob_path", "detail", "run_id"];
    keys_wanted.sort();
    for (row, cli) in rows.iter().zip(listed) {
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, keys_wanted, "{row}");
        for key in &keys_wanted {
            assert_eq!(row[key], cli[key], "{key} of {row}");
        }
        assert!(row["bytes"].is_i64(), "{row}");
        assert!(row["detail"].is_object(), "{row}");
        assert_eq!(row["run_id"], implementer.as_str());
        assert!(cli.get("exit_code").is_some() && cli.get("created_at").is_some(), "{cli}");
    }
    let codes: Vec<&str> = rows.iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert_eq!(codes, ["E001", "E002", "E003", "E004", "E005"]);
    assert_eq!(rows[0]["detail"]["exit_code"], 0);
    assert_eq!(rows[1]["detail"]["exit_code"], 3);
    assert_eq!(rows[0]["bytes"], 12);
    assert_eq!(f.evidence_rows(&other), Vec::<Value>::new());

    // run_of: the named run of the named task; an unknown run panics with
    // the status document
    let row = f.run_of(&task, &implementer);
    assert_eq!(row["run_id"], implementer.as_str());
    assert_eq!(row["task_id"], task.as_str());
    assert_eq!(row["role"], "implementer");
    assert_eq!(row["status"], "completed");
    assert!(row["run_dir"].is_string());
    let message = panic_message(|| {
        f.run_of(&task, "01NOSUCHRUN000000000000000");
    });
    assert!(message.contains(&format!("task {task} has no run 01NOSUCHRUN000000000000000")), "{message}");
    assert!(message.contains(&implementer), "the whole status document is shown: {message}");

    // explain_run: the row's status and the log tails; a log longer than 20
    // lines keeps only its tail, a missing one is (absent)
    let run_dir = std::path::PathBuf::from(row["run_dir"].as_str().unwrap());
    let long: String = (1..=30).map(|i| format!("stderr line {i}\n")).collect();
    std::fs::write(run_dir.join("stderr.log"), long).unwrap();
    let _ = std::fs::remove_file(run_dir.join("runner-error.log"));
    let text = f.explain_run(&implementer);
    assert!(text.starts_with(&format!("run {implementer}: status=completed interrupted_reason=(null) error_text=(null)\n")), "{text}");
    assert!(text.contains("--- runner-error.log (last 20 lines) ---\n(absent)\n"), "{text}");
    assert!(text.contains("--- stderr.log (last 20 lines) ---\nstderr line 11\n"), "{text}");
    assert!(text.ends_with("stderr line 30\n"), "{text}");
    assert!(!text.contains("stderr line 10\n"), "{text}");
    assert_eq!(text.matches("stderr line ").count(), 20, "{text}");
    // the newest run of the repo is this one
    let recent = f.explain_recent();
    assert!(recent.starts_with("--- latest events ---\n"), "{recent}");
    assert!(recent.contains(&format!("run {implementer}: status=completed")), "{recent}");
    assert!(recent.contains("stderr line 30\n"), "{recent}");

    // a review: the reviewer launched after the implementer is the newest
    // run, whatever later events the implementer's run gets
    let r = f.review_start(&task, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer, "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(f.wait_for_task_status(&task, &["pass", "reject", "unverified"], 30), "pass");

    // a terminal task is hidden from the task list but still read by TASK
    let open = cli_json(&f, &["status", "--json"]);
    let open: Vec<String> = open["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap().to_string())
        .collect();
    assert!(!open.contains(&task), "pass is hidden from the list: {open:?}");
    assert_eq!(f.task_status(&task), "pass");
    assert_eq!(f.run_of(&task, &reviewer)["role"], "reviewer");
    assert_eq!(f.run_of(&task, &reviewer)["status"], "completed");

    let text = f.explain_task(&task);
    assert!(text.starts_with("review_invalid: (none)\nresult_invalid: (none)\n"), "{text}");
    assert!(text.contains(&format!("run {reviewer}: status=completed interrupted_reason=(null) error_text=(null)\n")), "{text}");
    assert!(!text.contains(&format!("run {implementer}:")), "{text}");
    let recent = f.explain_recent();
    assert!(recent.contains(&format!("run {reviewer}: status=completed")), "{recent}");
    assert!(!recent.contains(&format!("run {implementer}:")), "{recent}");
    // the ten newest events, newest first, with ts, type, run_id and detail
    let all = cli_json(&f, &["events", "--json"]);
    let all = all["events"].as_array().unwrap();
    assert!(all.len() > 10);
    let listed: Vec<&str> = recent.lines().skip(1).take(10).collect();
    for (line, e) in listed.iter().zip(all.iter().rev()) {
        let wanted = format!(
            "{} {} {} {}",
            e["ts"].as_str().unwrap(),
            e["type"].as_str().unwrap(),
            e["run_id"].as_str().unwrap_or(""),
            if e["detail"].is_null() { String::new() } else { e["detail"].to_string() }
        );
        assert_eq!(*line, wanted);
    }
    let text = f.explain_run(&reviewer);
    assert!(text.starts_with(&format!("run {reviewer}: status=completed")), "{text}");
    let events = f.events_json(&task);
    assert!(events.iter().any(|e| e["type"] == "verdict" && e["run_id"] == reviewer.as_str()), "{events:?}");
}
