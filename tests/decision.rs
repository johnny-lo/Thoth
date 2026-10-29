mod common;

// Linux support: the checks below run in the platform's own shell (`cmd` on
// Windows, `sh` elsewhere; see `SHELL`). AC14: a shell check is one program
// and plain arguments, so whatever needs shell syntax is one of the scripts
// `commit_scripts` puts in the fixture repo, and a check names it (`script`).

/// An absolute directory outside every worktree.
#[cfg(windows)]
const OUTSIDE_ABS: &str = "C:/Windows";
#[cfg(not(windows))]
const OUTSIDE_ABS: &str = "/etc";

/// The scripts every `settled_run` repo holds, as (stem, Windows body, Linux
/// body). `ex N` exits N; `say X` prints X; `mark` writes `marker.txt` in its
/// cwd; `long` runs far longer than the 1s `--timeout` it is used with;
/// `detached` exits at once, leaving a detached child that still holds its
/// pipes; `needscand` passes only where `cand.txt` exists (the run's worktree
/// has it, a checkout of the base commit does not); `args A B` prints
/// `[A] [B]`.
const SCRIPTS: &[(&str, &str, &str)] = &[
    ("ex", "exit /b %1", "exit \"$1\""),
    ("say", "echo %1", "echo \"$1\""),
    ("mark", "echo x> marker.txt", "echo x > marker.txt"),
    ("long", "ping -n 30 127.0.0.1", "sleep 30"),
    ("detached", "start /b ping -n 15 127.0.0.1", "sleep 15 &"),
    ("needscand", "if exist cand.txt (exit /b 0) else (exit /b 4)", "test -f cand.txt"),
    ("args", "echo [%1] [%2]", "echo \"[$1] [$2]\""),
];

/// Commits `SCRIPTS` (`STEM.cmd` / executable `STEM.sh`), plus a PowerShell
/// script on Windows and a bash script on Linux, to the fixture repo, so the
/// main tree, every run's worktree and a baseline checkout hold the same
/// bytes. The main tree's copies are then checked out again, so git's line
/// ending conversion (if any) is applied to them exactly as to a worktree.
fn commit_scripts(f: &common::Fixture) {
    let repo = f.repo_path();
    let mut names = Vec::new();
    for (stem, win, unix) in SCRIPTS {
        if cfg!(windows) {
            let name = format!("{stem}.cmd");
            std::fs::write(repo.join(&name), format!("@echo off\r\n{win}\r\n")).unwrap();
            names.push(name);
        } else {
            let name = format!("{stem}.sh");
            std::fs::write(repo.join(&name), format!("#!/bin/sh\n{unix}\n")).unwrap();
            names.push(name);
        }
    }
    if cfg!(windows) {
        let body = "if (Test-Path Cargo.toml) { exit 0 } else { exit 7 }\r\n";
        std::fs::write(repo.join("pscargo.ps1"), body).unwrap();
        names.push("pscargo.ps1".into());
    } else {
        std::fs::write(repo.join("bashy.sh"), "#!/bin/bash\n[[ -f Cargo.toml ]]\n").unwrap();
        names.push("bashy.sh".into());
    }
    #[cfg(unix)]
    for n in &names {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join(n), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut add = vec!["add"];
    add.extend(names.iter().map(String::as_str));
    common::git(&repo, &add);
    common::git(&repo, &["commit", "-m", "check scripts"]);
    for n in &names {
        std::fs::remove_file(repo.join(n)).unwrap();
    }
    let mut checkout = vec!["checkout", "HEAD", "--"];
    checkout.extend(names.iter().map(String::as_str));
    common::git(&repo, &checkout);
}

/// The check command that runs script `stem` from `SCRIPTS` with `args`.
fn script(stem: &str, args: &str) -> String {
    let program = if cfg!(windows) { format!(r".\{stem}.cmd") } else { format!("./{stem}.sh") };
    format!("{program} {args}").trim_end().to_string()
}

/// v6 §6.4 and §10.7: the write path for a long-lived decision. One line, one
/// event, no commit.
#[test]
fn decision_add_appends_a_dated_line_and_records_an_event() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();

    let out = f
        .cmd()
        .args(["decision", "add", "use SQLite, not Postgres", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["created_file"], true);
    assert_eq!(v["committed"], false);
    // v6 §6.4 rides on AGENTS.md, which this throwaway repo does not have
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1);
    assert!(v["warnings"][0].as_str().unwrap().contains("AGENTS.md"));

    let path = f.repo_path().join("decisions.md");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# Decisions"), "got {text}");
    let bullets: Vec<&str> = text.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(bullets.len(), 1);
    assert!(bullets[0].ends_with(": use SQLite, not Postgres"), "got {}", bullets[0]);
    assert_eq!(bullets[0].len(), "- 2026-09-16: use SQLite, not Postgres".len());

    // a second decision appends rather than replacing, and creates nothing
    let out = f
        .cmd()
        .args(["decision", "add", "worktrees live outside the repo", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["created_file"], false);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().filter(|l| l.starts_with("- ")).count(), 2);

    // nothing was committed
    assert!(
        common::git(&f.repo_path(), &["status", "--porcelain"]).contains("decisions.md"),
        "decision add must leave the file uncommitted"
    );

    // --file goes where it is told, relative to the repo root
    f.cmd()
        .args(["decision", "add", "keep blobs forever", "--file", "docs/adr.md"])
        .assert()
        .success();
    let adr = std::fs::read_to_string(f.repo_path().join("docs").join("adr.md")).unwrap();
    assert!(adr.contains("keep blobs forever"));

    let conn = f.state_conn();
    let details: Vec<String> = conn
        .prepare("SELECT detail FROM events WHERE type = 'decision_added' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert_eq!(details.len(), 3);
    assert!(details[0].contains("use SQLite"));
    assert!(details[2].contains("adr.md"));

    // an empty decision is refused
    f.cmd().args(["decision", "add", "   "]).assert().failure();
}

/// The shell the platform-specific command lines in this file are written for.
#[cfg(windows)]
const SHELL: &str = "cmd";
#[cfg(not(windows))]
const SHELL: &str = "sh";

/// Writes `marker.txt` in its cwd if it ever runs.
#[cfg(windows)]
const WRITES_MARKER: &str = "echo x> marker.txt";
#[cfg(not(windows))]
const WRITES_MARKER: &str = "echo x > marker.txt";

fn json_of(out: Vec<u8>) -> serde_json::Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `checks add ID --cmd CMD --shell SHELL [extra...]`.
fn add_check_as(f: &common::Fixture, id: &str, cmd: &str, shell: &str, extra: &[&str]) {
    f.cmd()
        .args(["checks", "add", id, "--cmd", cmd, "--shell", shell])
        .args(extra)
        .args(["--json"])
        .assert()
        .success();
}

/// A check in this platform's shell (`cmd` / `sh`).
fn add_check(f: &common::Fixture, id: &str, cmd: &str) {
    add_check_as(f, id, cmd, SHELL, &[]);
}

/// `checks approve` (the human's step), from the main tree or a run's worktree.
fn approve(f: &common::Fixture, run: Option<&str>) {
    let mut c = f.cmd();
    c.args(["checks", "approve", "--reason", "test approval", "--json"]);
    if let Some(r) = run {
        c.args(["--run", r]);
    }
    c.assert().success();
}

/// Points an existing check's cwd somewhere `checks add` would refuse, the
/// way only a direct write to state.db can.
fn set_check_cwd(f: &common::Fixture, id: &str, cwd: &str) {
    f.state_conn()
        .execute("UPDATE checks SET cwd = ?2 WHERE check_id = ?1", [id, cwd])
        .unwrap();
}

/// init, one profile, a task from `spec`, one `hooks` run that settled in
/// review. Returns (task_id, run_id, base_commit, worktree).
fn settled_run(f: &common::Fixture, spec: &str) -> (String, String, String, std::path::PathBuf) {
    commit_scripts(f);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, base) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    let worktree = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    (task_id, run_id, base, worktree)
}

/// `verify RUN [args...] --json`, expecting exit 0 (`ok`) or 1.
fn verify(f: &common::Fixture, run_id: &str, args: &[&str], ok: bool) -> serde_json::Value {
    let mut c = f.cmd();
    c.args(["verify", run_id]).args(args).arg("--json");
    let a = c.assert();
    let a = if ok { a.success() } else { a.code(1) };
    json_of(a.get_output().stdout.clone())
}

/// `verify RUN [args...]` without `--json`: the human text.
fn verify_human(f: &common::Fixture, run_id: &str, args: &[&str]) -> String {
    let out = f.cmd().args(["verify", run_id]).args(args).assert().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

fn verify_rows(f: &common::Fixture, task_id: &str, prefix: &str) -> Vec<serde_json::Value> {
    f.evidence_rows(task_id)
        .into_iter()
        .filter(|r| r["label"].as_str().unwrap().starts_with(prefix))
        .collect()
}

fn verify_ran_details(f: &common::Fixture) -> Vec<serde_json::Value> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE type = 'verify_ran' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

/// Submits `requests` as the run's declared verification_requests.
fn resubmit_requests(f: &common::Fixture, task_id: &str, run_id: &str, requests: serde_json::Value) {
    let file = f.home_path().join("resubmit-requests.json");
    std::fs::write(
        &file,
        serde_json::json!({
            "task_id": task_id, "status": "completed", "summary": "s",
            "verification_requests": requests
        })
        .to_string(),
    )
    .unwrap();
    f.cmd()
        .args(["submit", "-f"])
        .arg(&file)
        .args(["--run", run_id, "--json"])
        .assert()
        .success();
}

/// brief §10 / v6 §2.2 rule 4, P7-1b: `verify` runs the repo's approved checks
/// and records each as control_plane evidence, without touching the task's
/// status; what the worker declared is only listed.
#[test]
fn verify_runs_the_approved_checks_as_control_plane_evidence() {
    let f = common::fixture();
    let (task_id, run_id, _base, worktree) = settled_run(&f, common::Fixture::SPEC);
    assert_eq!(f.evidence_rows(&task_id).len(), 5);
    let (say, ex3) = (script("say", "verify-one"), script("ex", "3"));
    add_check(&f, "c1-echo", &say);
    add_check(&f, "c2-exit", &ex3);
    approve(&f, None);
    let approved_hex: String = f
        .state_conn()
        .query_row(
            "SELECT approved_fingerprint FROM checks WHERE check_id = 'c1-echo'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // --dry-run lists without running or recording
    let v = verify(&f, &run_id, &["--dry-run"], true);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["source"], "approved checks (all)");
    let listed = v["results"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["check_id"], "c1-echo");
    assert_eq!(listed[0]["cmd"], say.as_str());
    assert_eq!(listed[0]["shell"], SHELL);
    assert_eq!(listed[0]["fingerprint_ok"], true);
    assert_eq!(listed[1]["cmd"], ex3.as_str());
    // the fake worker's own two requests, listed for reference only
    assert_eq!(
        v["worker_declared"],
        serde_json::json!([{"cmd": "echo verify-one", "cwd": "."}, {"cmd": "exit 3", "cwd": "."}])
    );
    assert_eq!(f.evidence_rows(&task_id).len(), 5, "a dry run records nothing");

    // the real run: one check succeeds, one exits 3, and both are recorded
    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["ok"], false);
    assert_eq!(v["failures"], 1);
    assert_eq!(v["source"], "approved checks (all)");
    let results = v["results"].as_array().unwrap();
    assert_eq!(results[0]["exit_code"], 0);
    assert_eq!(results[0]["evidence"], "E006");
    assert_eq!(results[0]["check_id"], "c1-echo");
    assert_eq!(results[0]["shell"], SHELL);
    assert_eq!(results[1]["exit_code"], 3);
    assert_eq!(results[1]["evidence"], "E007");
    assert_eq!(v["worker_declared"].as_array().unwrap().len(), 2);

    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), 7, "two new rows: {rows:#?}");
    let verify_rows = verify_rows(&f, &task_id, "verify: ");
    assert_eq!(verify_rows.len(), 2);
    assert_eq!(verify_rows[0]["label"], format!("verify: {say}"));
    // Fix round 1 / R1: `collected_by` is always "control_plane" for every
    // row `insert_row` writes, hook-captured ones included - it is not what
    // distinguishes a `verify` row. The label prefix and these are.
    assert_eq!(verify_rows[0]["detail"]["event"], "control_plane_rerun");
    assert_eq!(verify_rows[0]["detail"]["blob_source"], "control_plane");
    assert_eq!(verify_rows[0]["detail"]["truncated_inline"], false);
    assert_eq!(results[0]["output_truncated"], false);
    // P7-1b / AC3: which approved check ran, and against which approval
    let detail = &verify_rows[0]["detail"];
    assert_eq!(detail["check_id"], "c1-echo");
    assert_eq!(detail["shell"], SHELL);
    assert_eq!(detail["fingerprint"], approved_hex.as_str());
    assert_eq!(detail["fingerprint_ok"], true);
    assert_eq!(detail["inputs_checked"], true);
    assert!(detail["approved_at"].is_string(), "got {detail:#}");
    // L7: the argv that actually ran is on the record next to the cmd; AC14
    // (b): the program goes to the shell as the absolute path pinned
    let program = detail["program"].as_str().unwrap();
    assert_eq!(
        std::fs::canonicalize(program).unwrap(),
        std::fs::canonicalize(worktree.join(if cfg!(windows) { "say.cmd" } else { "say.sh" })).unwrap()
    );
    let argv: Vec<&str> = detail["argv"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    #[cfg(windows)]
    assert_eq!(argv, ["cmd", "/D", "/V:OFF", "/S", "/C", "\"\"%THOTH_CHECK_PROGRAM%\" verify-one\""]);
    #[cfg(not(windows))]
    assert_eq!(argv, ["sh", "-c", "\"$0\" \"$@\"", program, "verify-one"]);
    assert_eq!(verify_rows[1]["detail"]["exit_code"], 3);
    let stdout = std::fs::read_to_string(verify_rows[0]["blob_path"].as_str().unwrap()).unwrap();
    assert!(stdout.contains("verify-one"), "got {stdout:?}");

    // the task's status is deliberately unchanged
    assert_eq!(f.task_status(&task_id), "in_review");

    // the event and the republished index
    let detail = verify_ran_details(&f).pop().unwrap();
    assert_eq!(detail["count"], 2);
    assert_eq!(detail["failures"], 1);
    assert_eq!(detail["checks"], serde_json::json!(["c1-echo", "c2-exit"]));
    assert_eq!(detail["worker_declared"].as_array().unwrap().len(), 2);
    let index: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index.len(), 7);
    assert_eq!(index[6]["exit_code"], 3);

    // the human text lists the declared commands after the results
    let human = verify_human(&f, &run_id, &["--dry-run"]);
    let at = human.find("worker declared (reference only):").expect(&human);
    assert!(human.find(&format!("c2-exit: {ex3}")).unwrap() < at, "{human}");
    assert!(human[at..].contains("echo verify-one"), "{human}");

    // and a worker may now cite the re-run: `submit` the same document again
    // with the new code attached
    let resubmit = f.home_path().join("resubmit.json");
    std::fs::write(
        &resubmit,
        serde_json::json!({
            "task_id": task_id,
            "status": "completed",
            "summary": "resubmitted citing the control plane's own re-run",
            "claims": [{"kind": "test_result", "text": "verified by the control plane",
                        "evidence": ["E006"]}]
        })
        .to_string(),
    )
    .unwrap();
    f.cmd()
        .args(["submit", "-f"])
        .arg(&resubmit)
        .args(["--run", &run_id, "--json"])
        .assert()
        .success();
    assert_eq!(f.task_status(&task_id), "in_review");
}

/// Fix round 1 / I1, kept by P7-1b: an absolute cwd, a `..`, and a directory
/// that does not exist must each be refused as data - not spawned, and not
/// allowed to abort the rest of the batch - while a later, valid check in the
/// same run still runs. The first two can only reach the catalogue through a
/// direct write to state.db, which is exactly why `verify` checks again.
#[test]
fn verify_rejects_an_unsafe_or_missing_cwd_without_aborting_the_batch() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    // Each refused one carries a distinctive exit code (7/8/9) that would show
    // up in `exit_code` if the safety check had a hole and it actually ran.
    add_check(&f, "a-abs", &script("ex", "7"));
    add_check(&f, "b-parent", &script("ex", "8"));
    add_check_as(&f, "c-missing", &script("ex", "9"), SHELL, &["--cwd", "does-not-exist-xyz"]);
    add_check(&f, "d-ok", &script("ex", "0"));
    set_check_cwd(&f, "a-abs", OUTSIDE_ABS);
    set_check_cwd(&f, "b-parent", "..");
    approve(&f, None);

    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["failures"], 3);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "the batch must not abort after the first refusal");
    for (r, expect_substr) in results[..3].iter().zip([OUTSIDE_ABS, "..", "does-not-exist-xyz"]) {
        assert!(r["exit_code"].is_null(), "got {r:#}");
        let err = r["error"].as_str().expect("a refused row must carry an error");
        assert!(err.contains(expect_substr), "got {err}");
    }
    // the fourth, valid check still ran
    assert_eq!(results[3]["exit_code"], 0);

    // the rows themselves agree: exit_code null, error populated
    let rows = verify_rows(&f, &task_id, "verify: ");
    assert_eq!(rows.len(), 4);
    for r in &rows[..3] {
        assert!(r["detail"]["exit_code"].is_null(), "got {r:#}");
        assert!(!r["detail"]["error"].is_null(), "got {r:#}");
        assert_eq!(r["collected_by"], "control_plane");
    }
    assert_eq!(rows[3]["detail"]["exit_code"], 0);

    // refresh_index and verify_ran still ran despite the refusals
    let detail = verify_ran_details(&f).pop().unwrap();
    assert_eq!(detail["count"], 4);
    assert_eq!(detail["failures"], 3);

    // the human exit column says REFUSED for them
    let human = verify_human(&f, &run_id, &[]);
    assert_eq!(human.matches("REFUSED").count(), 3, "{human}");
}

/// Fix round 1 / I2: a timed-out check must become a failed, `exit_code: null`
/// row - not an error that aborts the batch - and the next check must still
/// run.
#[test]
fn verify_records_a_timed_out_check_as_a_failure_and_still_runs_the_next_one() {
    let f = common::fixture();
    let (_task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    // runs for far longer than the 1s --timeout below
    add_check(&f, "a-long", &script("long", ""));
    add_check(&f, "b-ok", &script("ex", "0"));
    approve(&f, None);

    let v = verify(&f, &run_id, &["--timeout", "1s"], false);
    assert_eq!(v["failures"], 1);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "the timeout must not abort the batch");
    assert!(results[0]["exit_code"].is_null(), "got {:#}", results[0]);
    assert!(
        results[0]["error"].as_str().unwrap().contains("timed out"),
        "got {:#}",
        results[0]
    );
    assert_eq!(results[1]["exit_code"], 0);

    // Fix round 2 / R2: the human line for a timed-out check says TIMEOUT,
    // not REFUSED; and the human form exits non-zero just like --json.
    let a = f.cmd().args(["verify", &run_id, "--timeout", "1s"]).assert();
    let code = a.get_output().status.code();
    assert!(code.is_some() && code != Some(0), "a timed-out check must fail the command, got {code:?}");
    let human = String::from_utf8(a.get_output().stdout.clone()).unwrap();
    assert!(human.contains("TIMEOUT"), "got {human}");
    assert!(!human.contains("REFUSED"), "got {human}");
}

/// Fix round 1 / R3: a queued or running run's worktree may still be edited
/// by the worker itself. `verify` must refuse rather than execute anything.
#[test]
fn verify_refuses_a_run_that_is_still_queued_or_running() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    add_check(&f, "ok", &script("ex", "0"));
    approve(&f, None);
    let before = f.evidence_rows(&task_id).len();

    // Force the already-settled run back to "running" to exercise the guard
    // without racing a real in-flight worker.
    f.state_conn()
        .execute(
            "UPDATE runs SET status = 'running' WHERE run_id = ?1",
            [run_id.as_str()],
        )
        .unwrap();

    let assertion = f.cmd().args(["verify", &run_id]).assert().failure();
    let stderr = String::from_utf8(assertion.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("running"), "got {stderr}");
    assert_eq!(f.evidence_rows(&task_id).len(), before, "nothing must be recorded");
}

/// Fix round 1 / R2, P7-1b / AC1: with nothing approved to run, `verify`
/// exits 1 with the same key set as the normal path plus `reason`.
#[test]
fn verify_reports_the_same_json_shape_when_there_are_no_approved_checks() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let before = f.evidence_rows(&task_id).len();

    let v = verify(&f, &run_id, &[], false);
    let keys = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let mut expected = vec![
        "dry_run", "failures", "ok", "reason", "results", "run_id", "source", "task_id",
        "warnings", "worker_declared", "approved_checks", "previous_verify",
        "missing_since_last_verify",
        // RFIX-1: where the checks ran, null when nothing ran
        "head_commit", "head_commit_after", "worktree_clean",
    ];
    expected.sort();
    assert_eq!(keys(&v), expected);
    assert_eq!(v["head_commit"], serde_json::Value::Null);
    assert_eq!(v["ok"], false);
    assert_eq!(v["failures"], 0);
    assert_eq!(v["reason"], "no_approved_checks");
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
    assert_eq!(v["worker_declared"].as_array().unwrap().len(), 2);
    assert_eq!(f.evidence_rows(&task_id).len(), before, "nothing ran or was recorded");
    assert!(verify_ran_details(&f).is_empty());

    let a = f.cmd().args(["verify", &run_id]).assert().code(1);
    let human = String::from_utf8(a.get_output().stdout.clone()).unwrap();
    assert!(
        human.contains(
            "no approved checks for this repository; add them with thoth checks add ... then \
             approve them with: thoth checks approve --reason \"WHY\""
        ),
        "got {human}"
    );

    // the normal path's key set is the same, without `reason`
    add_check(&f, "ok", &script("ex", "0"));
    approve(&f, None);
    let v = verify(&f, &run_id, &[], true);
    expected.retain(|k| *k != "reason");
    assert_eq!(keys(&v), expected);
}

/// P7-1b / AC1: a spec that lists only ids the catalogue does not hold has
/// nothing to run either.
#[test]
fn verify_has_nothing_to_run_when_no_spec_check_is_in_the_catalogue() {
    let f = common::fixture();
    let spec = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"risk":"low","checks":["ghost"]}"#;
    let (_task_id, run_id, _, _) = settled_run(&f, spec);
    add_check(&f, "other", "exit 0");
    approve(&f, None);
    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["reason"], "no_approved_checks");
    assert_eq!(v["source"], "approved checks (spec)");
    assert_eq!(v["results"].as_array().unwrap().len(), 0);
}

/// P7-1b / AC1: the spec's ids run in spec order; one the catalogue does not
/// hold is a refused row; a catalogue check the spec does not list is not run.
#[test]
fn verify_runs_the_spec_checks_in_spec_order_and_refuses_an_unknown_id() {
    let f = common::fixture();
    let spec = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"risk":"low","checks":["zz-real","ghost"]}"#;
    let (task_id, run_id, _, worktree) = settled_run(&f, spec);
    add_check(&f, "zz-real", &script("ex", "0"));
    add_check(&f, "aa-other", &script("mark", ""));
    approve(&f, None);

    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["source"], "approved checks (spec)");
    assert_eq!(v["failures"], 1);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "aa-other is not in the spec: {v:#}");
    assert_eq!(results[0]["check_id"], "zz-real");
    assert_eq!(results[0]["exit_code"], 0);
    assert_eq!(results[1]["check_id"], "ghost");
    assert!(results[1]["exit_code"].is_null());
    assert_eq!(
        results[1]["error"],
        "check ghost is not in this repository's check catalogue"
    );
    assert!(!worktree.join("marker.txt").exists());
    let rows = verify_rows(&f, &task_id, "verify: ");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["detail"]["check_id"], "ghost");
    assert!(rows[1]["detail"]["exit_code"].is_null());
    let human = verify_human(&f, &run_id, &[]);
    assert!(human.contains("REFUSED"), "{human}");
}

/// P7-1b / AC6 (a): a check nobody approved is refused and never runs.
#[test]
fn verify_refuses_an_unapproved_check_without_running_it() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check(&f, "writes", &script("mark", ""));

    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["failures"], 1);
    let r = &v["results"][0];
    assert!(r["exit_code"].is_null(), "got {r:#}");
    assert_eq!(
        r["error"],
        "check writes is not approved; approve it with: thoth checks approve --reason \"WHY\""
    );
    assert!(!worktree.join("marker.txt").exists(), "an unapproved check must not run");
    let rows = verify_rows(&f, &task_id, "verify: ");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["collected_by"], "control_plane");
    assert!(rows[0]["detail"]["approved_at"].is_null());
    assert!(verify_human(&f, &run_id, &[]).contains("REFUSED"));
}

/// P7-1b / AC6 (b) and AC3: one changed byte in an input refuses the check
/// with `input=`, naming this run in the approve hint; approving from the
/// run's worktree lets it run. `--at-base` does not compare inputs.
#[test]
fn verify_refuses_a_check_whose_input_changed_until_it_is_approved_from_the_run() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    // The fixture wrote a.txt raw, while git may have converted line endings
    // when it checked the worktree out; start both trees from the same bytes.
    std::fs::copy(worktree.join("a.txt"), f.repo_path().join("a.txt")).unwrap();
    add_check_as(&f, "reads-a", &script("ex", "0"), SHELL, &["--input", "a.txt"]);
    approve(&f, None);
    // untouched: it runs
    verify(&f, &run_id, &[], true);

    let a = worktree.join("a.txt");
    let mut bytes = std::fs::read(&a).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&a, &bytes).unwrap();

    let v = verify(&f, &run_id, &[], false);
    let err = v["results"][0]["error"].as_str().unwrap();
    assert!(err.starts_with("fingerprint mismatch for check reads-a: input=a.txt"), "got {err}");
    assert!(
        err.ends_with(&format!(
            "if this change is intended, approve it with: thoth checks approve --run {run_id} --reason \"WHY\""
        )),
        "got {err}"
    );
    assert!(v["results"][0]["exit_code"].is_null());
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], false);
    assert_eq!(row["detail"]["inputs_checked"], true);
    let dry = verify(&f, &run_id, &["--dry-run"], true);
    assert_eq!(dry["results"][0]["fingerprint_ok"], false);
    assert_eq!(dry["results"][0]["would_refuse"], true);

    approve(&f, Some(&run_id));
    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0);

    // the approval now pins the edited a.txt, which the base checkout does not
    // have; --at-base compares only shell/cmd/cwd/exe, so it still runs
    let v = verify(&f, &run_id, &["--at-base"], true);
    assert_eq!(v["results"][0]["exit_code"], 0);
    let row = verify_rows(&f, &task_id, "verify@base: ").pop().unwrap();
    assert_eq!(row["detail"]["inputs_checked"], false);
    assert_eq!(row["detail"]["fingerprint_ok"], true);
}

/// P7-1b / AC2: a check in a shell this platform lacks is refused.
#[test]
fn verify_refuses_a_shell_this_platform_does_not_have() {
    let f = common::fixture();
    let (_task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let (shell, os) = if cfg!(windows) { ("bash", "windows") } else { ("powershell", "linux") };
    add_check_as(&f, "foreign", "exit 0", shell, &[]);
    approve(&f, None);
    let v = verify(&f, &run_id, &[], false);
    assert_eq!(
        v["results"][0]["error"],
        format!("check foreign uses shell {shell}, which is not available on {os}")
    );
    assert!(v["results"][0]["exit_code"].is_null());
}

/// P7-1b / AC6 (c): a powershell check's exit code is `$LASTEXITCODE`/`exit`,
/// sent as `-EncodedCommand`, with the script on the record. AC14 (g)(10): the
/// PowerShell test (`if (Test-Path Cargo.toml) { exit 0 } else { exit 7 }`)
/// lives in the committed `pscargo.ps1`, and the script calls it by path.
#[cfg(windows)]
#[test]
fn verify_runs_a_powershell_check_with_its_exit_code() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check_as(&f, "ps", r".\pscargo.ps1", "powershell", &[]);
    approve(&f, None);

    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["results"][0]["exit_code"], 7, "{v:#}");
    assert_eq!(v["results"][0]["shell"], "powershell");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(
        row["detail"]["script"],
        "$ErrorActionPreference='Stop'; & $env:THOTH_CHECK_PROGRAM; \
         if ($null -ne $LASTEXITCODE) { exit $LASTEXITCODE }"
    );
    assert_program_is(&row, &worktree.join("pscargo.ps1"));
    let argv = row["detail"]["argv"].as_array().unwrap();
    assert_eq!(argv[0], "powershell.exe");
    assert_eq!(argv[5], "-EncodedCommand");
    assert_eq!(argv.len(), 7);

    std::fs::write(worktree.join("Cargo.toml"), "x").unwrap();
    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0);
}

/// P7-1b / AC6 (c), as AC14 (g)(10) rewrote it: a `cmd` check's two
/// arguments reach its script as written (raw_arg, no re-quoting).
#[cfg(windows)]
#[test]
fn verify_passes_a_cmd_checks_arguments_through_unchanged() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let cmd = script("args", "alpha omega");
    add_check_as(&f, "quoted", &cmd, "cmd", &[]);
    approve(&f, None);
    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["argv"][5], "\"\"%THOTH_CHECK_PROGRAM%\" alpha omega\"");
    assert_program_is(&row, &worktree.join("args.cmd"));
    let out = std::fs::read_to_string(row["blob_path"].as_str().unwrap()).unwrap();
    assert!(out.contains("[alpha] [omega]"), "got {out:?}");
}

/// P7-1b / AC6 (d): a bash check (bash-only syntax, `[[ -f Cargo.toml ]]`,
/// in the committed `bashy.sh` since AC14 (g)(10)) on Linux.
#[cfg(target_os = "linux")]
#[test]
fn verify_runs_a_bash_check() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check_as(&f, "bashy", "./bashy.sh", "bash", &[]);
    approve(&f, None);
    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["results"][0]["exit_code"], 1, "{v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    let program = row["detail"]["program"].as_str().unwrap().to_string();
    assert_eq!(row["detail"]["argv"], serde_json::json!(["bash", "-c", "\"$0\" \"$@\"", program]));
    assert_program_is(&row, &worktree.join("bashy.sh"));
    std::fs::write(worktree.join("Cargo.toml"), "x").unwrap();
    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0);
}

/// P7-1b / AC6 (e): an argv check runs with no shell on either platform.
#[test]
fn verify_runs_an_argv_check_without_a_shell() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    add_check_as(&f, "git-version", "git --version", "argv", &[]);
    approve(&f, None);
    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    assert_eq!(v["results"][0]["shell"], "argv");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    // AC10 (2): the program is sent as the absolute path the fingerprint
    // resolved on PATH
    let argv = row["detail"]["argv"].as_array().unwrap();
    assert_eq!(argv.len(), 2);
    let program = std::path::Path::new(argv[0].as_str().unwrap());
    assert!(program.is_absolute(), "got {program:?}");
    let stem = program.file_stem().unwrap().to_string_lossy().to_lowercase();
    assert_eq!(stem, "git", "got {program:?}");
    assert_eq!(argv[1], "--version");
    // AC14 (b): detail.program is that same absolute path
    let detail_program = row["detail"]["program"].as_str().unwrap_or_else(|| panic!("{row:#}"));
    assert!(std::path::Path::new(detail_program).is_absolute(), "got {detail_program:?}");
    assert_eq!(detail_program, argv[0].as_str().unwrap());
    let out = std::fs::read_to_string(row["blob_path"].as_str().unwrap()).unwrap();
    assert!(out.contains("git version"), "got {out:?}");
}

/// P7-1b / AC4, AC6 (f): what the worker declared is listed and never run.
#[test]
fn verify_lists_the_worker_declared_commands_without_running_them() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    resubmit_requests(
        &f,
        &task_id,
        &run_id,
        serde_json::json!([{"cmd": WRITES_MARKER, "cwd": "."}]),
    );
    let ok = script("ex", "0");
    add_check(&f, "ok", &ok);
    approve(&f, None);

    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["worker_declared"], serde_json::json!([{"cmd": WRITES_MARKER, "cwd": "."}]));
    assert_eq!(v["results"].as_array().unwrap().len(), 1);
    assert!(!worktree.join("marker.txt").exists(), "a declared command must not run");
    let labels: Vec<String> = verify_rows(&f, &task_id, "verify: ")
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(labels, [format!("verify: {ok}")], "no evidence for the declared command");
    let detail = verify_ran_details(&f).pop().unwrap();
    assert_eq!(detail["worker_declared"], v["worker_declared"]);
    assert_eq!(detail["checks"], serde_json::json!(["ok"]));

    let dry = verify(&f, &run_id, &["--dry-run"], true);
    assert_eq!(dry["worker_declared"], v["worker_declared"]);
    let human = verify_human(&f, &run_id, &["--dry-run"]);
    let at = human.find("worker declared (reference only):").expect(&human);
    assert!(human[at..].contains(WRITES_MARKER), "{human}");
    assert!(!worktree.join("marker.txt").exists());
}

/// Fix round 2 / N1.6b: a check whose shell exits but leaves a detached
/// descendant holding its output pipe must be recorded as a failed row with
/// `output_incomplete`'s error text, and the next check must still run.
#[test]
fn verify_records_an_incomplete_output_check_as_a_failure_and_still_runs_the_next_one() {
    let f = common::fixture();
    let (_task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    // the shell returns almost instantly; the detached child (short enough to
    // self-heal, per fix round 2 / R1) keeps the pipe open past the grace.
    add_check(&f, "a-detached", &script("detached", ""));
    add_check(&f, "b-ok", &script("ex", "0"));
    approve(&f, None);

    let v = verify(&f, &run_id, &[], false);
    assert_eq!(v["failures"], 1);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "the incomplete output must not abort the batch");
    assert!(
        results[0]["error"]
            .as_str()
            .unwrap()
            .contains("output may be incomplete"),
        "got {:#}",
        results[0]
    );
    assert_eq!(results[1]["exit_code"], 0);
}

/// Fix round 2 / R5: `--dry-run` marks a check it would refuse, leaves a
/// normal one unmarked, and executes or records nothing either way.
#[test]
fn verify_dry_run_marks_would_refuse_without_executing_or_recording_anything() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check(&f, "a-parent", &script("ex", "7"));
    add_check(&f, "b-writes", &script("mark", ""));
    set_check_cwd(&f, "a-parent", "..");
    approve(&f, None);
    let before = f.evidence_rows(&task_id).len();

    let v = verify(&f, &run_id, &["--dry-run"], true);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["would_refuse"], true);
    assert!(results[0]["reason"].as_str().unwrap().contains(".."), "got {:#}", results[0]);
    assert_eq!(results[1]["would_refuse"], false);
    assert_eq!(results[1]["fingerprint_ok"], true);
    assert!(results[1].get("reason").is_none(), "got {:#}", results[1]);

    assert_eq!(
        f.evidence_rows(&task_id).len(),
        before,
        "a dry run must not execute or record anything"
    );
    assert!(!worktree.join("marker.txt").exists(), "a dry run must not execute anything");
    assert!(verify_ran_details(&f).is_empty());
}

/// `verify --at-base` runs the approved checks in a temporary detached
/// worktree at the task's base_commit and records them as `verify@base: `
/// evidence; plain `verify` is unaffected.
#[test]
fn verify_at_base_reruns_in_a_temporary_worktree_at_the_base_commit() {
    let f = common::fixture();
    let (task_id, run_id, base, worktree) = settled_run(&f, common::Fixture::SPEC);

    // A file only the candidate has, and an approved check that needs it.
    std::fs::write(worktree.join("cand.txt"), "x").unwrap();
    let needs_cand = script("needscand", "");
    add_check(&f, "needs-cand", &needs_cand);
    approve(&f, None);
    let before = f.evidence_rows(&task_id).len();
    let wt_list_before = common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]);

    // plain verify: passes at the candidate
    let plain = verify(&f, &run_id, &[], true);
    assert!(plain.get("at_commit").is_none());
    assert_eq!(plain["failures"], 0);
    assert_eq!(f.evidence_rows(&task_id).len(), before + 1);

    // --at-base --dry-run: lists, runs and records nothing
    let dry = verify(&f, &run_id, &["--at-base", "--dry-run"], true);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["at_commit"], base.as_str());
    assert_eq!(dry["results"][0]["cmd"], needs_cand.as_str());
    let where_ = dry["results"][0]["cwd"].as_str().unwrap();
    assert!(
        where_.starts_with(f.home_path().to_str().unwrap()),
        "must be under THOTH_HOME, got {where_}"
    );
    assert!(!where_.starts_with(worktree.to_str().unwrap()));
    assert_eq!(f.evidence_rows(&task_id).len(), before + 1);
    assert_eq!(
        common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]),
        wt_list_before
    );

    // the real baseline run: the same check now fails, and is recorded
    let v = verify(&f, &run_id, &["--at-base"], false);
    assert_eq!(v["ok"], false);
    assert_eq!(v["failures"], 1);
    assert_eq!(v["at_commit"], base.as_str());
    for key in ["run_id", "task_id", "source", "dry_run", "results", "warnings", "worker_declared"] {
        assert!(v.get(key).is_some(), "missing {key}");
    }
    assert_ne!(v["results"][0]["exit_code"], 0);
    let rows = f.evidence_rows(&task_id);
    assert_eq!(rows.len(), before + 2);
    let row = rows.last().unwrap();
    assert!(row["label"].as_str().unwrap().starts_with("verify@base: "));
    assert_eq!(row["detail"]["at_commit"], base.as_str());
    assert_ne!(row["detail"]["exit_code"], 0);
    assert_eq!(row["detail"]["inputs_checked"], false);

    // the temporary worktree is gone again
    assert_eq!(
        common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]),
        wt_list_before
    );
    // and the plain rows carry no at_commit
    assert!(rows[rows.len() - 2]["detail"].get("at_commit").is_none());
}

#[test]
fn verify_at_base_is_refused_on_a_run_that_is_not_terminal() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    add_check(&f, "ok", &script("ex", "0"));
    approve(&f, None);
    let d = f.dispatch(&task_id, "claude-b", "slow", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let assertion = f.cmd().args(["verify", &run_id, "--at-base"]).assert().failure();
    let err = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(err.contains("still"), "got {err}");
    f.cmd().args(["cancel", &run_id]).assert().success();
}

/// A failing (here: timing-out) check or a refused cwd must not leave the
/// baseline checkout or its registration behind.
#[test]
fn verify_at_base_cleans_up_after_a_timeout_and_after_a_refused_cwd() {
    let f = common::fixture();
    let (_task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let wt_list_before = common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]);
    let baseline_dir = |f: &common::Fixture| -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![f.home_path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "baseline-worktrees") {
                        found.extend(std::fs::read_dir(&p).unwrap().flatten().map(|e| e.path()));
                    } else if !p.file_name().is_some_and(|n| n == "worktrees" || n == ".git") {
                        stack.push(p);
                    }
                }
            }
        }
        found
    };

    add_check(&f, "only", &script("long", ""));
    approve(&f, None);
    verify(&f, &run_id, &["--at-base", "--timeout", "1s"], false);
    assert_eq!(common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]), wt_list_before);
    assert!(baseline_dir(&f).is_empty(), "left behind: {:?}", baseline_dir(&f));

    // replacing the check clears its approval; point it outside, approve again
    add_check(&f, "only", "exit 0");
    set_check_cwd(&f, "only", "../escape");
    approve(&f, None);
    let v = verify(&f, &run_id, &["--at-base"], false);
    assert!(v["results"][0]["error"].as_str().unwrap().contains("../escape"), "{v:#}");
    assert_eq!(common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]), wt_list_before);
    assert!(baseline_dir(&f).is_empty(), "left behind: {:?}", baseline_dir(&f));
}

/// A checkout an interrupted `--at-base` left registered (its directory gone)
/// is pruned before the next baseline is made.
#[test]
fn verify_at_base_prunes_a_stale_baseline_registration() {
    let f = common::fixture();
    let (_task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    add_check(&f, "ok", &script("ex", "0"));
    approve(&f, None);

    let wt_list_before = common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]);
    let dry = verify(&f, &run_id, &["--at-base", "--dry-run"], true);
    let baseline_root = std::path::Path::new(dry["results"][0]["cwd"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let stale = baseline_root.join("stale-checkout");
    std::fs::create_dir_all(&baseline_root).unwrap();
    common::git(
        &f.repo_path(),
        &["worktree", "add", "--detach", stale.to_str().unwrap(), "HEAD"],
    );
    std::fs::remove_dir_all(&stale).unwrap();
    assert_ne!(common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]), wt_list_before);

    verify(&f, &run_id, &["--at-base"], true);
    assert_eq!(common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]), wt_list_before);
}

/// AC10 (1): every refused row - including the ones refused before any
/// fingerprint was compared - carries `fingerprint_ok` and `inputs_checked` as
/// booleans (false when nothing was compared), never null. Dry runs likewise.
#[test]
fn verify_writes_boolean_fingerprint_fields_on_every_kind_of_refused_row() {
    let f = common::fixture();
    let spec = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"risk":"low","checks":["ghost","unapproved","unsafe","foreign","ok"]}"#;
    let (task_id, run_id, _, _) = settled_run(&f, spec);
    let foreign = if cfg!(windows) { "bash" } else { "powershell" };
    add_check(&f, "unsafe", "exit 7");
    add_check_as(&f, "foreign", "exit 0", foreign, &[]);
    add_check(&f, "ok", &script("ex", "0"));
    set_check_cwd(&f, "unsafe", "..");
    approve(&f, None);
    add_check(&f, "unapproved", "exit 0");

    let expect_refused = ["ghost", "unapproved", "unsafe", "foreign"];
    for (args, inputs_checked_when_compared) in [(&[][..], true), (&["--at-base"][..], false)] {
        let v = verify(&f, &run_id, args, false);
        assert_eq!(v["failures"], 4, "{v:#}");
        let prefix = if args.is_empty() { "verify: " } else { "verify@base: " };
        let rows = verify_rows(&f, &task_id, prefix);
        let rows = &rows[rows.len() - 5..];
        for (row, id) in rows.iter().zip(["ghost", "unapproved", "unsafe", "foreign", "ok"]) {
            let d = &row["detail"];
            assert_eq!(d["check_id"], id, "{row:#}");
            assert!(d["fingerprint_ok"].is_boolean(), "fingerprint_ok for {id}: {d:#}");
            assert!(d["inputs_checked"].is_boolean(), "inputs_checked for {id}: {d:#}");
            if expect_refused.contains(&id) {
                assert!(d["exit_code"].is_null(), "{id} must be refused: {d:#}");
                assert_eq!(d["fingerprint_ok"], false, "{id}: nothing was compared");
                assert_eq!(d["inputs_checked"], false, "{id}: nothing was compared");
            } else {
                assert_eq!(d["exit_code"], 0, "{d:#}");
                assert_eq!(d["fingerprint_ok"], true);
                assert_eq!(d["inputs_checked"], inputs_checked_when_compared);
            }
        }
    }

    // the dry runs, including a baseline one whose checkout does not exist yet
    for args in [&["--dry-run"][..], &["--dry-run", "--at-base"][..]] {
        // a dry run executes nothing, so it counts no failures
        let v = verify(&f, &run_id, args, true);
        for r in v["results"].as_array().unwrap() {
            assert!(r["fingerprint_ok"].is_boolean(), "{args:?}: {r:#}");
        }
    }
}

/// AC10 (2): an argv check whose program is a relative path inside the tree
/// runs that file - the one the fingerprint pinned - even though thoth itself
/// was started in another directory (the main repo) holding a different file
/// under the same relative path.
#[test]
fn verify_runs_the_in_tree_argv_program_when_thoth_starts_in_another_directory() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let (name, args) = if cfg!(windows) { ("tool.exe", "--help") } else { ("tool.sh", "x") };
    let cmd = format!("./bin/{name} {args}");
    add_check_as(&f, "local-tool", &cmd, "argv", &[]);

    // The run's worktree holds a working program; the directory thoth is
    // started in (the main repo) holds a decoy under the same relative path
    // that fails if it is ever run.
    std::fs::create_dir_all(worktree.join("bin")).unwrap();
    std::fs::create_dir_all(f.repo_path().join("bin")).unwrap();
    let real = worktree.join("bin").join(name);
    let decoy = f.repo_path().join("bin").join(name);
    #[cfg(windows)]
    {
        std::fs::copy(env!("CARGO_BIN_EXE_thoth"), &real).unwrap();
        std::fs::write(&decoy, "not a program").unwrap();
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&real, "#!/bin/sh\necho in-tree-tool\n").unwrap();
        std::fs::write(&decoy, "#!/bin/sh\necho decoy-tool\nexit 5\n").unwrap();
        for p in [&real, &decoy] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    approve(&f, Some(&run_id));

    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], true);
    let argv0 = std::path::PathBuf::from(row["detail"]["argv"][0].as_str().unwrap());
    assert!(argv0.is_absolute(), "the program must be sent as an absolute path: {argv0:?}");
    assert_eq!(
        std::fs::canonicalize(&argv0).unwrap(),
        std::fs::canonicalize(&real).unwrap(),
        "must run the worktree's file, not the one under thoth's own directory"
    );
    #[cfg(not(windows))]
    {
        let out = std::fs::read_to_string(row["blob_path"].as_str().unwrap()).unwrap();
        assert!(out.contains("in-tree-tool"), "got {out:?}");
    }
}

/// AC13 (b): an argv check whose in-tree program path holds a space, written
/// in double quotes, runs that file with its fingerprint matching; one byte
/// changed in the file is then refused as an `exe=` change.
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn verify_runs_a_quoted_argv_program_whose_path_has_a_space() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let (name, args) = if cfg!(windows) { ("my tool.exe", "--help") } else { ("my tool.sh", "x") };
    add_check_as(&f, "spaced", &format!("\"./bin/{name}\" {args}"), "argv", &[]);

    std::fs::create_dir_all(worktree.join("bin")).unwrap();
    let real = worktree.join("bin").join(name);
    #[cfg(windows)]
    std::fs::copy(env!("CARGO_BIN_EXE_thoth"), &real).unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&real, "#!/bin/sh\necho spaced-tool\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    approve(&f, Some(&run_id));

    let v = verify(&f, &run_id, &[], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], true, "{row:#}");
    let argv0 = std::path::PathBuf::from(row["detail"]["argv"][0].as_str().unwrap());
    assert!(argv0.is_absolute(), "the program must be sent as an absolute path: {argv0:?}");
    assert_eq!(
        std::fs::canonicalize(&argv0).unwrap(),
        std::fs::canonicalize(&real).unwrap()
    );

    // one byte changed in the program: refused as an exe= change
    let mut bytes = std::fs::read(&real).unwrap();
    bytes.push(b'\n');
    std::fs::write(&real, bytes).unwrap();
    let v = verify(&f, &run_id, &[], false);
    let err = v["results"][0]["error"].as_str().unwrap_or_default();
    assert!(err.contains("exe="), "{v:#}");
}

/// AC13: an `argv` line whose double quote is never closed is refused by
/// `checks add` (exit 1, nothing written) and, for a row that got into
/// state.db some other way, by `verify`.
#[test]
fn an_argv_check_with_an_unclosed_quote_is_refused_by_add_and_verify() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let cmd = r#""./my tool.exe --version"#;
    let a = f
        .cmd()
        .args(["checks", "add", "quoted", "--cmd", cmd, "--shell", "argv", "--json"])
        .assert()
        .code(1);
    let out = a.get_output();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("not closed"), "{text}");
    let n: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM checks WHERE check_id = 'quoted'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "checks add wrote a row it refused");

    approve_quoted_row(&f, cmd, "argv", &[]);
    let v = verify(&f, &run_id, &[], false);
    let r = &v["results"][0];
    assert!(r["exit_code"].is_null(), "{r:#}");
    assert!(r["error"].as_str().unwrap().contains("not closed"), "{r:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], false);
    assert_eq!(row["detail"]["inputs_checked"], false);
}

/// AC10 (3): a `cmd` check naming a program on PATH must never run a
/// same-named batch file a worker left in the check's cwd - cmd.exe would
/// otherwise look there first while the fingerprint pinned the PATH program.
/// Control: without the decoy the check runs normally.
#[cfg(windows)]
#[test]
fn verify_never_runs_a_same_named_batch_file_from_a_cmd_checks_cwd() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check_as(&f, "host", "hostname", "cmd", &[]);
    approve(&f, None);
    // Some hosts (Claude Code among them) already export the variable that
    // stops cmd.exe searching its cwd; thoth must not rely on that.
    let verify_clean_env = || {
        let a = f
            .cmd()
            .env_remove("NoDefaultCurrentDirectoryInExePath")
            .args(["verify", &run_id, "--json"])
            .assert()
            .success();
        json_of(a.get_output().stdout.clone())
    };

    // control: no decoy, the real hostname.exe runs
    let v = verify_clean_env();
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");

    // the decoy: an untracked hostname.bat in the check's cwd
    std::fs::write(worktree.join("hostname.bat"), "@echo x> marker.txt\r\n@exit /b 0\r\n").unwrap();
    let v = verify_clean_env();
    assert!(
        !worktree.join("marker.txt").exists(),
        "cmd.exe ran the worktree's hostname.bat instead of the approved program: {v:#}"
    );
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], true);
}

/// AC10 (3), the other shells: none of them searches the check's cwd by
/// itself, but a relative PATH entry (`.`) would make every one of them do so.
/// With `.` first on thoth's PATH and a same-named decoy in the cwd, each
/// shell this platform has still runs the approved PATH program.
#[test]
fn verify_ignores_a_relative_path_entry_for_every_shell() {
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    #[cfg(windows)]
    let (program, shells) = ("hostname", ["cmd", "powershell", "argv"]);
    #[cfg(not(windows))]
    let (program, shells) = ("uname", ["sh", "bash", "argv"]);
    for s in shells {
        add_check_as(&f, &format!("via-{s}"), program, s, &[]);
    }
    approve(&f, None);

    #[cfg(windows)]
    std::fs::write(worktree.join("hostname.bat"), "@echo x> marker.txt\r\n@exit /b 0\r\n").unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let decoy = worktree.join("uname");
        std::fs::write(&decoy, "#!/bin/sh\necho x > marker.txt\n").unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::var_os("PATH").unwrap();
    let mut entries = vec![std::path::PathBuf::from(".")];
    entries.extend(std::env::split_paths(&path));
    let dotted = std::env::join_paths(entries).unwrap();

    let a = f
        .cmd()
        .env("PATH", &dotted)
        .env_remove("NoDefaultCurrentDirectoryInExePath")
        .args(["verify", &run_id, "--json"])
        .assert()
        .success();
    let v = json_of(a.get_output().stdout.clone());
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{v:#}");
    for r in results {
        assert_eq!(r["exit_code"], 0, "{r:#}");
    }
    assert!(!worktree.join("marker.txt").exists(), "a decoy in the cwd ran: {v:#}");
}

/// AC10 (4): `--at-base` compares against the approval's own hex (current
/// shell/cmd/cwd + current exe + the approved input lines), so an approval
/// whose stored fingerprint no longer matches is refused there too.
#[test]
fn verify_at_base_refuses_when_the_approved_fingerprint_does_not_match() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    add_check(&f, "ok", &script("ex", "0"));
    approve(&f, None);
    // untampered: the baseline run goes ahead
    verify(&f, &run_id, &["--at-base"], true);

    let hex: String = f
        .state_conn()
        .query_row("SELECT approved_fingerprint FROM checks WHERE check_id = 'ok'", [], |r| r.get(0))
        .unwrap();
    let flipped = if hex.starts_with('0') { "1" } else { "0" };
    let tampered = format!("{flipped}{}", &hex[1..]);
    f.state_conn()
        .execute(
            "UPDATE checks SET approved_fingerprint = ?1 WHERE check_id = 'ok'",
            [tampered.as_str()],
        )
        .unwrap();
    f.state_conn()
        .execute(
            "UPDATE check_approvals SET fingerprint = ?1 WHERE check_id = 'ok'",
            [tampered.as_str()],
        )
        .unwrap();

    let v = verify(&f, &run_id, &["--at-base"], false);
    assert!(v["results"][0]["exit_code"].is_null(), "{v:#}");
    let err = v["results"][0]["error"].as_str().unwrap();
    assert!(err.starts_with("fingerprint mismatch for check ok: "), "got {err}");
    assert!(
        err.ends_with(&format!("approve it with: thoth checks approve --run {run_id} --reason \"WHY\"")),
        "got {err}"
    );
    let row = verify_rows(&f, &task_id, "verify@base: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], false);
    assert_eq!(row["detail"]["inputs_checked"], false);
}

// ---------------------------------------------------------------------------
// AC11: what runs is the file that was fingerprinted. Every test here plants a
// file that writes a marker if it ever runs, next to (or ahead of) the file the
// approval pinned, and asserts after `verify` that the marker does not exist.

/// `verify RUN --json` with extra environment, never inheriting the variable
/// that stops cmd.exe searching its cwd (thoth must not rely on the host
/// having set it).
fn verify_env(
    f: &common::Fixture,
    run_id: &str,
    envs: &[(&str, &std::ffi::OsStr)],
    ok: bool,
) -> serde_json::Value {
    let mut c = f.cmd();
    c.env_remove("NoDefaultCurrentDirectoryInExePath");
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(["verify", run_id, "--json"]);
    let a = c.assert();
    let a = if ok { a.success() } else { a.code(1) };
    json_of(a.get_output().stdout.clone())
}

/// `checks approve --json [--run RUN]` with extra environment.
fn approve_env(f: &common::Fixture, run: Option<&str>, envs: &[(&str, &std::ffi::OsStr)]) {
    let mut c = f.cmd();
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(["checks", "approve", "--reason", "test approval", "--json"]);
    if let Some(r) = run {
        c.args(["--run", r]);
    }
    c.assert().success();
}

/// The `exe.path` a check's approval pinned.
fn approved_exe_path(f: &common::Fixture, id: &str) -> String {
    let detail: String = f
        .state_conn()
        .query_row("SELECT approved_detail FROM checks WHERE check_id = ?1", [id], |r| r.get(0))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&detail).unwrap();
    v["exe"]["path"].as_str().unwrap().to_string()
}

/// A batch file that writes `marker` (absolute) when it runs.
#[cfg(windows)]
fn marker_batch(marker: &std::path::Path) -> String {
    format!("@echo x> \"{}\"\r\n@exit /b 0\r\n", marker.display())
}

/// A PowerShell script that writes `marker` (absolute) when it runs.
#[cfg(windows)]
fn marker_ps1(marker: &std::path::Path) -> String {
    format!("Set-Content -LiteralPath '{}' -Value x\r\nexit 0\r\n", marker.display())
}

/// A real PE (`fake-codex`) that appends to `$FAKE_CODEX_ARGS_LOG` as soon as
/// it runs, whatever its arguments; the marker is that log file.
#[cfg(windows)]
fn place_marker_pe(dest: &std::path::Path) {
    std::fs::copy(env!("CARGO_BIN_EXE_fake-codex"), dest).unwrap();
}

/// AC11 (a): Rust's `Command` on Windows tries `<path>.exe` first for a path
/// that does not end in `.exe`, so an argv program that resolves to anything
/// but an `.exe` is refused - otherwise a `tool.exe` (or `tool.cmd.exe`) the
/// worker adds next to it would run while `fingerprint_ok` stays true.
#[cfg(windows)]
#[test]
fn verify_exe_argv_refuses_a_program_windows_would_swap_for_a_dot_exe() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let bin = worktree.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // pinned: an extensionless program and a batch file named in full
    std::fs::copy(env!("CARGO_BIN_EXE_thoth"), bin.join("tool")).unwrap();
    std::fs::write(bin.join("tool2.cmd"), "@exit /b 0\r\n").unwrap();
    add_check_as(&f, "bare", "./bin/tool --help", "argv", &[]);
    add_check_as(&f, "named-cmd", "./bin/tool2.cmd", "argv", &[]);
    approve(&f, Some(&run_id));

    // the worker's additions, each of which Windows would run instead
    place_marker_pe(&bin.join("tool.exe"));
    place_marker_pe(&bin.join("tool2.cmd.exe"));
    let marker = f.home_path().join("argv-hijack.log");
    let v = verify_env(&f, &run_id, &[("FAKE_CODEX_ARGS_LOG", marker.as_os_str())], false);

    assert!(!marker.exists(), "a .exe planted next to the pinned program ran: {v:#}");
    for r in v["results"].as_array().unwrap() {
        assert!(r["exit_code"].is_null(), "{r:#}");
        let err = r["error"].as_str().unwrap();
        assert!(err.contains("use shell cmd for this check"), "{err}");
        assert!(err.contains(".exe"), "{err}");
    }
    assert_eq!(verify_rows(&f, &task_id, "verify: ").len(), 2);
}

/// AC11 (b)(i): a `cmd` check naming an in-tree program must write its
/// extension. cmd.exe never runs an extensionless file and tries PATHEXT in
/// order (`.COM`, `.EXE`, `.BAT`, `.CMD`, ...), so `.\tool` approved while only
/// `tool.cmd` existed would run a `tool.bat` added later.
#[cfg(windows)]
#[test]
fn verify_exe_cmd_refuses_an_in_tree_program_without_its_extension() {
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    std::fs::write(worktree.join("tool.cmd"), "@exit /b 0\r\n").unwrap();
    add_check_as(&f, "no-ext", r".\tool", "cmd", &[]);
    approve(&f, Some(&run_id));

    let marker = f.home_path().join("cmd-no-ext.marker");
    std::fs::write(worktree.join("tool.bat"), marker_batch(&marker)).unwrap();
    let v = verify_env(&f, &run_id, &[], false);
    assert!(!marker.exists(), "cmd.exe ran the planted tool.bat: {v:#}");
    let err = v["results"][0]["error"].as_str().unwrap();
    assert!(err.contains("extension"), "{err}");
    assert!(v["results"][0]["exit_code"].is_null());
}

/// AC11 (b)(ii): with the extension written, cmd.exe runs exactly that file
/// (probed on Windows 11: `.\tool.cmd` beside `tool.com`, `tool.cmd.bat` and
/// `tool.cmd.com` still runs `tool.cmd`), so none of those planted files runs.
#[cfg(windows)]
#[test]
fn verify_exe_cmd_runs_exactly_the_named_in_tree_file() {
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    std::fs::write(worktree.join("tool.cmd"), "@exit /b 0\r\n").unwrap();
    add_check_as(&f, "named", r".\tool.cmd", "cmd", &[]);
    approve(&f, Some(&run_id));

    let marker = f.home_path().join("cmd-named.marker");
    place_marker_pe(&worktree.join("tool.com"));
    place_marker_pe(&worktree.join("tool.cmd.com"));
    std::fs::write(worktree.join("tool.cmd.bat"), marker_batch(&marker)).unwrap();
    let v = verify_env(&f, &run_id, &[("FAKE_CODEX_ARGS_LOG", marker.as_os_str())], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    assert!(!marker.exists(), "a planted file ran instead of tool.cmd: {v:#}");
}

/// AC11 (b)(iii): for a bare name, the `exe=` line pins what cmd.exe runs -
/// in one PATH directory holding `tool.com` and `tool.exe`, that is the
/// `.com` - and verify runs exactly that file.
#[cfg(windows)]
#[test]
fn verify_exe_cmd_pins_and_runs_the_com_before_the_exe_on_path() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let dir = tempfile::tempdir().unwrap();
    place_marker_pe(&dir.path().join("ac11tool.com"));
    std::fs::copy(env!("CARGO_BIN_EXE_thoth"), dir.path().join("ac11tool.exe")).unwrap();
    let mut entries = vec![dir.path().to_path_buf()];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(entries).unwrap();

    let mut add = f.cmd();
    add.env("PATH", &path)
        .args(["checks", "add", "com-first", "--cmd", "ac11tool --help", "--shell", "cmd", "--json"])
        .assert()
        .success();
    approve_env(&f, None, &[("PATH", path.as_os_str())]);
    let pinned = approved_exe_path(&f, "com-first");
    assert!(pinned.to_lowercase().ends_with("ac11tool.com"), "exe= pinned {pinned}");

    let ran = f.home_path().join("com-ran.log");
    let v = verify_env(
        &f,
        &run_id,
        &[("PATH", path.as_os_str()), ("FAKE_CODEX_ARGS_LOG", ran.as_os_str())],
        false,
    );
    assert_eq!(v["results"][0]["error"], serde_json::Value::Null, "{v:#}");
    assert!(ran.exists(), "verify did not run the pinned ac11tool.com: {v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], true);
}

/// AC11 (c): a powershell check naming a program on PATH never runs a
/// same-named `.bat`, `.cmd`, `.ps1`, `.exe` or `.com` from its cwd
/// (PowerShell does not search the current directory for commands).
#[cfg(windows)]
#[test]
fn verify_exe_powershell_never_runs_a_same_named_file_from_the_cwd() {
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_check_as(&f, "ps-host", "hostname", "powershell", &[]);
    approve(&f, None);

    let marker = f.home_path().join("ps-cwd.marker");
    std::fs::write(worktree.join("hostname.bat"), marker_batch(&marker)).unwrap();
    std::fs::write(worktree.join("hostname.cmd"), marker_batch(&marker)).unwrap();
    std::fs::write(worktree.join("hostname.ps1"), marker_ps1(&marker)).unwrap();
    place_marker_pe(&worktree.join("hostname.exe"));
    place_marker_pe(&worktree.join("hostname.com"));
    let v = verify_env(&f, &run_id, &[("FAKE_CODEX_ARGS_LOG", marker.as_os_str())], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    assert!(!marker.exists(), "powershell ran a file from the check's cwd: {v:#}");
}

/// AC11 (c), as (b)(i): an in-tree powershell program must be named with its
/// extension; PowerShell resolves `.\tool` to `tool.ps1` before `tool.exe`.
#[cfg(windows)]
#[test]
fn verify_exe_powershell_refuses_an_in_tree_program_without_its_extension() {
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    std::fs::copy(env!("CARGO_BIN_EXE_thoth"), worktree.join("tool.exe")).unwrap();
    add_check_as(&f, "ps-no-ext", r".\tool --help", "powershell", &[]);
    approve(&f, Some(&run_id));

    let marker = f.home_path().join("ps-no-ext.marker");
    std::fs::write(worktree.join("tool.ps1"), marker_ps1(&marker)).unwrap();
    let v = verify_env(&f, &run_id, &[], false);
    assert!(!marker.exists(), "powershell ran the planted tool.ps1: {v:#}");
    let err = v["results"][0]["error"].as_str().unwrap();
    assert!(err.contains("extension"), "{err}");
}

/// AC11 (c), as (b)(iii): for a bare name the `exe=` line pins what
/// PowerShell runs - `tool.ps1` ahead of `tool.exe` in the same PATH
/// directory - and verify runs exactly that file.
#[cfg(windows)]
#[test]
fn verify_exe_powershell_pins_and_runs_the_ps1_before_the_exe_on_path() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let dir = tempfile::tempdir().unwrap();
    let ran = f.home_path().join("ps1-ran.marker");
    std::fs::write(dir.path().join("ac11pstool.ps1"), marker_ps1(&ran)).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_thoth"), dir.path().join("ac11pstool.exe")).unwrap();
    let mut entries = vec![dir.path().to_path_buf()];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(entries).unwrap();

    let mut add = f.cmd();
    add.env("PATH", &path)
        .args(["checks", "add", "ps1-first", "--cmd", "ac11pstool", "--shell", "powershell", "--json"])
        .assert()
        .success();
    approve_env(&f, None, &[("PATH", path.as_os_str())]);
    let pinned = approved_exe_path(&f, "ps1-first");
    assert!(pinned.to_lowercase().ends_with("ac11pstool.ps1"), "exe= pinned {pinned}");

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], true);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v:#}");
    assert!(ran.exists(), "verify did not run the pinned ac11pstool.ps1: {v:#}");
    let row = verify_rows(&f, &task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], true);
    // AC14 (g)(10): the file handed to PowerShell is the one pinned
    assert_program_is(&row, &dir.path().join("ac11pstool.ps1"));
}

/// AC11 (d), Linux: `sh`, `bash` and `argv` run an in-tree `./tool` as the
/// named file - no extension is ever appended, and a `.` on PATH is dropped -
/// so a planted `tool.sh`, `tool.exe` or same-named PATH decoy never runs.
#[cfg(target_os = "linux")]
#[test]
fn verify_exe_sh_bash_and_argv_run_the_in_tree_program_that_was_pinned() {
    use std::os::unix::fs::PermissionsExt;
    let f = common::fixture();
    let (_task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let tool = worktree.join("tool");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    for s in ["sh", "bash", "argv"] {
        add_check_as(&f, &format!("tool-{s}"), "./tool", s, &[]);
    }
    approve(&f, Some(&run_id));
    for s in ["sh", "bash", "argv"] {
        assert_eq!(approved_exe_path(&f, &format!("tool-{s}")), "tool");
    }

    let marker = f.home_path().join("linux-decoy.marker");
    for name in ["tool.sh", "tool.exe", "./tool.bash"] {
        let p = worktree.join(name);
        std::fs::write(&p, format!("#!/bin/sh\necho x > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut entries = vec![std::path::PathBuf::from(".")];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let dotted = std::env::join_paths(entries).unwrap();
    let v = verify_env(&f, &run_id, &[("PATH", dotted.as_os_str())], true);
    for r in v["results"].as_array().unwrap() {
        assert_eq!(r["exit_code"], 0, "{r:#}");
    }
    assert!(!marker.exists(), "a planted file ran: {v:#}");
}

// ---------------------------------------------------------------------------
// AC12 (b): a shell check's program is a plain word. The fingerprint takes the
// first whitespace-separated word as the program; a shell would read quotes in
// it and run another file. `checks add` refuses such a command, and a row that
// got into state.db some other way is refused by verify.

/// `checks add` with `cmd` in `shell`: expects exit 1 with the plain-word
/// message, and no row written.
fn add_refused_as_not_plain(f: &common::Fixture, cmd: &str, shell: &str) {
    let a = f
        .cmd()
        .args(["checks", "add", "quoted", "--cmd", cmd, "--shell", shell, "--json"])
        .assert()
        .code(1);
    let out = a.get_output();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("the program name must be a plain word"), "{text}");
    let n: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM checks WHERE check_id = 'quoted'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "checks add wrote a row it refused");
}

/// Approves check `quoted` with `cmd` in `shell`, written straight into
/// state.db (as only a direct write could), with `envs` for the approval.
fn approve_quoted_row(
    f: &common::Fixture,
    cmd: &str,
    shell: &str,
    envs: &[(&str, &std::ffi::OsStr)],
) {
    add_check_as(f, "quoted", "exit 0", shell, &[]);
    f.state_conn()
        .execute("UPDATE checks SET cmd = ?1 WHERE check_id = 'quoted'", [cmd])
        .unwrap();
    approve_env(f, None, envs);
}

/// Runs `verify RUN --json` with `envs` and asserts the plain-word refusal.
fn assert_verify_refuses_not_plain(
    f: &common::Fixture,
    task_id: &str,
    run_id: &str,
    envs: &[(&str, &std::ffi::OsStr)],
    marker: &std::path::Path,
) {
    let mut c = f.cmd();
    c.env_remove("NoDefaultCurrentDirectoryInExePath");
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.args(["verify", run_id, "--json"]).output().unwrap();
    let v = json_of(out.stdout.clone());
    assert!(!marker.exists(), "the program the quotes named ran: {v:#}");
    assert_eq!(out.status.code(), Some(1), "{v:#}");
    let r = &v["results"][0];
    assert!(r["exit_code"].is_null(), "{r:#}");
    let err = r["error"].as_str().unwrap();
    assert!(err.contains("the program name must be a plain word"), "{err}");
    let row = verify_rows(f, task_id, "verify: ").pop().unwrap();
    assert_eq!(row["detail"]["fingerprint_ok"], false);
    assert_eq!(row["detail"]["inputs_checked"], false);
}

#[cfg(windows)]
#[test]
fn verify_exe_cmd_refuses_a_quoted_program_word() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    add_refused_as_not_plain(&f, r#""my tool" arg"#, "cmd");

    // `my tool.bat` on PATH: what cmd.exe runs for `"my tool" arg`, while the
    // fingerprint looked for a program named `my`.
    let dir = tempfile::tempdir().unwrap();
    let marker = f.home_path().join("quoted-cmd.marker");
    std::fs::write(dir.path().join("my tool.bat"), marker_batch(&marker)).unwrap();
    let mut entries = vec![dir.path().to_path_buf()];
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(entries).unwrap();
    approve_quoted_row(&f, r#""my tool" arg"#, "cmd", &[("PATH", path.as_os_str())]);
    assert_verify_refuses_not_plain(&f, &task_id, &run_id, &[("PATH", path.as_os_str())], &marker);
}

#[cfg(target_os = "linux")]
#[test]
fn verify_exe_sh_refuses_a_quoted_program_word() {
    use std::os::unix::fs::PermissionsExt;
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    add_refused_as_not_plain(&f, r#""./my tool" arg"#, "sh");

    let marker = f.home_path().join("quoted-sh.marker");
    let p = worktree.join("my tool");
    std::fs::write(&p, format!("#!/bin/sh\necho x > '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    approve_quoted_row(&f, r#""./my tool" arg"#, "sh", &[]);
    assert_verify_refuses_not_plain(&f, &task_id, &run_id, &[], &marker);
}

// ---------------------------------------------------------------------------
// AC14: a shell check is one program and plain arguments, and the program is
// the absolute path the fingerprint resolved, handed to the shell by thoth.
// The shell never looks the program's word up itself, so no splitting,
// escaping, built-in, alias or PATHEXT rule of its own can pick another file.

/// The fixed part of AC14 (a)'s refusal.
const NOT_PLAIN: &str = "a shell check must be one program and plain arguments";

/// Puts check `id` (`cmd` in `shell`, cwd `.`) straight into state.db - the
/// way only a direct write can, for a command `checks add` refuses - and
/// approves it from the run's worktree with `envs`.
#[cfg(windows)]
fn put_approved_row(
    f: &common::Fixture,
    id: &str,
    cmd: &str,
    shell: &str,
    run_id: &str,
    envs: &[(&str, &std::ffi::OsStr)],
) {
    f.state_conn()
        .execute(
            "INSERT OR REPLACE INTO checks (check_id, shell, cmd, cwd, inputs, created_at) \
             VALUES (?1, ?2, ?3, '.', '[]', 'n')",
            [id, shell, cmd],
        )
        .unwrap();
    approve_env(f, Some(run_id), envs);
}

/// `dirs` ahead of this process's own PATH.
fn path_with_first(dirs: &[&std::path::Path]) -> std::ffi::OsString {
    let mut entries: Vec<std::path::PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(entries).unwrap()
}

/// The newest `verify: ` evidence row for check `id`.
fn newest_row_for(f: &common::Fixture, task_id: &str, id: &str) -> serde_json::Value {
    verify_rows(f, task_id, "verify: ")
        .into_iter()
        .rev()
        .find(|r| r["detail"]["check_id"] == id)
        .unwrap_or_else(|| panic!("no verify row for {id}"))
}

/// Asserts that a row's `detail.program` is an absolute path naming `want`.
fn assert_program_is(row: &serde_json::Value, want: &std::path::Path) {
    let program = row["detail"]["program"]
        .as_str()
        .unwrap_or_else(|| panic!("no detail.program: {row:#}"));
    assert!(std::path::Path::new(program).is_absolute(), "{program}");
    assert_eq!(
        std::fs::canonicalize(program).unwrap(),
        std::fs::canonicalize(want).unwrap(),
        "{row:#}"
    );
}

/// AC14 (g)(2): cmd drops `^` from `.\to^ol.cmd` and would run `tool.cmd`;
/// the grammar refuses `^` before anything resolves or runs, in `verify` (a
/// row already in state.db) and in `checks add`.
#[cfg(windows)]
#[test]
fn ac14_a_caret_in_a_cmd_check_is_refused_by_verify_and_by_add() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let marker = f.home_path().join("caret.marker");
    std::fs::write(worktree.join("to^ol.cmd"), "@exit /b 0\r\n").unwrap();
    std::fs::write(worktree.join("tool.cmd"), marker_batch(&marker)).unwrap();
    put_approved_row(&f, "caret", r".\to^ol.cmd", "cmd", &run_id, &[]);

    let v = verify_env(&f, &run_id, &[], false);
    assert!(!marker.exists(), "cmd ran tool.cmd for .\\to^ol.cmd: {v:#}");
    let r = &v["results"][0];
    assert!(r["exit_code"].is_null(), "{r:#}");
    assert!(r["error"].as_str().unwrap().contains(NOT_PLAIN), "{r:#}");
    let row = newest_row_for(&f, &task_id, "caret");
    assert_eq!(row["detail"]["fingerprint_ok"], false);
    assert_eq!(row["detail"]["inputs_checked"], false);

    let count_checks = || -> i64 {
        f.state_conn().query_row("SELECT COUNT(*) FROM checks", [], |r| r.get(0)).unwrap()
    };
    let rows_before = count_checks();
    let a = f
        .cmd()
        .args(["checks", "add", "caret2", "--cmd", r".\to^ol.cmd", "--shell", "cmd", "--json"])
        .assert()
        .code(1);
    let out = a.get_output();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains(NOT_PLAIN), "{text}");
    let n: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM checks WHERE check_id = 'caret2'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "checks add wrote a row it refused");
    assert_eq!(count_checks(), rows_before, "checks add changed the number of checks rows");
}

/// AC14 (g)(3): cmd would split `.\x/tool.cmd` at the `/` and run `x.bat`;
/// thoth hands it the pinned `x\tool.cmd` instead.
#[cfg(windows)]
#[test]
fn ac14_cmd_runs_the_pinned_program_not_a_split_of_its_path() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    std::fs::create_dir_all(worktree.join("x")).unwrap();
    let real = worktree.join("x").join("tool.cmd");
    let m_real = f.home_path().join("x-real.marker");
    std::fs::write(&real, format!("@echo x> \"{}\"\r\n@exit /b 7\r\n", m_real.display())).unwrap();
    add_check_as(&f, "split", r".\x/tool.cmd", "cmd", &[]);
    approve(&f, Some(&run_id));

    let (m1, m2) = (f.home_path().join("x-cmd.marker"), f.home_path().join("x-bat.marker"));
    std::fs::write(worktree.join("x.cmd"), marker_batch(&m1)).unwrap();
    std::fs::write(worktree.join("x.bat"), marker_batch(&m2)).unwrap();
    let v = verify_env(&f, &run_id, &[], false);
    assert!(!m1.exists() && !m2.exists(), "cmd ran x.cmd or x.bat: {v:#}");
    assert_eq!(v["results"][0]["exit_code"], 7, "{v:#}");
    assert!(m_real.exists(), "x\\tool.cmd did not write its marker: {v:#}");
    let row = newest_row_for(&f, &task_id, "split");
    assert_eq!(row["detail"]["fingerprint_ok"], true, "{row:#}");
    assert_program_is(&row, &real);
}

/// AC14 (g)(4): `call` is a cmd built-in, not a program; a check naming it
/// is refused rather than handed to cmd with the program it would call.
#[cfg(windows)]
#[test]
fn ac14_a_cmd_built_in_carrying_another_program_is_refused() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let marker = f.home_path().join("call.marker");
    std::fs::write(worktree.join("tool.cmd"), marker_batch(&marker)).unwrap();
    add_check_as(&f, "call", r"call .\tool.cmd", "cmd", &[]);
    approve(&f, Some(&run_id));

    let v = verify_env(&f, &run_id, &[], false);
    assert!(!marker.exists(), "cmd ran tool.cmd through call: {v:#}");
    let r = &v["results"][0];
    assert!(r["exit_code"].is_null(), "{r:#}");
    assert!(r["error"].as_str().unwrap().contains("program not found"), "{r:#}");
    let row = newest_row_for(&f, &task_id, "call");
    assert_eq!(row["detail"]["fingerprint_ok"], false, "{row:#}");
    assert_eq!(row["detail"]["inputs_checked"], false, "{row:#}");
}

/// AC14 (g)(5): cmd reads a word starting `echo.` as its built-in echo; the
/// pinned `echo.cmd` on PATH is what must run.
#[cfg(windows)]
#[test]
fn ac14_a_cmd_program_named_like_a_built_in_runs_the_pinned_file() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let dir = tempfile::tempdir().unwrap();
    let marker = f.home_path().join("echo-cmd.marker");
    let real = dir.path().join("echo.cmd");
    std::fs::write(&real, format!("@echo x> \"{}\"\r\n@exit /b 7\r\n", marker.display())).unwrap();
    let path = path_with_first(&[dir.path()]);
    add_check_as(&f, "echo-file", "echo.cmd hello", "cmd", &[]);
    approve_env(&f, None, &[("PATH", path.as_os_str())]);

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], false);
    assert_eq!(v["results"][0]["exit_code"], 7, "{v:#}");
    assert!(marker.exists(), "the pinned echo.cmd did not run: {v:#}");
    assert_program_is(&newest_row_for(&f, &task_id, "echo-file"), &real);
}

/// AC14 (g)(6): PowerShell resolves `sort` to its alias Sort-Object; the
/// pinned `sort.ps1` on PATH is what must run.
#[cfg(windows)]
#[test]
fn ac14_a_powershell_program_named_like_an_alias_runs_the_pinned_file() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let dir = tempfile::tempdir().unwrap();
    let marker = f.home_path().join("sort-ps1.marker");
    let real = dir.path().join("sort.ps1");
    std::fs::write(
        &real,
        format!("Set-Content -LiteralPath '{}' -Value x\r\nexit 7\r\n", marker.display()),
    )
    .unwrap();
    let path = path_with_first(&[dir.path()]);
    add_check_as(&f, "sort-file", "sort", "powershell", &[]);
    approve_env(&f, None, &[("PATH", path.as_os_str())]);

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], false);
    assert_eq!(v["results"][0]["exit_code"], 7, "{v:#}");
    assert!(marker.exists(), "the pinned sort.ps1 did not run: {v:#}");
    assert_program_is(&newest_row_for(&f, &task_id, "sort-file"), &real);
}

/// AC14 (g)(7): absolute paths with a space, `&`, `^`, `'`, parentheses and
/// a literal `%PATH%` all run, through cmd and through PowerShell.
#[cfg(windows)]
#[test]
fn ac14_programs_whose_paths_hold_special_characters_run() {
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let root = tempfile::tempdir().unwrap();
    let d1 = root.path().join("a b&c^d'e(f)");
    let d2 = root.path().join("p%PATH%q");
    for d in [&d1, &d2] {
        std::fs::create_dir_all(d).unwrap();
    }
    let markers: Vec<std::path::PathBuf> =
        (1..=4).map(|i| f.home_path().join(format!("special-{i}.marker"))).collect();
    let cmd_script = |m: &std::path::Path| format!("@echo x> \"{}\"\r\n@exit /b 7\r\n", m.display());
    let ps_script = |m: &std::path::Path| {
        format!("Set-Content -LiteralPath '{}' -Value x\r\nexit 7\r\n", m.display())
    };
    let scripts = [
        d1.join("ac14a.cmd"),
        d2.join("ac14c.cmd"),
        d1.join("ac14b.ps1"),
        d2.join("ac14d.ps1"),
    ];
    std::fs::write(&scripts[0], cmd_script(&markers[0])).unwrap();
    std::fs::write(&scripts[1], cmd_script(&markers[1])).unwrap();
    std::fs::write(&scripts[2], ps_script(&markers[2])).unwrap();
    std::fs::write(&scripts[3], ps_script(&markers[3])).unwrap();
    let path = path_with_first(&[&d1, &d2]);
    let checks = [
        ("k1", "ac14a", "cmd"),
        ("k2", "ac14c", "cmd"),
        ("k3", "ac14b", "powershell"),
        ("k4", "ac14d", "powershell"),
    ];
    for (id, cmd, shell) in checks {
        add_check_as(&f, id, cmd, shell, &[]);
    }
    approve_env(&f, None, &[("PATH", path.as_os_str())]);

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], false);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "{v:#}");
    for r in results {
        assert_eq!(r["exit_code"], 7, "{r:#}");
    }
    for (i, (id, _, _)) in checks.iter().enumerate() {
        let row = newest_row_for(&f, &task_id, id);
        assert_eq!(row["detail"]["fingerprint_ok"], true, "{id}");
        assert!(markers[i].exists(), "{id}: its own marker file is missing: {row:#}");
        assert_program_is(&row, &scripts[i]);
    }
}

/// AC14 (g)(8): what Windows would not run as the pinned file itself is
/// refused: a `.vbs` (cmd), an extensionless file (powershell), a batch file
/// (powershell: use shell cmd), and a path PowerShell's `&` would read as a
/// wildcard. Control: an in-tree `.ps1` runs.
#[cfg(windows)]
#[test]
fn ac14_files_windows_would_not_run_as_themselves_are_refused() {
    let f = common::fixture();
    let (task_id, run_id, _, worktree) = settled_run(&f, common::Fixture::SPEC);
    let root = tempfile::tempdir().unwrap();
    let marker = f.home_path().join("ac14-8.marker");
    let plain = root.path().join("plain");
    let bracket = root.path().join("t[1]");
    let decoy = root.path().join("t1");
    for d in [&plain, &bracket, &decoy] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        plain.join("dtool.vbs"),
        format!(
            "CreateObject(\"Scripting.FileSystemObject\").CreateTextFile(\"{}\").Close\r\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::write(plain.join("etool"), "not a program").unwrap();
    std::fs::write(bracket.join("btool.ps1"), "exit 0\r\n").unwrap();
    std::fs::write(decoy.join("btool.ps1"), marker_ps1(&marker)).unwrap();
    std::fs::write(worktree.join("check.cmd"), marker_batch(&marker)).unwrap();
    let control_marker = f.home_path().join("control.marker");
    std::fs::write(
        worktree.join("check.ps1"),
        format!("Set-Content -LiteralPath '{}' -Value x\r\nexit 7\r\n", control_marker.display()),
    )
    .unwrap();
    let path = path_with_first(&[&plain, &bracket]);
    add_check_as(&f, "r1-vbs", "dtool", "cmd", &[]);
    add_check_as(&f, "r2-noext", "etool", "powershell", &[]);
    add_check_as(&f, "r3-batch", r".\check.cmd", "powershell", &[]);
    add_check_as(&f, "r4-bracket", "btool", "powershell", &[]);
    add_check_as(&f, "r5-control", r".\check.ps1", "powershell", &[]);
    approve_env(&f, Some(&run_id), &[("PATH", path.as_os_str())]);

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], false);
    assert!(!marker.exists(), "a refused program ran: {v:#}");
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 5, "{v:#}");
    for r in &results[..4] {
        assert!(r["exit_code"].is_null(), "{r:#}");
        assert!(r["error"].is_string(), "{r:#}");
        let row = newest_row_for(&f, &task_id, r["check_id"].as_str().unwrap());
        assert_eq!(row["detail"]["fingerprint_ok"], false, "{row:#}");
        assert_eq!(row["detail"]["inputs_checked"], false, "{row:#}");
    }
    let err = |i: usize| results[i]["error"].as_str().unwrap().to_string();
    assert!(err(0).contains("a '.vbs' file") && err(0).contains("cmd does not run"), "{v:#}");
    assert!(err(1).contains("without an extension"), "{v:#}");
    assert!(err(2).contains("use shell cmd for this check"), "{v:#}");
    assert!(err(3).contains("t[1]") && err(3).contains("wildcard"), "{v:#}");
    assert_eq!(results[4]["exit_code"], 7, "{v:#}");
    assert!(control_marker.exists(), "check.ps1 did not write its marker: {v:#}");
}

/// AC14 (g)(9), Linux: `test` is an sh/bash built-in; the pinned `test`
/// script first on PATH is what must run. And `exit` is no program at all.
#[cfg(target_os = "linux")]
#[test]
fn ac14_sh_and_bash_run_the_pinned_program_not_a_built_in() {
    use std::os::unix::fs::PermissionsExt;
    let f = common::fixture();
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let dir = tempfile::tempdir().unwrap();
    let marker = f.home_path().join("test-builtin.marker");
    let real = dir.path().join("test");
    std::fs::write(&real, format!("#!/bin/sh\necho x > '{}'\nexit 7\n", marker.display())).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = path_with_first(&[dir.path()]);
    for s in ["sh", "bash"] {
        add_check_as(&f, &format!("t-{s}"), "test -f Cargo.toml", s, &[]);
    }
    add_check_as(&f, "u-exit", "exit 0", "sh", &[]);
    approve_env(&f, None, &[("PATH", path.as_os_str())]);

    let v = verify_env(&f, &run_id, &[("PATH", path.as_os_str())], false);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{v:#}");
    for r in &results[..2] {
        assert_eq!(r["exit_code"], 7, "{r:#}");
    }
    assert!(marker.exists(), "the pinned test script did not run: {v:#}");
    for s in ["sh", "bash"] {
        assert_program_is(&newest_row_for(&f, &task_id, &format!("t-{s}")), &real);
    }
    assert!(results[2]["exit_code"].is_null(), "{v:#}");
    assert!(
        results[2]["error"].as_str().unwrap().contains("program not found"),
        "{v:#}"
    );
    let row = newest_row_for(&f, &task_id, "u-exit");
    assert_eq!(row["detail"]["fingerprint_ok"], false, "{row:#}");
    assert_eq!(row["detail"]["inputs_checked"], false, "{row:#}");
}

/// AC15 (1): a blank `--cmd` for cmd, powershell, sh or bash gets AC14 (a)'s
/// refusal from `checks add`, exactly as `verify` words it for a stored row;
/// only argv keeps `--cmd must not be blank`. Nothing is written either way.
#[test]
fn ac15_checks_add_with_a_blank_cmd_is_refused_in_the_shell_grammar_wording() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    // `checks add` has no platform check, so all four shells reach the grammar
    // refusal on both platforms.
    let list_checks = || {
        let list = f.cmd().args(["checks", "list", "--json"]).assert().success();
        let v: serde_json::Value = serde_json::from_slice(&list.get_output().stdout).unwrap();
        let n = v
            .get("checks")
            .and_then(|c| c.as_array())
            .or_else(|| v.as_array())
            .unwrap_or_else(|| panic!("unexpected checks list shape: {v:#}"))
            .len();
        (n, v)
    };
    let before = list_checks().0;
    for shell in ["cmd", "powershell", "sh", "bash"] {
        let a = f
            .cmd()
            .args(["checks", "add", "blank", "--cmd", "   ", "--shell", shell])
            .assert()
            .code(1);
        let text = String::from_utf8_lossy(&a.get_output().stderr).to_string();
        assert!(text.contains(NOT_PLAIN), "{shell}: {text}");
        assert!(text.contains("write it in a script file"), "{shell}: {text}");
        assert!(text.contains("use shell argv for this check"), "{shell}: {text}");
    }

    let a = f
        .cmd()
        .args(["checks", "add", "blank", "--cmd", "   ", "--shell", "argv"])
        .assert()
        .code(1);
    let text = String::from_utf8_lossy(&a.get_output().stderr).to_string();
    assert!(text.contains("--cmd must not be blank"), "{text}");

    // the real `checks list --json` shows nothing was added
    let after = list_checks();
    assert_eq!(after.0, before, "{:#}", after.1);
    assert!(!after.1.to_string().contains("\"blank\""), "{:#}", after.1);

    let n: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM checks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "checks add wrote a row it refused");
}

/// A folder under the fixture's home (outside the repo and every worktree)
/// holding the program `pinme`, a copy of the stand-in binary `src`.
fn pinme_dir(f: &common::Fixture, src: &str) -> std::path::PathBuf {
    let dir = f.home_path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let to = dir.join(if cfg!(windows) { "pinme.exe" } else { "pinme" });
    std::fs::copy(src, &to).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// `--exe-pin path`: verify still runs a check whose program's bytes changed
/// (the hash-pinned twin is refused) and says how it was pinned.
#[test]
fn verify_runs_a_path_only_check_after_its_program_changed_and_says_so() {
    let f = common::fixture();
    // 1.
    let (task_id, run_id, _, _) = settled_run(&f, common::Fixture::SPEC);
    let bin = pinme_dir(&f, env!("CARGO_BIN_EXE_fake-claude"));
    let mut dirs = vec![bin.clone()];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let path = std::env::join_paths(dirs).unwrap();
    // 2.
    f.cmd()
        .env("PATH", &path)
        .args(["checks", "add", "byhash", "--cmd", "pinme --version", "--shell", "argv"])
        .assert()
        .success();
    f.cmd()
        .env("PATH", &path)
        .args(["checks", "add", "bypath", "--cmd", "pinme --version", "--shell", "argv", "--exe-pin", "path"])
        .assert()
        .success();
    f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
    // 3.
    pinme_dir(&f, env!("CARGO_BIN_EXE_fake-codex"));
    // 4.
    let out = f
        .cmd()
        .env("PATH", &path)
        .args(["verify", &run_id, "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v = json_of(out);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{v:#}");
    assert_eq!(results[0]["check_id"], "byhash", "{v:#}");
    assert!(results[0]["exit_code"].is_null(), "{v:#}");
    assert!(
        results[0]["error"].as_str().unwrap().contains("fingerprint mismatch for check byhash: exe="),
        "{v:#}"
    );
    assert_eq!(results[0]["exe_pin"], "hash", "{v:#}");
    assert_eq!(results[1]["check_id"], "bypath", "{v:#}");
    assert_eq!(results[1]["exit_code"], 0, "{v:#}");
    assert!(results[1]["error"].is_null(), "{v:#}");
    assert_eq!(results[1]["exe_pin"], "path", "{v:#}");
    // 5.
    let rows = verify_rows(&f, &task_id, "verify: ");
    assert_eq!(rows.len(), 2, "{rows:#?}");
    let by = |id: &str| rows.iter().find(|r| r["detail"]["check_id"] == id).unwrap().clone();
    let p = by("bypath");
    assert_eq!(p["detail"]["fingerprint_ok"], true, "{p:#}");
    assert_eq!(p["detail"]["exe_pin"], "path", "{p:#}");
    let program = p["detail"]["program"].as_str().unwrap();
    assert!(program.ends_with(if cfg!(windows) { "pinme.exe" } else { "pinme" }), "{p:#}");
    let h = by("byhash");
    assert_eq!(h["detail"]["fingerprint_ok"], false, "{h:#}");
    assert_eq!(h["detail"]["exe_pin"], "hash", "{h:#}");
    // 6.
    let out = f
        .cmd()
        .env("PATH", &path)
        .args(["verify", &run_id, "--dry-run", "--json"])
        .assert()
        .get_output()
        .stdout
        .clone();
    let v = json_of(out);
    let dry = |id: &str| v["results"].as_array().unwrap().iter().find(|r| r["check_id"] == id).unwrap().clone();
    assert_eq!(dry("bypath")["would_refuse"], false, "{v:#}");
    assert_eq!(dry("bypath")["fingerprint_ok"], true, "{v:#}");
    assert_eq!(dry("bypath")["exe_pin"], "path", "{v:#}");
    assert_eq!(dry("byhash")["would_refuse"], true, "{v:#}");
    // 7.
    let out = f.cmd().env("PATH", &path).args(["verify", &run_id]).assert().get_output().stdout.clone();
    let human = String::from_utf8(out).unwrap();
    let line = human.lines().find(|l| l.contains("bypath: pinme --version")).expect(&human);
    assert!(line.contains("[exe pinned by path only]"), "{human}");
    let line = human.lines().find(|l| l.contains("byhash: pinme --version")).expect(&human);
    assert!(line.contains("REFUSED"), "{human}");
    assert!(!line.contains("[exe pinned by path only]"), "{human}");
}
