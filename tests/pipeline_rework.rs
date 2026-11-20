//! AUTO-1b: `thoth pipeline start` continues instead of stopping: REJECT
//! rework on the next implementer entry, quota/auth hand-over, one crash
//! resume, one doubled-timeout retry, reviewer rotation, the quota threshold
//! preflight and the opt-in fold of findings into the spec.
//!
//! Everything is observed through the CLI (`pipeline start/show --json`,
//! `events --task --json`, `status --json`, `review show/packet --json`,
//! `recommend`, `profile usage`), the MCP server, the user-visible pipeline
//! folder and the fakes' launch record (`FAKE_ATTEMPTS_FILE`). Workers are
//! the fake CLIs only, driven across providers by one shared
//! `FAKE_SCENARIO_SEQUENCE`; quota is seeded only as codex rollout files or
//! through fake agy output. No test opens a database.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REASON: &str = "rework fixture: fake workers in a temporary repo";
const IMPL: &str = "claude-b:sonnet:medium";
const REVIEW: &str = "codex-a:gpt-5.6-luna:medium";
/// A same-provider stand-in reviewer at another model than IMPL.
const CLAUDE_REVIEW: &str = "claude-a:opus:medium";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");
const FUTURE: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z
const PAST: u64 = 1_000_000_000; // 2001-09-09
const IMPORTANT_SPEC: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":["keep it short"],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;
const APPROVE_DOC: &str = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}]}"#;

// --- running thoth ------------------------------------------------------------

/// `f.cmd()` without inherited variables whose names hold KEY, TOKEN, SECRET
/// or PASSWORD (names only are read) and without the two benchmark keys.
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c.env_remove("AISTUPIDLEVEL_API_KEY").env_remove("ARTIFICIALANALYSIS_API_KEY");
    c.env_remove("THOTH_PIPELINE_ID");
    c.env("FAKE_CHECK_DIR", f.home_path().join("check-gates"));
    c.env("THOTH_RETRY_WAITS", "0,0");
    c
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}\nstderr: {}", self.stdout, self.stderr))
    }
    fn error(&self) -> String {
        let last = self.stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        let v: Value = serde_json::from_str(last).unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", self.stderr));
        s(&v["error"])
    }
}

fn exec(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let mut c = cmd(f);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.args(args).output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let o = exec(f, args, envs);
    assert_eq!(o.code, 0, "thoth {args:?} exited {}\nstdout: {}\nstderr: {}", o.code, o.stdout, o.stderr);
    o.json()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

// --- fixtures ---------------------------------------------------------------------

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) -> PathBuf {
    let cfg = f.home_path().join("profiles").join(id);
    fs::create_dir_all(&cfg).unwrap();
    let dir = cfg.display().to_string();
    ok(f, &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &dir, "--json"], &[]);
    cfg
}

/// Claude and Codex profiles and one approved green check.
fn setup() -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    ok(&f, &["init", "--json"], &[]);
    add_profile(&f, "claude-b", "anthropic", "claude-code");
    add_profile(&f, "claude-a", "anthropic", "claude-code");
    add_profile(&f, "codex-a", "openai", "codex");
    fs::create_dir_all(f.home_path().join("check-gates")).unwrap();
    ok(&f, &["checks", "add", "green", "--cmd", &format!("\"{FAKE_CHECK}\" exit 0"), "--json"], &[]);
    ok(&f, &["checks", "approve", "--reason", "fixture checks", "--json"], &[]);
    f
}

fn rand_name() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn write_file(f: &Fixture, prefix: &str, body: &str) -> String {
    let file = f.home_path().join(format!("{prefix}-{}.json", rand_name()));
    fs::write(&file, body).unwrap();
    file.display().to_string()
}

fn create_with(f: &Fixture, spec: &str) -> String {
    let file = write_file(f, "spec", spec);
    s(&ok(f, &["task", "create", "--title", "rework", "-f", &file, "--json"], &[])["task_id"])
}

fn create(f: &Fixture) -> String {
    create_with(f, Fixture::SPEC)
}

/// An important-risk task whose spec has an APPROVE spec review, so its
/// first dispatch is allowed.
fn create_important(f: &Fixture) -> String {
    let task = create_with(f, IMPORTANT_SPEC);
    let doc = write_file(f, "approve", APPROVE_DOC);
    ok(f, &["spec", "review", "record", &task, "-f", &doc, "--json"], &[]);
    task
}

/// One launch record and one scenario list shared by every fake.
struct Seq {
    task: String,
    attempts: String,
    sequence: String,
}

impl Seq {
    fn new(f: &Fixture, task: &str, sequence: &str) -> Seq {
        let attempts = f.home_path().join(format!("attempts-{}.txt", rand_name()));
        Seq { task: task.to_string(), attempts: attempts.display().to_string(), sequence: sequence.to_string() }
    }
    fn env(&self) -> Vec<(&str, &str)> {
        vec![
            ("FAKE_TASK_ID", self.task.as_str()),
            ("FAKE_ATTEMPTS_FILE", self.attempts.as_str()),
            ("FAKE_SCENARIO_SEQUENCE", self.sequence.as_str()),
        ]
    }
    /// The fakes' launch record: `fresh ID` / `resume ID`, one per worker launch.
    fn launches(&self) -> Vec<String> {
        fs::read_to_string(&self.attempts)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn start_args(task: &str, implementers: &[&str], reviewers: &[&str], extra: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = ["pipeline", "start", task].iter().map(|x| x.to_string()).collect();
    for i in implementers {
        a.extend(["--implementer".to_string(), i.to_string()]);
    }
    for r in reviewers {
        a.extend(["--reviewer".to_string(), r.to_string()]);
    }
    a.extend(["--reason".to_string(), REASON.to_string()]);
    a.extend(extra.iter().map(|x| x.to_string()));
    a.push("--json".into());
    a
}

fn start(f: &Fixture, task: &str, implementers: &[&str], reviewers: &[&str], extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let args = start_args(task, implementers, reviewers, extra);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let v = ok(f, &args, envs);
    assert_eq!(v["status"], "running", "{v}");
    v
}

fn show(f: &Fixture, task: &str) -> Value {
    ok(f, &["pipeline", "show", task, "--json"], &[])
}

fn log_of(doc: &Value) -> String {
    doc["dir"]
        .as_str()
        .and_then(|d| fs::read_to_string(Path::new(d).join("pipeline.log")).ok())
        .unwrap_or_default()
}

fn wait_stopped(f: &Fixture, task: &str) -> Value {
    let o = exec(f, &["pipeline", "show", task, "--wait", "300s", "--json"], &[]);
    let v = o.json();
    assert_eq!(o.code, 0, "still running after 300s: {v:#}\n{}", log_of(&v));
    assert_eq!(v["status"], "stopped", "{v:#}");
    v
}

fn wait_show(f: &Fixture, task: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let v = show(f, task);
        if pred(&v) {
            return v;
        }
        if v["status"] == "stopped" || Instant::now() >= deadline {
            panic!("never saw {what}: {v:#}\n{}", log_of(&v));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[])["events"].as_array().unwrap().clone()
}

fn of_type(events: &[Value], ty: &str) -> Vec<Value> {
    events.iter().filter(|e| e["type"] == ty).cloned().collect()
}

fn steps(doc: &Value, kind: &str) -> Vec<Value> {
    doc["steps"].as_array().unwrap().iter().filter(|s| s["kind"] == kind).cloned().collect()
}

/// The `pipeline_step` event of the step with this index.
fn step_event(f: &Fixture, task: &str, index: &Value) -> Value {
    of_type(&events(f, task), "pipeline_step")
        .into_iter()
        .find(|e| e["detail"]["index"] == *index)
        .unwrap_or_else(|| panic!("no step {index}"))["detail"]
        .clone()
}

fn argv(f: &Fixture, task: &str, step: &Value) -> Vec<String> {
    step_event(f, task, &step["index"])["argv"].as_array().unwrap().iter().map(s).collect()
}

fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    argv.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn stop_reason(doc: &Value) -> String {
    s(&doc["stop"]["reason"])
}

fn status(f: &Fixture, task: &str) -> Value {
    ok(f, &["status", task, "--json"], &[])
}

fn run_row(f: &Fixture, task: &str, run: &str) -> Value {
    status(f, task)["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run).cloned().unwrap()
}

fn skipped_profiles(step: &Value) -> Vec<String> {
    step["skipped"].as_array().unwrap().iter().map(|k| s(&k["profile"])).collect()
}

/// The preflight step of `kind` whose result names list `entry` of `role`.
fn preflight(doc: &Value, kind: &str, role: &str, entry: u64) -> Value {
    steps(doc, kind)
        .into_iter()
        .rfind(|st| st["result"]["role"] == role && st["result"]["entry"] == entry)
        .unwrap_or_else(|| panic!("no {kind} step for {role} entry {entry}: {doc:#}"))
}

/// A codex profile whose only rollout file holds one general-bucket reading
/// observed at `observed`: five-hour `used`% resetting at `resets_at`, weekly
/// 1% resetting in 2100; with `extra`, a newer reserve bucket at that percent.
fn seed_codex(cfg: &Path, observed: &str, used: f64, resets_at: u64, extra: Option<f64>) {
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let line = |ts: &str, limits: Value| {
        json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": limits}}).to_string() + "\n"
    };
    let mut text = line(
        observed,
        json!({"limit_id": "codex", "limit_name": null,
            "primary": {"used_percent": used, "window_minutes": 300, "resets_at": resets_at},
            "secondary": {"used_percent": 1.0, "window_minutes": 10080, "resets_at": FUTURE}}),
    );
    if let Some(x) = extra {
        text += &line(
            observed,
            json!({"limit_id": "base_model_inference", "limit_name": "gpt-reserve",
                "primary": {"used_percent": x, "window_minutes": 10080, "resets_at": FUTURE}, "secondary": null}),
        );
    }
    fs::write(sessions.join("rollout.jsonl"), text).unwrap();
}

// --- AC4/AC6/AC9: rework after a REJECT ---------------------------------------------------

/// Low risk: REJECT, then the second list entry (a duplicate, so list
/// position not profile identity decides) reworks attempt 2 and passes; the
/// reviewer position is kept across attempts.
#[test]
fn a_low_risk_reject_is_reworked_on_the_next_entry_and_passes() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_pass");
    let started = start(&f, &task, &[IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let id = s(&started["pipeline_id"]);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2, "{doc:#}");
    assert_eq!((d[0]["entry_index"].clone(), d[1]["entry_index"].clone()), (json!(1), json!(2)));
    assert_eq!((d[0]["attempt"].clone(), d[1]["attempt"].clone()), (json!(1), json!(2)));
    assert_eq!(d[1]["result"]["attempt"], 2);
    assert_eq!(d[1]["result"]["rework"], true);
    assert!(d[1]["result"]["folded_findings"].as_i64().unwrap() > 0, "{}", d[1]);
    assert_eq!(d[1]["skipped"], json!([]));
    // the rework's reason: pipeline id, step, REJECT id and attempt, entry
    // position and the original reason
    let reviews = of_type(&events(&f, &task), "verdict");
    let reject_id = s(&reviews.iter().find(|e| e["detail"]["verdict"] == "REJECT").unwrap()["detail"]["review_id"]);
    let a = argv(&f, &task, &d[1]);
    let reason = flag_value(&a, "--reason").unwrap();
    for part in [
        format!("pipeline {id} step {}", d[1]["index"]),
        format!("REJECT {reject_id} (attempt 1 rejected, now attempt 2)"),
        "implementer entry 2 of 2".to_string(),
        REASON.to_string(),
    ] {
        assert!(reason.contains(&part), "{part:?} missing from {reason}");
    }
    assert_eq!(flag_value(&a, "--allow-attempt"), None, "attempt 2 is within the task's cap");
    assert_eq!(flag_value(&a, "--model").as_deref(), Some("sonnet"));
    assert_eq!(flag_value(&a, "--effort").as_deref(), Some("medium"));
    // the reviewer position is kept: both reviews used entry 1
    let r = steps(&doc, "review_start");
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|x| x["entry_index"] == 1), "{r:?}");
    // AC9 progress fields agree across show, status and the events
    assert_eq!(doc["implementer_index"], 1);
    assert_eq!(doc["reviewer_index"], 0);
    assert_eq!(doc["attempts_used"], 2);
    let st = status(&f, &task);
    assert_eq!(st["attempt"], 2);
    for key in ["implementer_index", "reviewer_index", "attempts_used"] {
        assert_eq!(st["pipeline"][key], doc[key], "{key}");
    }
    let ev = events(&f, &task);
    let stopped = &of_type(&ev, "pipeline_stopped")[0]["detail"];
    for key in ["implementer_index", "reviewer_index", "attempts_used"] {
        assert_eq!(stopped[key], doc[key], "{key}");
    }
    let rework_ev = step_event(&f, &task, &d[1]["index"]);
    assert_eq!(rework_ev["entry_index"], 2);
    assert_eq!(rework_ev["attempt"], 2);
    assert_eq!(rework_ev["implementer_index"], 1);
    // the packet's runs agree with the step attempts
    let packet = ok(&f, &["review", "packet", &task, "--json"], &[]);
    let impl_attempts: Vec<i64> = packet["runs"].as_array().unwrap().iter().filter(|r| r["role"] == "implementer").map(|r| r["attempt"].as_i64().unwrap()).collect();
    assert_eq!(impl_attempts, vec![1, 2]);
    // four worker launches: implement, review, implement, review
    let launches = seq.launches();
    assert_eq!(launches.len(), 4, "{launches:?}");
    assert!(launches.iter().all(|l| l.starts_with("fresh ")), "{launches:?}");
    // the human show leads with the conclusion, then entry summaries
    let human = exec(&f, &["pipeline", "show", &task], &[]).stdout;
    assert!(human.lines().next().unwrap().contains("stopped: pass"), "{human}");
    assert!(human.contains("entry 2"), "{human}");
}

/// AC4: a REJECT whose next attempt passes the authorised max-attempts stops
/// attempts_exhausted with the unmet criteria and the review brief.
#[test]
fn a_reject_past_max_attempts_stops_with_the_unmet_criteria() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_reject");
    start(&f, &task, &[IMPL, IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "attempts_exhausted", "{doc:#}\n{}", log_of(&doc));
    let review = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(doc["stop"]["evidence"]["attempt"], 3);
    assert_eq!(doc["stop"]["evidence"]["max_attempts"], 2);
    assert_eq!(doc["stop"]["evidence"]["unmet"], json!(["AC1"]));
    assert_eq!(doc["stop"]["evidence"]["review_id"], review["review_id"]);
    assert_eq!(doc["stop"]["evidence"]["brief"], review["brief"]);
    if let Some(b) = review["brief"].as_str() {
        assert!(s(&doc["stop"]["summary"]).starts_with(b.trim()), "{doc:#}");
    }
    assert_eq!(steps(&doc, "dispatch").len(), 2, "no third dispatch");
    assert_eq!(doc["attempts_used"], 2);
    assert_eq!(seq.launches().len(), 4);
}

/// AC4/AC6: `--allow-attempt` only past the task cap and inside the
/// pipeline's max-attempts; dispatch's own stuck refusal stops `stuck` with
/// its diagnostic, never bypassed.
#[test]
fn allow_attempt_stays_inside_max_attempts_and_a_stuck_dispatch_stops_stuck() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_reject,hooks,review_reject");
    start(&f, &task, &[IMPL, IMPL, IMPL, IMPL], &[REVIEW], &["--max-attempts", "5"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "stuck", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 4, "{doc:#}");
    let allow: Vec<Option<String>> = d.iter().map(|x| flag_value(&argv(&f, &task, x), "--allow-attempt")).collect();
    assert_eq!(allow, vec![None, None, Some("3".to_string()), Some("4".to_string())]);
    assert_eq!(d[3]["ok"], false);
    let stuck = &doc["stop"]["evidence"]["stuck"];
    assert!(stuck["kind"].is_string() && stuck["value"].is_string(), "{doc:#}");
    assert!(s(&doc["stop"]["summary"]).contains("is stuck"), "{doc:#}");
    assert_eq!(d[3]["error"], doc["stop"]["evidence"]["error"]);
}

// --- AC5: important/high rework ------------------------------------------------------------

#[test]
fn an_important_reject_stops_for_a_rewrite_and_restarts_from_the_first_entry() {
    let f = setup();
    let task = create_important(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_pass");
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "spec_rewrite_needed", "{doc:#}\n{}", log_of(&doc));
    let next: Vec<String> = doc["stop"]["next"].as_array().unwrap().iter().map(s).collect();
    assert!(next.contains(&format!("thoth task update-spec {task} -f FILE --json")), "{next:?}");
    assert!(steps(&doc, "update_spec").is_empty(), "no fold without the opt-in");
    assert_eq!(steps(&doc, "dispatch").len(), 1);
    let before = ok(&f, &["review", "packet", &task, "--json"], &[])["spec"]["constraints"].clone();
    assert_eq!(before, json!(["keep it short"]), "the spec is untouched");

    // the commander rewrites the spec; a new pipeline from reject starts at entry 1
    let rewritten = IMPORTANT_SPEC.replace("\"keep it short\"", "\"keep it short\",\"exercise AC1 in a test\"");
    let file = write_file(&f, "rewrite", &rewritten);
    ok(&f, &["task", "update-spec", &task, "-f", &file, "--json"], &[]);
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let again = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&again), "pass", "{again:#}\n{}", log_of(&again));
    let d = steps(&again, "dispatch");
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["entry_index"], 1, "entry reject searches from the first item");
    assert_eq!(d[0]["result"]["attempt"], 2);
    assert_eq!(d[0]["result"]["rework"], true);
    assert_eq!(d[0]["result"]["folded_findings"], 0, "the rewritten spec is the answer");
    assert_eq!(of_type(&events(&f, &task), "pipeline_started")[1]["detail"]["entry_status"], "reject");
}

/// AC5 (review of attempt 1): an author's APPROVE through `spec review
/// confirm` after the REJECT is the rewrite; no second mechanism. A new
/// pipeline from reject reworks from entry 1 without folding anything.
#[test]
fn an_authors_approve_after_the_reject_counts_as_the_rewrite() {
    let f = setup();
    let task = create_important(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_pass");
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "spec_rewrite_needed", "{doc:#}\n{}", log_of(&doc));
    assert_eq!(status(&f, &task)["status"], "reject");

    // the commander's route: an edit spec review, then the author's APPROVE
    let launch = |verb: &str, scenario: &str, extra: &[&str]| {
        let mut args = vec!["spec", "review", verb, task.as_str(), "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--reason", REASON];
        args.extend_from_slice(extra);
        args.push("--json");
        let r = ok(&f, &args, &[("FAKE_TASK_ID", task.as_str()), ("FAKE_SCENARIO", scenario)]);
        ok(&f, &["wait", "--run", &s(&r["run_id"]), "--timeout", "120s", "--json"], &[])
    };
    assert_eq!(launch("start", "spec_review_edit", &["--edit"])["verdict"], "REVISE");
    assert_eq!(launch("confirm", "spec_confirm_approve", &[])["verdict"], "APPROVE");
    let updated = of_type(&events(&f, &task), "spec_updated");
    assert_eq!(updated.len(), 1, "{updated:?}");
    assert_eq!(updated[0]["detail"]["source"], "author_check");
    let approved = ok(&f, &["review", "packet", &task, "--json"], &[])["spec"].clone();
    assert_ne!(approved["constraints"], json!(["keep it short"]), "the author's edits replaced the spec: {approved}");

    // from reject: entry 1, attempt 2, the author-approved spec as it stands
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &[], &seq.env());
    let again = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&again), "pass", "{again:#}\n{}", log_of(&again));
    assert!(steps(&again, "update_spec").is_empty(), "nothing folded");
    let d = steps(&again, "dispatch");
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["entry_index"], 1);
    assert_eq!(d[0]["result"]["attempt"], 2);
    assert_eq!(d[0]["result"]["rework"], true);
    assert_eq!(d[0]["result"]["folded_findings"], 0);
    assert_eq!(ok(&f, &["review", "packet", &task, "--json"], &[])["spec"], approved, "the spec was left as approved");
    assert_eq!(of_type(&events(&f, &task), "spec_updated").len(), 1);
}

/// The controlled REJECT the fake Claude reviewer (`review_reject`) records:
/// AC1 unmet and one `minor` extra finding, each with its fixed note, both
/// labelled spec_mismatch (a blocking category) and citing the one evidence
/// code `findings` shows. Returns that code, after checking the fixture is
/// the one the expected lines below are written for.
fn fake_reject_code(findings: &Value) -> String {
    let unmet = &findings["criteria"][0];
    let extra = &findings["extra_findings"][0];
    assert_eq!((unmet["id"].clone(), unmet["verdict"].clone(), unmet["category"].clone()), (json!("AC1"), json!("unmet"), json!("spec_mismatch")), "{findings}");
    assert_eq!((extra["severity"].clone(), extra["category"].clone()), (json!("minor"), json!("spec_mismatch")), "{findings}");
    assert_eq!(findings["extra_findings"].as_array().unwrap().len(), 1, "{findings}");
    assert_eq!(unmet["evidence"].as_array().unwrap().len(), 1, "{findings}");
    assert_eq!(extra["evidence"], unmet["evidence"], "{findings}");
    s(&unmet["evidence"][0])
}

/// The constraint lines that controlled REJECT folds to, as the contract
/// writes them (`ID: NOTE (evidence CODES)`, then `SEVERITY: NOTE (evidence
/// CODES)`), spelled out rather than computed.
fn fake_reject_lines(code: &str) -> Vec<String> {
    vec![
        format!("AC1: no captured evidence shows this criterion being exercised (evidence {code})"),
        format!("minor: the new test asserts a constant rather than the behaviour (evidence {code})"),
    ]
}

/// AC4/AC5: with --fold-on-reject an important task's findings are appended
/// unchanged after a marker, the spec replaced through task update-spec (a
/// step with the new sha), the rework folds nothing more; the third REJECT
/// in a row sharing AC1 stops repeated_reject.
#[test]
fn fold_on_reject_appends_the_findings_unchanged_and_three_repeats_stop() {
    let f = setup();
    let task = create_important(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,hooks,review_reject,hooks,review_reject");
    let env = seq.env();
    let started = start(&f, &task, &[IMPL, IMPL, IMPL, IMPL], &[CLAUDE_REVIEW], &["--fold-on-reject", "--max-attempts", "4"], &env);
    let id = s(&started["pipeline_id"]);
    assert_eq!(started["settings"]["fold_on_reject"], true);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "repeated_reject", "{doc:#}\n{}", log_of(&doc));
    let ev = events(&f, &task);
    let rejects: Vec<String> = of_type(&ev, "verdict").iter().filter(|e| e["detail"]["verdict"] == "REJECT").map(|e| s(&e["detail"]["review_id"])).collect();
    assert_eq!(rejects.len(), 3);
    assert_eq!(doc["stop"]["evidence"]["criterion"], "AC1");
    assert_eq!(doc["stop"]["evidence"]["review_ids"], json!(rejects));

    // two folds (after REJECT 1 and 2), each its own update_spec step
    let u = steps(&doc, "update_spec");
    assert_eq!(u.len(), 2, "{doc:#}");
    let spec_updates = of_type(&ev, "spec_updated");
    for (i, step) in u.iter().enumerate() {
        assert_eq!(step["ok"], true, "{step}");
        assert_eq!(step["run_id"], Value::Null);
        assert_eq!(step["entry_index"], Value::Null);
        assert_eq!(step["result"]["review_id"], rejects[i].as_str());
        assert_eq!(step["result"]["spec_sha256"], spec_updates[i]["detail"]["spec_sha256"]);
        let file = PathBuf::from(s(&step["result"]["file"]));
        assert!(file.ends_with(format!("step-{:03}.spec.json", step["index"].as_u64().unwrap())), "{file:?}");
        let written: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert!(written["constraints"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().starts_with(&format!("REJECT {} (", rejects[i]))));
    }
    // every rework dispatch replaced the stored spec rather than folding again
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 3);
    for x in &d[1..] {
        assert_eq!(x["result"]["rework"], true);
        assert_eq!(x["result"]["folded_findings"], 0);
    }
    // the packet's spec holds the original constraint, then per REJECT the
    // marker and that REJECT's two finding lines, unchanged
    let packet = ok(&f, &["review", "packet", &task, "--json"], &[]);
    let constraints: Vec<String> = packet["spec"]["constraints"].as_array().unwrap().iter().map(s).collect();
    let reviews_shown = packet["reviews"].as_array().unwrap().clone();
    let mut expected = vec!["keep it short".to_string()];
    for (i, rid) in rejects.iter().take(2).enumerate() {
        assert_eq!(reviews_shown[i]["verdict"], "REJECT");
        expected.push(format!("REJECT {rid} (attempt {}), folded as-is by pipeline {id}:", i + 1));
        expected.extend(fake_reject_lines(&fake_reject_code(&reviews_shown[i]["findings"])));
    }
    assert_eq!(constraints, expected);
    assert!(packet["spec"]["goal"] == "say hello" && packet["spec"]["acceptance_criteria"] == json!(["AC1: prints hello"]));
}

/// RD masking: a REJECT recorded with a fake secret in its finding (an
/// external `review record`; the runner keeps secret-looking values out of a
/// worker's environment) is folded from `reject`, and the secret appears in
/// no pipeline output, event or file, the folded spec file included.
#[test]
fn a_secret_in_a_folded_finding_never_reaches_the_pipeline_outputs() {
    let f = setup();
    let task = create_important(&f);
    let seq = Seq::new(&f, &task, "hooks,hooks,review_pass");
    dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &seq.env());
    let code = s(&ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"][0]["code"]);
    let secret = ["ghp_", "Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2Jh1Gf0EdCbA9"].concat();
    let review = write_file(&f, "review", &json!({"verdict": "REJECT", "findings": {
        "criteria": [{"id": "AC1", "verdict": "unmet", "evidence": [code], "note": format!("AC1 is never exercised; token seen: {secret}")}],
        "extra_findings": [],
    }}).to_string());
    let recorded = exec(&f, &["review", "record", &task, "-f", &review, "--json"], &[]);
    assert_eq!(recorded.code, 0, "{}", recorded.stderr);
    assert_eq!(status(&f, &task)["status"], "reject");
    start(&f, &task, &[IMPL], &[REVIEW], &["--fold-on-reject"], &seq.env());
    let doc = wait_stopped(&f, &task);
    let u = steps(&doc, "update_spec");
    assert_eq!(u.len(), 1, "{doc:#}\n{}", log_of(&doc));
    // whichever way task update-spec answers, the outcome is consistent
    if u[0]["ok"] == true {
        assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    } else {
        assert_eq!(stop_reason(&doc), "spec_rewrite_needed", "{doc:#}");
        assert_eq!(doc["stop"]["evidence"]["error"], u[0]["error"]);
    }
    let file = PathBuf::from(s(&u[0]["result"]["file"]));
    let folded = fs::read_to_string(&file).unwrap();
    assert!(folded.contains("token seen:") && !folded.contains(&secret), "{folded}");
    for out in [
        exec(&f, &["pipeline", "show", &task, "--json"], &[]).stdout,
        exec(&f, &["pipeline", "show", &task], &[]).stdout,
        exec(&f, &["events", "--since", "0", "--task", &task, "--json"], &[]).stdout,
        exec(&f, &["status", &task, "--json"], &[]).stdout,
    ] {
        assert!(!out.contains(&secret));
    }
    for e in fs::read_dir(s(&doc["dir"])).unwrap().flatten() {
        assert!(!fs::read_to_string(e.path()).unwrap_or_default().contains(&secret), "{:?}", e.path());
    }
}

/// AC5: a fold that `task update-spec` refuses (spec lint's absolute size
/// limit) stops spec_rewrite_needed with that error; the spec is unchanged.
#[test]
fn a_fold_refused_by_update_spec_stops_with_its_error_and_leaves_the_spec() {
    let f = setup();
    // a spec close to the 40,960-byte limit, and a finding that pushes the
    // folded spec past it (an environment variable holds at most 32,767
    // characters on Windows, so the bulk is in the spec)
    let filler = "y".repeat(30_000);
    let spec = IMPORTANT_SPEC.replace("\"keep it short\"", &format!("\"keep it short\",\"{filler}\""));
    let task = create_with(&f, &spec);
    let approve = write_file(&f, "approve", APPROVE_DOC);
    ok(&f, &["spec", "review", "record", &task, "-f", &approve, "--json"], &[]);
    let constraints_before = ok(&f, &["review", "packet", &task, "--json"], &[])["spec"]["constraints"].clone();
    let note = "x".repeat(12_000);
    let seq = Seq::new(&f, &task, "hooks,review_reject");
    let mut env = seq.env();
    env.push(("FAKE_REVIEW_NOTE", note.as_str()));
    start(&f, &task, &[IMPL, IMPL], &[CLAUDE_REVIEW], &["--fold-on-reject"], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "spec_rewrite_needed", "{doc:#}\n{}", log_of(&doc));
    let u = steps(&doc, "update_spec");
    assert_eq!(u.len(), 1);
    assert_eq!(u[0]["ok"], false);
    let err = s(&u[0]["error"]);
    assert!(err.contains("spec lint"), "{err}");
    assert_eq!(doc["stop"]["evidence"]["error"], err.as_str());
    assert!(s(&doc["stop"]["summary"]).contains(&err), "{doc:#}");
    assert_eq!(ok(&f, &["review", "packet", &task, "--json"], &[])["spec"]["constraints"], constraints_before);
    assert!(of_type(&events(&f, &task), "spec_updated").is_empty());
    assert_eq!(steps(&doc, "dispatch").len(), 1, "no rework after a refused fold");
}

// --- AC7: implementer interruptions ----------------------------------------------------------

#[test]
fn quota_and_auth_stops_hand_over_to_the_next_entry_with_a_handoff() {
    let f = setup();
    // quota on claude-b: the next entry, claude-a, continues attempt 1
    let task = create(&f);
    let seq = Seq::new(&f, &task, "quota,hooks,review_pass");
    start(&f, &task, &[IMPL, "claude-a:sonnet:medium"], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2);
    let first = s(&d[0]["run_id"]);
    assert_eq!(run_row(&f, &task, &first)["interrupted_reason"], "quota");
    assert_eq!(d[1]["entry_index"], 2);
    assert_eq!(d[1]["result"]["attempt"], 1, "a hand-over keeps the attempt");
    assert_eq!(d[1]["result"]["handoff"]["path"], ".agent/handoff.md");
    assert_eq!(d[1]["result"]["handoff"]["previous_run"], first.as_str());
    let reason = flag_value(&argv(&f, &task, &d[1]), "--reason").unwrap();
    assert!(reason.contains(&format!("handoff after run {first} on claude-b stopped (quota)")), "{reason}");
    let wt = PathBuf::from(s(&run_row(&f, &task, &s(&d[1]["run_id"]))["worktree"]));
    let handoff = fs::read_to_string(wt.join(".agent").join("handoff.md")).unwrap_or_default();
    let run_dir = PathBuf::from(s(&run_row(&f, &task, &s(&d[1]["run_id"]))["run_dir"]));
    let kept = fs::read_to_string(run_dir.join("handoff.md")).unwrap();
    assert!(kept.contains(&first), "{kept}");
    assert!(handoff.is_empty() || handoff.contains(&first), "{handoff}");

    // auth on claude-b (that account alone is logged out): claude-a continues.
    // A fresh home: the quota stop above left claude-b rate_limited.
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "auth_expired,hooks,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_LOGOUT_IN_CONFIG", "1"));
    start(&f, &task, &[IMPL, "claude-a:sonnet:medium"], &[REVIEW], &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    let first = s(&d[0]["run_id"]);
    assert_eq!(run_row(&f, &task, &first)["interrupted_reason"], "auth");
    assert_eq!(d[1]["entry_index"], 2);
    assert_eq!(d[1]["result"]["attempt"], 1);
    assert_eq!(d[1]["result"]["handoff"]["path"], ".agent/handoff.md");
    let reason = flag_value(&argv(&f, &task, &d[1]), "--reason").unwrap();
    assert!(reason.contains(&format!("handoff after run {first} on claude-b stopped (auth)")), "{reason}");
}

#[test]
fn quota_on_every_entry_ends_implementers_exhausted_listing_each() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "quota");
    start(&f, &task, &[IMPL, "claude-a:sonnet:medium"], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "implementers_exhausted", "{doc:#}\n{}", log_of(&doc));
    let entries = doc["stop"]["evidence"]["entries"].as_array().unwrap().clone();
    let quota_stops: Vec<Value> = entries.iter().filter(|e| s(&e["outcome"]).contains("(quota)")).map(|e| e["entry_index"].clone()).collect();
    assert_eq!(quota_stops, vec![json!(1), json!(2)], "{entries:?}");
    let summary = s(&doc["stop"]["summary"]);
    assert!(summary.contains("entry 1 claude-b:sonnet:medium") && summary.contains("entry 2 claude-a:sonnet:medium"), "{summary}");
    assert_eq!(seq.launches().len(), 2, "nothing is waited for or retried");
    // the stop keeps the last run's diagnostic and next command next to the
    // entry history, in show and in the pipeline_stopped event alike
    let last = s(&steps(&doc, "dispatch")[1]["run_id"]);
    let row = run_row(&f, &task, &last);
    assert_eq!(doc["stop"]["evidence"]["run_id"], last.as_str());
    for key in ["interrupted_reason", "error_text", "next_step"] {
        assert_eq!(doc["stop"]["evidence"][key], row[key], "{key}");
    }
    assert!(summary.contains(&format!("run {last} was interrupted (quota)")), "{summary}");
    if let Some(next) = row["next_step"].as_str() {
        assert!(doc["stop"]["next"].as_array().unwrap().iter().any(|n| n == next), "{doc:#}");
    }
    let stopped = of_type(&events(&f, &task), "pipeline_stopped").pop().unwrap()["detail"].clone();
    assert_eq!(stopped["stop_evidence"], doc["stop"]["evidence"]);
    assert_eq!(stopped["stop_next"], doc["stop"]["next"]);
}

#[test]
fn a_crashed_run_is_resumed_once_then_handed_forward() {
    let f = setup();
    // one crash: resumed in its own session, then it finishes
    let task = create(&f);
    let seq = Seq::new(&f, &task, "crash,hooks,review_pass");
    start(&f, &task, &[IMPL, "claude-a:sonnet:medium"], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 1);
    let run = s(&d[0]["run_id"]);
    let r = steps(&doc, "resume");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["run_id"], run.as_str());
    assert_eq!(r[0]["entry_index"], 1);
    assert_eq!(r[0]["attempt"], 1);
    let why = s(&r[0]["reason"]);
    assert!(why.contains(&format!("resume of run {run}")) && why.contains("(crash)") && why.contains(REASON), "{why}");
    assert_eq!(argv(&f, &task, &r[0]), vec!["resume".to_string(), run.clone(), "--json".to_string()], "resume takes no --reason");
    assert_eq!(step_event(&f, &task, &r[0]["index"])["reason"], why.as_str());
    let launches = seq.launches();
    assert!(launches[0].starts_with("fresh ") && launches[1].starts_with("resume "), "{launches:?}");
    assert_eq!(launches[0][6..], launches[1][7..], "the same session was continued");

    // crashing again: the attempt's one resume is used, the next entry goes
    let task = create(&f);
    let seq = Seq::new(&f, &task, "crash,crash,hooks,review_pass");
    start(&f, &task, &[IMPL, "claude-a:sonnet:medium"], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2);
    assert_eq!(steps(&doc, "resume").len(), 1);
    assert_eq!(d[1]["entry_index"], 2);
    assert_eq!(d[1]["result"]["attempt"], 1);
    let reason = flag_value(&argv(&f, &task, &d[1]), "--reason").unwrap();
    assert!(reason.contains("again") && reason.contains("(crash)"), "{reason}");
}

/// AC7: an interrupted run without a session (agy's eligibility failure
/// before `init`, retried twice by the runner) is refused by resume, and the
/// next entry is dispatched instead.
#[test]
fn a_run_whose_resume_is_refused_is_handed_forward() {
    let f = setup();
    add_profile(&f, "agy-a", "google", "antigravity");
    let data = tempfile::tempdir().unwrap();
    let data_s = data.path().display().to_string();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "eligibility,eligibility,eligibility,hooks,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_AGY_DATA_DIR", data_s.as_str()));
    start(&f, &task, &["agy-a:gemini-3.8-flash-low", IMPL], &[REVIEW], &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    let row = run_row(&f, &task, &s(&d[0]["run_id"]));
    assert_eq!(row["interrupted_reason"], "transient", "{row}");
    let r = steps(&doc, "resume");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["ok"], false);
    assert!(s(&r[0]["error"]).contains("harness_session_id"), "{}", r[0]);
    assert_eq!(d[1]["entry_index"], 2);
    let reason = flag_value(&argv(&f, &task, &d[1]), "--reason").unwrap();
    assert!(reason.contains("resume was refused"), "{reason}");
}

#[test]
fn a_timeout_is_retried_once_at_twice_the_limit_and_a_second_stops_run_failed() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "slow,hooks,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW], &["--timeout", "3s"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2);
    let first = run_row(&f, &task, &s(&d[0]["run_id"]));
    assert_eq!((first["status"].clone(), first["interrupted_reason"].clone()), (json!("failed"), json!("timeout")));
    assert_eq!(first["timeout_seconds"], 3);
    assert_eq!(flag_value(&argv(&f, &task, &d[0]), "--timeout").as_deref(), Some("3s"));
    assert_eq!(flag_value(&argv(&f, &task, &d[1]), "--timeout").as_deref(), Some("6s"));
    assert_eq!(run_row(&f, &task, &s(&d[1]["run_id"]))["timeout_seconds"], 6);
    assert_eq!(d[1]["entry_index"], 1, "the same entry, still usable");
    assert_eq!(d[1]["result"]["attempt"], 1);

    let task = create(&f);
    let seq = Seq::new(&f, &task, "slow");
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &["--timeout", "3s"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "run_failed", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2, "one retry only");
    assert_eq!(doc["stop"]["evidence"]["run_id"], d[1]["run_id"]);
    assert_eq!(doc["stop"]["evidence"]["interrupted_reason"], "timeout");
}

// --- AC8: reviewers ---------------------------------------------------------------------

#[test]
fn reviewers_with_the_implementers_model_and_requested_or_lowered_effort_are_skipped() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    start(
        &f,
        &task,
        &["claude-b:claude-opus-4-6:xhigh"],
        &["claude-a:claude-opus-4-6:xhigh", "claude-a:claude-opus-4-6:high", REVIEW],
        &[],
        &seq.env(),
    );
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let run = s(&steps(&doc, "dispatch")[0]["run_id"]);
    let row = run_row(&f, &task, &run);
    assert_eq!((row["effort_requested"].clone(), row["effort"].clone()), (json!("xhigh"), json!("high")));
    let r = steps(&doc, "review_start");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["entry_index"], 3);
    assert_eq!(skipped_profiles(&r[0]), vec!["claude-a", "claude-a"]);
    for k in r[0]["skipped"].as_array().unwrap() {
        let why = s(&k["reason"]);
        assert!(why.contains(&run) && why.contains("same model and effort"), "{why}");
    }
    assert_eq!(doc["reviewer_index"], 2);
    let ev = step_event(&f, &task, &r[0]["index"]);
    assert_eq!(ev["skipped"], r[0]["skipped"]);
    assert_eq!(ev["reviewer_index"], 2);
}

#[test]
fn a_reviewer_stopped_by_quota_or_auth_hands_over_across_providers() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,quota,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW, "claude-a:opus:high"], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let r = steps(&doc, "review_start");
    assert_eq!(r.len(), 2);
    assert_eq!((r[0]["entry_index"].clone(), r[1]["entry_index"].clone()), (json!(1), json!(2)));
    let first = s(&r[0]["run_id"]);
    assert_eq!(run_row(&f, &task, &first)["interrupted_reason"], "quota");
    let reason = flag_value(&argv(&f, &task, &r[1]), "--reason").unwrap();
    assert!(reason.contains(&format!("review after reviewer run {first} on codex-a stopped (quota)")), "{reason}");
    assert!(reason.contains("reviewer entry 2 of 2") && reason.contains(REASON), "{reason}");
    assert_eq!(flag_value(&argv(&f, &task, &r[1]), "--profile").as_deref(), Some("claude-a"));
    assert_eq!(doc["reviewer_index"], 1);
    let review = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(review["reviewer_run_id"], r[1]["run_id"]);

    // auth: the stand-in claude-a placed first is logged out mid-review; the
    // codex entry after it reviews. A fresh home: the quota stop above left
    // codex-a rate_limited.
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,auth_expired,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_LOGOUT_IN_CONFIG", "1"));
    start(&f, &task, &[IMPL], &["claude-a:opus:high", REVIEW], &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let r = steps(&doc, "review_start");
    assert_eq!(r.len(), 2);
    assert_eq!(run_row(&f, &task, &s(&r[0]["run_id"]))["interrupted_reason"], "auth", "{doc:#}");
    assert_eq!(r[1]["entry_index"], 2);
    assert_eq!(flag_value(&argv(&f, &task, &r[1]), "--profile").as_deref(), Some("codex-a"));
}

#[test]
fn a_crashed_or_timed_out_review_is_retried_once_then_moves_on() {
    let f = setup();
    // crash once: the same entry again
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,crash,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW, "claude-a:opus:high"], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let r = steps(&doc, "review_start");
    assert_eq!(r.iter().map(|x| x["entry_index"].clone()).collect::<Vec<_>>(), vec![json!(1), json!(1)]);
    assert_eq!(run_row(&f, &task, &s(&r[0]["run_id"]))["interrupted_reason"], "crash");
    // preflight ran again before the retry
    assert_eq!(steps(&doc, "recommend").iter().filter(|x| x["result"]["role"] == "reviewer").count(), 2);

    // crash twice: on to entry 2
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,crash,crash,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW, "claude-a:opus:high"], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let r = steps(&doc, "review_start");
    assert_eq!(r.iter().map(|x| x["entry_index"].clone()).collect::<Vec<_>>(), vec![json!(1), json!(1), json!(2)]);

    // a timeout: retried once at twice that run's limit, then on to entry 2
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,slow,slow,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW, "claude-a:opus:high"], &["--review-timeout", "3s"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let r = steps(&doc, "review_start");
    assert_eq!(r.iter().map(|x| x["entry_index"].clone()).collect::<Vec<_>>(), vec![json!(1), json!(1), json!(2)]);
    let timeouts: Vec<Option<String>> = r.iter().map(|x| flag_value(&argv(&f, &task, x), "--timeout")).collect();
    assert_eq!(timeouts, vec![Some("3s".into()), Some("6s".into()), Some("3s".into())]);
    let row = run_row(&f, &task, &s(&r[0]["run_id"]));
    assert_eq!((row["status"].clone(), row["interrupted_reason"].clone()), (json!("failed"), json!("timeout")));
}

#[test]
fn reviewers_exhausted_stops_at_once_listing_every_entry() {
    let f = setup();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,quota");
    start(&f, &task, &[IMPL], &[REVIEW, "claude-b:sonnet:medium"], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "reviewers_exhausted", "{doc:#}\n{}", log_of(&doc));
    let entries = doc["stop"]["evidence"]["entries"].as_array().unwrap().clone();
    assert!(entries.iter().any(|e| e["entry_index"] == 1 && s(&e["outcome"]).contains("(quota)")), "{entries:?}");
    assert!(entries.iter().any(|e| e["entry_index"] == 2 && s(&e["outcome"]).contains("same model and effort")), "{entries:?}");
    assert_eq!(steps(&doc, "review_start").len(), 1);
    assert_eq!(seq.launches().len(), 2);
    assert_eq!(status(&f, &task)["status"], "in_review");
    // the stop still names the reviewer run that stopped and why
    let reviewer = s(&steps(&doc, "review_start")[0]["run_id"]);
    let row = run_row(&f, &task, &reviewer);
    assert_eq!(doc["stop"]["evidence"]["reviewer_run_id"], reviewer.as_str());
    for key in ["interrupted_reason", "error_text"] {
        assert_eq!(doc["stop"]["evidence"][key], row[key], "{key}");
    }
    let stopped = of_type(&events(&f, &task), "pipeline_stopped").pop().unwrap()["detail"].clone();
    assert_eq!(stopped["stop_evidence"], doc["stop"]["evidence"]);
}

// --- AC2/AC3: the preflight ---------------------------------------------------------------

/// Exact threshold, recommend's 95% exhaustion, an old reading that has not
/// reset (all skipped), then an account whose general bucket is low while
/// its codex extra bucket is full (launched: extra buckets never count).
#[test]
fn the_quota_threshold_skips_at_or_above_and_ignores_extra_buckets() {
    let f = setup();
    for (id, observed, used, extra) in [
        ("codex-x", "2026-11-01T00:00:00Z", 80.0, None),
        ("codex-e", "2026-11-01T00:00:00Z", 95.0, None),
        ("codex-o", "2026-01-01T00:00:00Z", 90.0, None),
        ("codex-b", "2026-11-01T00:00:00Z", 10.0, Some(99.0)),
    ] {
        let cfg = add_profile(&f, id, "openai", "codex");
        seed_codex(&cfg, observed, used, FUTURE, extra);
    }
    let task = create(&f);
    let seq = Seq::new(&f, &task, "ok,review_pass");
    let entries = ["codex-x:gpt-5.6-luna:medium", "codex-e:gpt-5.6-luna:medium", "codex-o:gpt-5.6-luna:medium", "codex-b:gpt-5.6-luna:medium"];
    start(&f, &task, &entries, &["claude-a:opus:medium"], &["--quota-threshold", "80"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    assert_eq!(doc["settings"]["quota_threshold"], 80);
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["entry_index"], 4);
    assert_eq!(skipped_profiles(&d[0]), vec!["codex-x", "codex-e", "codex-o"]);
    // exact threshold: held back, naming window, percent and threshold
    let x = preflight(&doc, "profile_usage", "implementer", 1);
    assert_eq!(x["run_id"], Value::Null);
    assert_eq!(x["result"]["window"]["name"], "five_hour");
    assert_eq!(x["result"]["window"]["used_percent"], 80.0);
    assert_eq!(x["result"]["held_back"], true);
    assert_eq!(x["result"]["threshold"], 80);
    let why = s(&d[0]["skipped"][0]["reason"]);
    assert!(why.contains("quota-threshold 80") && why.contains("five_hour") && why.contains("80%"), "{why}");
    // 95%: recommend's own eligibility, its reasons copied as they are
    let e = preflight(&doc, "recommend", "implementer", 2);
    assert_eq!(e["result"]["eligible"], false);
    assert_eq!(e["result"]["quota_state"], "exhausted");
    let reasons: Vec<String> = e["result"]["reasons"].as_array().unwrap().iter().map(s).collect();
    assert!(s(&d[0]["skipped"][1]["reason"]).contains(&reasons.join("; ")), "{doc:#}");
    // independently, recommend itself says the same
    let rec = ok(&f, &["recommend", "--role", "implementer", "--task", &task, "--model", "gpt-5.6-luna", "--json"], &[]);
    let cand = rec["candidates"].as_array().unwrap().iter().find(|c| c["profile_id"] == "codex-e").unwrap().clone();
    assert_eq!(cand["reasons"], e["result"]["reasons"]);
    // an old reading that has not reset still counts
    let o = preflight(&doc, "profile_usage", "implementer", 3);
    assert_eq!(o["result"]["window"]["used_percent"], 90.0);
    assert_eq!(o["result"]["held_back"], true);
    // the extra bucket's 99% is left out
    let b = preflight(&doc, "profile_usage", "implementer", 4);
    assert_eq!(b["result"]["window"]["used_percent"], 10.0);
    assert_eq!(b["result"]["held_back"], false);
    let usage = ok(&f, &["profile", "usage", "codex-b", "--json"], &[]);
    assert!(!usage["profiles"][0]["extra_buckets"].as_array().unwrap().is_empty(), "{usage}");
    // the recommend step's argv is the documented call
    let rec_ev = step_event(&f, &task, &preflight(&doc, "recommend", "implementer", 4)["index"]);
    let a: Vec<String> = rec_ev["argv"].as_array().unwrap().iter().map(s).collect();
    assert_eq!(a, ["recommend", "--role", "implementer", "--task", &task, "--model", "gpt-5.6-luna", "--json"]);
    let usage_ev = step_event(&f, &task, &b["index"]);
    let a: Vec<String> = usage_ev["argv"].as_array().unwrap().iter().map(s).collect();
    assert_eq!(a, ["profile", "usage", "codex-b", "--json"], "no --refresh");
}

/// Below the threshold, an already-reset window and an absent reading are
/// all launched; the unknown one with a warning, never as zero usage.
#[test]
fn below_threshold_reset_and_unknown_quota_are_launched() {
    let f = setup();
    let cfg = add_profile(&f, "codex-y", "openai", "codex");
    seed_codex(&cfg, "2026-11-01T00:00:00Z", 79.0, FUTURE, None);
    let cfg = add_profile(&f, "codex-r", "openai", "codex");
    seed_codex(&cfg, "2026-11-01T00:00:00Z", 99.0, PAST, None);
    add_profile(&f, "codex-n", "openai", "codex");
    // below: implementer codex-y; already reset: reviewer codex-r
    let task = create(&f);
    let seq = Seq::new(&f, &task, "ok,review_pass");
    start(&f, &task, &["codex-y:gpt-5.6-luna:medium"], &["codex-r:gpt-5.6-luna:high"], &["--quota-threshold", "80"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let y = preflight(&doc, "profile_usage", "implementer", 1);
    assert_eq!((y["result"]["window"]["used_percent"].clone(), y["result"]["held_back"].clone()), (json!(79.0), json!(false)));
    let r = preflight(&doc, "profile_usage", "reviewer", 1);
    assert_ne!(r["result"]["window"]["name"], "five_hour", "the reset window is ignored: {r}");
    assert_eq!(r["result"]["held_back"], false);
    assert_eq!(steps(&doc, "review_start")[0]["entry_index"], 1);

    // absent reading: launched, with a warning that says so
    let task = create(&f);
    let seq = Seq::new(&f, &task, "ok,review_pass");
    start(&f, &task, &["codex-n:gpt-5.6-luna:medium"], &["claude-a:opus:medium"], &["--quota-threshold", "80"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let n = preflight(&doc, "profile_usage", "implementer", 1);
    assert_eq!(n["result"]["window"], Value::Null);
    assert_eq!(n["result"]["held_back"], Value::Null);
    let w = s(&n["warning"]);
    assert!(w.contains("unknown") && w.contains("not held back"), "{w}");
    assert_eq!(steps(&doc, "dispatch")[0]["entry_index"], 1);
}

/// AC3: a preflight command that exits non-zero or prints no JSON is
/// recorded with its own error and ok false, warns, and holds nothing back.
#[test]
fn a_failed_preflight_is_recorded_and_the_launch_goes_ahead() {
    let f = setup();
    for (seam, kind) in [("recommend_exit", "recommend"), ("recommend_text", "recommend"), ("usage_exit", "profile_usage"), ("usage_text", "profile_usage")] {
        let task = create(&f);
        let seq = Seq::new(&f, &task, "hooks,review_pass");
        let mut env = seq.env();
        env.push(("THOTH_TEST_PIPELINE_PREFLIGHT", seam));
        start(&f, &task, &[IMPL], &[REVIEW], &["--quota-threshold", "50"], &env);
        let doc = wait_stopped(&f, &task);
        assert_eq!(stop_reason(&doc), "pass", "{seam}: {doc:#}\n{}", log_of(&doc));
        let st = preflight(&doc, kind, "implementer", 1);
        assert_eq!(st["ok"], false, "{seam}: {st}");
        let ev = step_event(&f, &task, &st["index"]);
        let a: Vec<String> = ev["argv"].as_array().unwrap().iter().map(s).collect();
        if seam.ends_with("_exit") {
            assert_ne!(st["exit_code"], 0, "{seam}");
            assert!(a.contains(&"--no-such-flag".to_string()), "{a:?}");
            assert!(st["error"].is_string(), "{seam}: {st}");
        } else {
            assert_eq!(st["exit_code"], 0, "{seam}");
            assert!(!a.contains(&"--json".to_string()), "{a:?}");
            assert!(st["error"].is_string(), "{seam}: {st}");
        }
        assert!(s(&st["warning"]).contains("not held back"), "{seam}: {st}");
        assert_eq!(steps(&doc, "dispatch")[0]["entry_index"], 1, "{seam}");
    }
}

/// SMALL-9a's reserve is reused, not re-implemented: an entry recommend marks
/// reserve_blocked is skipped with recommend's own reason.
#[test]
fn a_reserve_blocked_entry_is_skipped_with_recommends_reason() {
    let f = setup();
    let cfg = add_profile(&f, "codex-p", "openai", "codex");
    seed_codex(&cfg, "2026-11-01T00:00:00Z", 92.0, FUTURE, None);
    ok(&f, &["config", "set", "quota-reserve-percent", "10", "--json"], &[]);
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    start(&f, &task, &["codex-p:gpt-5.6-luna:medium", IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let p = preflight(&doc, "recommend", "implementer", 1);
    assert_eq!(p["result"]["reserve_blocked"], true);
    assert_eq!(p["result"]["eligible"], false);
    assert_eq!(p["result"]["quota_reserve_percent"], 10);
    let d = steps(&doc, "dispatch");
    assert_eq!(d[0]["entry_index"], 2);
    let why = s(&d[0]["skipped"][0]["reason"]);
    assert!(why.contains("quota-reserve-percent 10"), "{why}");
    assert!(steps(&doc, "profile_usage").is_empty(), "no threshold, no profile usage call");
}

/// SMALL-9a's launch refusal also rotates (review of attempt 1): with
/// recommend unreadable, the preflight lets the reserve-held codex-p through,
/// and then `dispatch` and `review start` each refuse it with
/// quota-reserve-percent; the pipeline consumes that entry and launches the
/// next one instead of stopping or waiting.
#[test]
fn a_dispatch_or_review_start_refused_by_the_reserve_moves_to_the_next_entry() {
    let f = setup();
    let cfg = add_profile(&f, "codex-p", "openai", "codex");
    seed_codex(&cfg, "2026-11-01T00:00:00Z", 92.0, FUTURE, None);
    ok(&f, &["config", "set", "quota-reserve-percent", "10", "--json"], &[]);
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    let mut env = seq.env();
    env.push(("THOTH_TEST_PIPELINE_PREFLIGHT", "recommend_exit"));
    let held = "codex-p:gpt-5.6-luna:medium";
    start(&f, &task, &[held, IMPL], &[held, CLAUDE_REVIEW], &[], &env);
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    for (kind, role) in [("dispatch", "implementer"), ("review_start", "reviewer")] {
        // the preflight could not be read, so it held nothing back
        assert_eq!(preflight(&doc, "recommend", role, 1)["ok"], false, "{kind}");
        let l = steps(&doc, kind);
        assert_eq!(l.len(), 2, "{kind}: {doc:#}");
        assert_eq!((l[0]["entry_index"].clone(), l[0]["ok"].clone(), l[0]["run_id"].clone()), (json!(1), json!(false), Value::Null), "{kind}");
        assert_eq!(flag_value(&argv(&f, &task, &l[0]), "--profile").as_deref(), Some("codex-p"));
        let err = s(&l[0]["error"]);
        assert!(err.contains("refusing to launch on profile codex-p") && err.contains("quota-reserve-percent 10"), "{kind}: {err}");
        assert_eq!((l[1]["entry_index"].clone(), l[1]["ok"].clone()), (json!(2), json!(true)), "{kind}");
        assert_eq!(skipped_profiles(&l[1]), vec!["codex-p"], "{kind}");
        let why = s(&l[1]["skipped"][0]["reason"]);
        assert!(why.contains("refused") && why.contains(&err), "{kind}: {why}");
        assert_eq!(step_event(&f, &task, &l[1]["index"])["skipped"], l[1]["skipped"]);
    }
    assert_eq!(doc["implementer_index"], 1);
    assert_eq!(doc["reviewer_index"], 1);
    assert_eq!(seq.launches().len(), 2, "codex-p never launched");
}

/// AC7: a resume that SMALL-9a's reserve refuses rotates too (review of
/// attempt 1). codex-q is 92% used with the reserve off, so its dispatch goes
/// ahead; while its worker is held, the reserve is set to 10; the worker
/// crashes, the preflight (recommend unreadable) lets the resume through,
/// `thoth resume` refuses it with quota-reserve-percent, and the next entry
/// is dispatched in the same attempt.
#[test]
fn a_resume_refused_by_the_reserve_hands_the_attempt_to_the_next_entry() {
    let f = setup();
    let cfg = add_profile(&f, "codex-q", "openai", "codex");
    seed_codex(&cfg, "2026-11-01T00:00:00Z", 92.0, FUTURE, None);
    let task = create(&f);
    let gate = f.home_path().join("crash-gate");
    let gate_s = gate.display().to_string();
    let seq = Seq::new(&f, &task, "crash,hooks,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_CRASH_GATE", gate_s.as_str()));
    env.push(("THOTH_TEST_PIPELINE_PREFLIGHT", "recommend_exit"));
    start(&f, &task, &["codex-q:gpt-5.6-luna:medium", IMPL], &[CLAUDE_REVIEW], &[], &env);
    wait_show(&f, &task, "the codex dispatch", |v| v["step"] == "wait_run");
    ok(&f, &["config", "set", "quota-reserve-percent", "10", "--json"], &[]);
    fs::write(&gate, "go").unwrap();
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 2, "{doc:#}");
    let crashed = s(&d[0]["run_id"]);
    let row = run_row(&f, &task, &crashed);
    assert_eq!((row["status"].clone(), row["interrupted_reason"].clone()), (json!("interrupted"), json!("crash")), "{row}");
    let r = steps(&doc, "resume");
    assert_eq!(r.len(), 1, "{doc:#}");
    assert_eq!((r[0]["ok"].clone(), r[0]["entry_index"].clone(), r[0]["attempt"].clone()), (json!(false), json!(1), json!(1)));
    assert_eq!(argv(&f, &task, &r[0]), vec!["resume".to_string(), crashed.clone(), "--json".to_string()]);
    let err = s(&r[0]["error"]);
    assert!(err.contains("refusing to launch on profile codex-q") && err.contains("quota-reserve-percent 10"), "{err}");
    assert_eq!((d[1]["entry_index"].clone(), d[1]["result"]["attempt"].clone()), (json!(2), json!(1)));
    let reason = flag_value(&argv(&f, &task, &d[1]), "--reason").unwrap();
    assert!(reason.contains(&format!("after run {crashed} on codex-q was interrupted (crash); resume was refused")), "{reason}");
    assert!(reason.contains("quota-reserve-percent 10"), "{reason}");
    // codex-q ran once and was never resumed
    let launches = seq.launches();
    assert_eq!(launches.len(), 3, "{launches:?}");
    assert!(launches.iter().all(|l| l.starts_with("fresh ")), "{launches:?}");
}

/// An agy-a profile whose stored reading (through fake agy's usage answer)
/// has two groups: Gemini 97% used, Claude and GPT 10% used. Returns the fake
/// agy data folder, which its workers need too.
fn seed_agy_groups(f: &Fixture) -> tempfile::TempDir {
    add_profile(f, "agy-a", "google", "antigravity");
    let answer = json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.03, "reset_time": "2099-01-01T00:00:00Z"}]},
        {"name": "Claude and GPT models", "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS", "buckets": [
            {"id": "3p-weekly", "window": "weekly", "remaining_fraction": 0.9, "reset_time": "2099-01-01T00:00:00Z"}]}
    ]}}});
    let file = write_file(f, "agy-usage", &answer.to_string());
    let data = tempfile::tempdir().unwrap();
    let data_s = data.path().display().to_string();
    ok(f, &["config", "set", "live-quota", "on", "--json"], &[]);
    ok(f, &["profile", "usage", "agy-a", "--refresh", "--json"], &[("FAKE_AGY_USAGE_FILE", file.as_str()), ("FAKE_AGY_DATA_DIR", data_s.as_str())]);
    ok(f, &["config", "set", "live-quota", "off", "--json"], &[]);
    data
}

/// AC2: an agy account's quota is decided by the model's group only.
#[test]
fn grouped_quota_is_decided_by_the_models_group() {
    let f = setup();
    let data = seed_agy_groups(&f);
    let data_s = data.path().display().to_string();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "ok,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_AGY_DATA_DIR", data_s.as_str()));
    start(
        &f,
        &task,
        &["agy-a:gemini-3.8-flash-low", "agy-a:claude-sonnet-4-6:medium"],
        &[REVIEW],
        &["--quota-threshold", "90"],
        &env,
    );
    let doc = wait_show(&f, &task, "the agy dispatch", |v| !steps(v, "dispatch").is_empty());
    let g = preflight(&doc, "profile_usage", "implementer", 1);
    assert_eq!(g["result"]["quota_group"], "Gemini Models", "{g}");
    assert_eq!(g["result"]["quota_group_known"], true);
    assert_eq!(g["result"]["window"]["group"], "Gemini Models");
    assert_eq!(g["result"]["held_back"], true);
    let c = preflight(&doc, "profile_usage", "implementer", 2);
    assert_eq!(c["result"]["quota_group"], "Claude and GPT models", "{c}");
    assert_eq!(c["result"]["window"]["name"], "3p-weekly");
    assert_eq!(c["result"]["held_back"], false);
    let d = steps(&doc, "dispatch");
    assert_eq!(d[0]["entry_index"], 2);
    assert_eq!(skipped_profiles(&d[0]), vec!["agy-a"]);
    // what the fake agy worker then delivers is not this test's business
    let done = wait_stopped(&f, &task);
    assert_ne!(stop_reason(&done), "error", "{done:#}");
}

/// AC2/AC3 (review of attempt 1): when recommend cannot be read, the entry's
/// quota group is unknown, which is not the explicit null of an ungrouped
/// account. The threshold then has no deciding window, so the Gemini group's
/// 97% never holds back the Claude-model entry; the step warns instead, and
/// the launch goes ahead.
#[test]
fn an_unknown_quota_group_never_lets_another_groups_window_hold_an_entry_back() {
    let f = setup();
    let data = seed_agy_groups(&f);
    let data_s = data.path().display().to_string();
    // the stored reading does hold a 97% window, in the Gemini group
    let usage = ok(&f, &["profile", "usage", "agy-a", "--json"], &[]);
    let windows = usage["profiles"][0]["windows"].as_array().unwrap().clone();
    assert!(windows.iter().any(|w| w["group"] == "Gemini Models" && w["used_percent"].as_f64().unwrap() >= 90.0), "{usage:#}");
    let task = create(&f);
    let seq = Seq::new(&f, &task, "ok,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_AGY_DATA_DIR", data_s.as_str()));
    env.push(("THOTH_TEST_PIPELINE_PREFLIGHT", "recommend_exit"));
    start(&f, &task, &["agy-a:claude-sonnet-4-6:medium"], &[REVIEW], &["--quota-threshold", "90"], &env);
    let doc = wait_show(&f, &task, "the agy dispatch", |v| !steps(v, "dispatch").is_empty());
    let rec = preflight(&doc, "recommend", "implementer", 1);
    assert_eq!(rec["ok"], false, "{rec}");
    assert_eq!(rec["result"]["quota_group"], Value::Null);
    let u = preflight(&doc, "profile_usage", "implementer", 1);
    assert_eq!(u["ok"], true, "{u}");
    assert_eq!(u["result"]["quota_group"], Value::Null);
    assert_eq!(u["result"]["quota_group_known"], false, "{u}");
    assert_eq!(u["result"]["window"], Value::Null, "no window decides: {u}");
    assert_eq!(u["result"]["held_back"], Value::Null, "{u}");
    let w = s(&u["warning"]);
    assert!(w.contains("quota_group") && w.contains("agy-a") && w.contains("not held back"), "{w}");
    assert_eq!(step_event(&f, &task, &u["index"])["warning"], u["warning"]);
    let d = steps(&doc, "dispatch");
    assert_eq!(d[0]["entry_index"], 1, "{doc:#}");
    assert_eq!(d[0]["skipped"], json!([]));
    let done = wait_stopped(&f, &task);
    assert_ne!(stop_reason(&done), "implementers_exhausted", "{done:#}");
    assert_ne!(stop_reason(&done), "error", "{done:#}");
}

/// AC3 (review of attempt 1): without a threshold, a readable eligible
/// candidate whose quota_state is unknown (no reading yet) is launched, and
/// its recommend step says the quota is unknown and nothing was held back.
#[test]
fn an_unknown_quota_state_is_a_warning_even_without_a_threshold() {
    let f = setup();
    let task = create(&f);
    let rec = ok(&f, &["recommend", "--role", "implementer", "--task", &task, "--model", "sonnet", "--json"], &[]);
    let cand = rec["candidates"].as_array().unwrap().iter().find(|c| c["profile_id"] == "claude-b").unwrap().clone();
    assert_eq!((cand["eligible"].clone(), cand["quota_state"].clone()), (json!(true), json!("unknown")), "{cand}");
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    start(&f, &task, &[IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let st = preflight(&doc, "recommend", "implementer", 1);
    assert_eq!(st["ok"], true);
    assert_eq!(st["result"]["eligible"], true);
    assert_eq!(st["result"]["quota_state"], "unknown");
    let w = s(&st["warning"]);
    assert!(w.contains("quota_state unknown") && w.contains("claude-b") && w.contains("not held back"), "{w}");
    assert_eq!(step_event(&f, &task, &st["index"])["warning"], st["warning"]);
    assert!(steps(&doc, "profile_usage").is_empty(), "no threshold, no profile usage call");
    assert_eq!(steps(&doc, "dispatch")[0]["entry_index"], 1);
    // the human show prints it under the step
    let human = exec(&f, &["pipeline", "show", &task], &[]).stdout;
    assert!(human.contains("quota_state unknown"), "{human}");
}

// --- AC1: entry states and options ---------------------------------------------------------

fn snapshot(f: &Fixture, task: &str) -> (Value, Value, i32) {
    let shown = exec(f, &["pipeline", "show", task, "--json"], &[]);
    (ok(f, &["events", "--since", "0", "--json"], &[])["events"].clone(), status(f, task)["status"].clone(), shown.code)
}

#[test]
fn invalid_thresholds_are_refused_unchanged_and_the_options_persist() {
    let f = setup();
    let task = create(&f);
    for bad in ["0", "101", "-5", "2.5", ""] {
        let before = snapshot(&f, &task);
        let args = start_args(&task, &[IMPL], &[REVIEW], &["--quota-threshold", bad]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = exec(&f, &args, &[]);
        assert_eq!(o.code, 1, "{bad:?}: {}", o.stdout);
        assert!(o.error().contains("--quota-threshold"), "{bad:?}: {}", o.stderr);
        assert_eq!(snapshot(&f, &task), before, "{bad:?} changed something");
    }
    let gate = f.home_path().join("gate-opts");
    let gate_s = gate.display().to_string();
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_WORKER_GATE", gate_s.as_str()));
    let started = start(&f, &task, &[IMPL, IMPL], &[REVIEW], &["--quota-threshold", "90", "--fold-on-reject"], &env);
    assert_eq!(started["settings"]["quota_threshold"], 90);
    assert_eq!(started["settings"]["fold_on_reject"], true);
    assert_eq!(started["warnings"], json!([]), "no only-the-first-entry warning");
    let shown = show(&f, &task);
    assert_eq!(shown["settings"], started["settings"]);
    let ev = of_type(&events(&f, &task), "pipeline_started");
    assert_eq!(ev[0]["detail"]["quota_threshold"], 90);
    assert_eq!(ev[0]["detail"]["fold_on_reject"], true);
    // a task that is running (no pipeline of its own needed) is refused
    wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
    wait_stopped(&f, &task);
    let before = snapshot(&f, &task);
    let args = start_args(&task, &[IMPL], &[REVIEW], &[]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = exec(&f, &args, &[]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o.error().contains("'running'"), "{}", o.stderr);
    assert_eq!(snapshot(&f, &task), before);
    fs::write(&gate, "go").unwrap();
    f.wait_for_task_status(&task, &["in_review"], 120);
    // the defaults: no threshold, no fold
    let t2 = create(&f);
    let seq = Seq::new(&f, &t2, "hooks,review_pass");
    let started = start(&f, &t2, &[IMPL], &[REVIEW], &[], &seq.env());
    assert_eq!(started["settings"]["quota_threshold"], Value::Null);
    assert_eq!(started["settings"]["fold_on_reject"], false);
    assert_eq!(stop_reason(&wait_stopped(&f, &t2)), "pass");
}

fn dispatch_now(f: &Fixture, task: &str, profile_model: (&str, &str), extra: &[&str], envs: &[(&str, &str)]) -> String {
    let mut args = vec!["dispatch", task, "--profile", profile_model.0, "--model", profile_model.1, "--effort", "medium", "--reason", REASON];
    args.extend_from_slice(extra);
    args.push("--json");
    let d = ok(f, &args, envs);
    let run = s(&d["run_id"]);
    ok(f, &["wait", "--run", &run, "--timeout", "120s", "--json"], &[]);
    run
}

#[test]
fn unverified_interrupted_and_failed_tasks_are_continued_from_the_first_entry() {
    let f = setup();
    // unverified by a refused delivery: the bounded reroute, from entry 1
    let task = create(&f);
    let seq = Seq::new(&f, &task, "bad_result,hooks,review_pass");
    let run = dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &seq.env());
    assert_eq!(status(&f, &task)["status"], "unverified");
    start(&f, &task, &["claude-a:sonnet:medium", IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["entry_index"], 1);
    assert_eq!(d[0]["result"]["attempt"], 1);
    assert!(flag_value(&argv(&f, &task, &d[0]), "--reason").unwrap().contains(&format!("unverified delivery of run {run}")));
    assert_eq!(of_type(&events(&f, &task), "pipeline_started")[0]["detail"]["entry_status"], "unverified");

    // unverified by an invalid review verdict: nothing is launched
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_bad,hooks");
    dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &seq.env());
    let r = ok(&f, &["review", "start", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--reason", REASON, "--json"], &seq.env());
    ok(&f, &["wait", "--run", &s(&r["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(status(&f, &task)["status"], "unverified");
    let launches = seq.launches().len();
    start(&f, &task, &[IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "review_unverified", "{doc:#}");
    assert_eq!(doc["steps"], json!([]), "no launch");
    assert_eq!(seq.launches().len(), launches);
    assert_eq!(status(&f, &task)["status"], "unverified");

    // failed by its timeout: one dispatch at twice that run's limit
    let task = create(&f);
    let seq = Seq::new(&f, &task, "slow,hooks,review_pass");
    dispatch_now(&f, &task, ("claude-b", "sonnet"), &["--timeout", "3s"], &seq.env());
    assert_eq!(status(&f, &task)["status"], "failed");
    start(&f, &task, &[IMPL], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    let d = steps(&doc, "dispatch");
    assert_eq!(flag_value(&argv(&f, &task, &d[0]), "--timeout").as_deref(), Some("6s"));

    // interrupted by quota: a fresh dispatch (a handoff), not a resume. Last,
    // because the quota stop leaves claude-b rate_limited.
    let task = create(&f);
    let seq = Seq::new(&f, &task, "quota,hooks,review_pass");
    let run = dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &seq.env());
    assert_eq!(status(&f, &task)["status"], "interrupted");
    start(&f, &task, &["claude-a:sonnet:medium"], &[REVIEW], &[], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "pass", "{doc:#}\n{}", log_of(&doc));
    assert!(steps(&doc, "resume").is_empty());
    let d = steps(&doc, "dispatch");
    assert_eq!(d[0]["entry_index"], 1);
    assert_eq!(d[0]["result"]["handoff"]["previous_run"], run.as_str());
}

// --- AC4: a pending fix ------------------------------------------------------------------

#[test]
fn a_pending_fix_is_never_folded_or_redispatched() {
    let f = setup();
    let repo = f.repo_path();
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/greet.rs"), &lines).unwrap();
    common::git(&repo, &["add", "--", "src/greet.rs"]);
    common::git(&repo, &["commit", "-q", "-m", "greet"]);
    let task = create(&f);
    dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &[("FAKE_TASK_ID", task.as_str()), ("FAKE_CLAUDE_SCENARIO", "ok")]);
    let code = s(&ok(&f, &["evidence", "list", &task, "--json"], &[])["evidence"][0]["code"]);
    let plan = write_file(&f, "plan", &json!({"document": {
        "verdict": "REJECT",
        "findings": {
            "criteria": [{"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"}],
            "extra_findings": [],
        },
        "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}],
    }}).to_string());
    let edit = ok(
        &f,
        &["review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--edit", "--reason", REASON, "--json"],
        &[("FAKE_TASK_ID", task.as_str()), ("FAKE_CLAUDE_SCENARIO", "review_fix"), ("FAKE_FIX_PLAN", plan.as_str())],
    );
    ok(&f, &["wait", "--run", &s(&edit["run_id"]), "--timeout", "120s", "--json"], &[]);
    let before = ok(&f, &["review", "show", &task, "--json"], &[]);
    assert_eq!(before["pending_fix"], true, "{before:#}");
    let spec_before = ok(&f, &["review", "packet", &task, "--json"], &[])["spec"].clone();
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    start(&f, &task, &[IMPL, IMPL], &[REVIEW], &["--fold-on-reject"], &seq.env());
    let doc = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&doc), "needs_fix_confirmation", "{doc:#}");
    assert_eq!(doc["steps"], json!([]), "nothing folded, dispatched or reviewed");
    assert!(seq.launches().is_empty());
    assert_eq!(ok(&f, &["review", "show", &task, "--json"], &[]), before);
    assert_eq!(ok(&f, &["review", "packet", &task, "--json"], &[])["spec"], spec_before);
    assert!(of_type(&events(&f, &task), "spec_updated").is_empty());
}

// --- AC10: the shared fake sequence --------------------------------------------------------

#[test]
fn the_fake_sequence_is_shared_across_providers_and_skips_login_probes() {
    let f = setup();
    add_profile(&f, "agy-a", "google", "antigravity");
    let data = tempfile::tempdir().unwrap();
    let data_s = data.path().display().to_string();
    let task = create(&f);
    let seq = Seq::new(&f, &task, "hooks,review_reject,ok");
    let mut env = seq.env();
    // all overridden by the sequence
    env.extend([("FAKE_SCENARIO", "crash"), ("FAKE_CODEX_SCENARIO", "crash"), ("FAKE_CLAUDE_SCENARIO", "crash"), ("FAKE_FIRST_SCENARIO", "crash")]);
    env.push(("FAKE_AGY_DATA_DIR", data_s.as_str()));
    // launch 1: claude implements (hooks)
    dispatch_now(&f, &task, ("claude-b", "sonnet"), &[], &env);
    assert_eq!(status(&f, &task)["status"], "in_review");
    // launch 2: codex reviews (review_reject)
    let r = ok(&f, &["review", "start", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--reason", REASON, "--json"], &env);
    ok(&f, &["wait", "--run", &s(&r["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(ok(&f, &["review", "show", &task, "--json"], &[])["verdict"], "REJECT");
    // launch 3: agy reworks (ok, the last entry)
    let d = ok(&f, &["dispatch", &task, "--profile", "agy-a", "--model", "gemini-3.8-flash-low", "--reason", REASON, "--json"], &env);
    ok(&f, &["wait", "--run", &s(&d["run_id"]), "--timeout", "120s", "--json"], &[]);
    assert_eq!(status(&f, &task)["status"], "in_review");
    let launches = seq.launches();
    assert_eq!(launches.len(), 3, "three worker launches; the login checks took no entry: {launches:?}");
    assert!(launches.iter().all(|l| l.starts_with("fresh ")), "{launches:?}");
}

// --- AC1/AC12: MCP and help -------------------------------------------------------------------

struct McpServer {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl McpServer {
    fn start(f: &Fixture, envs: &[(&str, &str)]) -> Self {
        use std::process::Stdio;
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp").arg("--default-repo").arg(f.repo_path()).arg("--max-wait").arg("5s").current_dir(f.repo_path());
        for name in std::env::vars_os().map(|(name, _)| name) {
            let upper = name.to_string_lossy().to_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
                c.env_remove(&name);
            }
        }
        for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME", "CLAUDE_PLUGIN_ROOT", "THOTH_PIPELINE_ID"] {
            c.env_remove(var);
        }
        c.env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .env("FAKE_CHECK_DIR", f.home_path().join("check-gates"));
        for (k, v) in envs {
            c.env(k, v);
        }
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn request(&mut self, v: Value) -> Value {
        use std::io::{BufRead, Write};
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}}))
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_start_takes_quota_threshold_and_fold_on_reject_with_their_types() {
    let f = setup();
    let task = create(&f);
    let gate = f.home_path().join("gate-mcp-rework");
    let gate_s = gate.display().to_string();
    let seq = Seq::new(&f, &task, "hooks,review_pass");
    let mut env = seq.env();
    env.push(("FAKE_WORKER_GATE", gate_s.as_str()));
    let mut server = McpServer::start(&f, &env);
    let list = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(serde_json::to_vec(tools).unwrap().len() <= 20_000);
    let tool = tools.iter().find(|t| t["name"] == "thoth_pipeline_start").unwrap();
    let props = &tool["inputSchema"]["properties"];
    assert_eq!(props["quota_threshold"], json!({"type": "integer"}));
    assert_eq!(props["fold_on_reject"], json!({"type": "boolean"}));
    let required: Vec<String> = tool["inputSchema"]["required"].as_array().unwrap().iter().map(s).collect();
    assert!(!required.contains(&"quota_threshold".to_string()) && !required.contains(&"fold_on_reject".to_string()));
    let desc = s(&tool["description"]);
    for word in ["reject", "unverified", "interrupted", "failed", "quota_threshold", "fold_on_reject"] {
        assert!(desc.contains(word), "{word}: {desc}");
    }
    assert!(!desc.contains("uses the first") && !desc.contains("reworks a REJECT"), "{desc}");
    // wrong types fail before any work
    let base = json!({"task": task, "implementers": [IMPL], "reviewers": [REVIEW], "reason": REASON});
    for (k, v) in [("quota_threshold", json!("50")), ("fold_on_reject", json!("yes")), ("quota_threshold", json!(101))] {
        let mut a = base.clone();
        a[k] = v;
        let r = server.call(2, "thoth_pipeline_start", a);
        assert_eq!(r["result"]["isError"], true, "{r}");
    }
    assert_eq!(exec(&f, &["pipeline", "show", &task, "--json"], &[]).code, 1, "nothing was started");
    let mut a = base.clone();
    a["quota_threshold"] = json!(50);
    a["fold_on_reject"] = json!(true);
    let r = server.call(3, "thoth_pipeline_start", a);
    assert_eq!(r["result"]["isError"], false, "{r}");
    let started = r["result"]["structuredContent"].clone();
    assert_eq!(started["settings"]["quota_threshold"], 50);
    assert_eq!(started["settings"]["fold_on_reject"], true);
    assert_eq!(started["warnings"], json!([]));
    wait_show(&f, &task, "wait_run", |v| v["step"] == "wait_run");
    let cli = show(&f, &task);
    assert_eq!(cli["settings"], started["settings"]);
    let r = server.call(4, "thoth_pipeline_show", json!({"task": task}));
    assert_eq!(r["result"]["structuredContent"]["settings"], cli["settings"]);
    assert_eq!(r["result"]["structuredContent"]["pipeline_id"], cli["pipeline_id"]);
    fs::write(&gate, "go").unwrap();
    let done = wait_stopped(&f, &task);
    assert_eq!(stop_reason(&done), "pass", "{done:#}");
    // fold_on_reject false is the default and adds no flag
    let t2 = create(&f);
    let mut a = base.clone();
    a["task"] = json!(t2);
    a["fold_on_reject"] = json!(false);
    let env2 = Seq::new(&f, &t2, "hooks,review_pass");
    drop(server);
    let mut server = McpServer::start(&f, &env2.env());
    let r = server.call(5, "thoth_pipeline_start", a);
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["settings"]["fold_on_reject"], false);
    assert_eq!(r["result"]["structuredContent"]["settings"]["quota_threshold"], Value::Null);
    drop(server);
    assert_eq!(stop_reason(&wait_stopped(&f, &t2)), "pass");
}

#[test]
fn start_help_names_the_entry_states_and_the_new_flags() {
    let f = setup();
    let help = String::from_utf8(cmd(&f).args(["pipeline", "start", "--help"]).output().unwrap().stdout).unwrap();
    for word in ["queued", "in_review", "reject", "unverified", "interrupted", "failed", "--quota-threshold", "--fold-on-reject"] {
        assert!(help.contains(word), "{word}: {help}");
    }
    assert!(!help.contains("This version uses the first"), "{help}");
    assert!(!help.contains("uses the first --implementer"), "{help}");
}
