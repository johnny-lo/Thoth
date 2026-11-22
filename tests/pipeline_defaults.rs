//! LIGHT-1: `thoth pipeline defaults set | show | clear` stores one default
//! implementer list and one default reviewer list per THOTH_HOME, and
//! `thoth pipeline start` on a low or normal risk code task uses a stored
//! list for a flag that is left out. Everything is observed through the CLI
//! (`--json` output, exit codes, `status`, `events`), the user-visible file
//! THOTH_HOME/pipeline-defaults.json and MCP tool calls; fake workers only
//! (fake-claude implements, fake-codex reviews), temporary homes and
//! repositories. No test opens a database.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const REASON: &str = "pipeline defaults fixture: fake workers in a temporary repo";
const IMPL_A: &str = "claude-a:sonnet:medium";
const IMPL_B: &str = "claude-b:sonnet:medium";
const REVIEW: &str = "codex-a:gpt-5.6-luna:medium";
const REVIEW_2: &str = "codex-a:gpt-5.6-luna:high";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");
const FILE: &str = "pipeline-defaults.json";

// --- running thoth ------------------------------------------------------------

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names only are read), and without the two benchmark
/// keys.
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        // GITENV-1: with GIT_CONFIG_KEY_n the count and values go too, so git never sees a count without its keys
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) || upper == "GIT_CONFIG_COUNT" || upper.starts_with("GIT_CONFIG_VALUE_") {
            c.env_remove(&name);
        }
    }
    c.env_remove("AISTUPIDLEVEL_API_KEY").env_remove("ARTIFICIALANALYSIS_API_KEY");
    c.env_remove("THOTH_PIPELINE_ID");
    c.env("FAKE_CHECK_DIR", check_dir(f));
    c
}

fn check_dir(f: &Fixture) -> PathBuf {
    let d = f.home_path().join("check-gates");
    std::fs::create_dir_all(&d).unwrap();
    d
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
    /// The `--json` error document's `error` (the last stderr line).
    fn error(&self) -> String {
        let last = self.stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        let v: Value = serde_json::from_str(last)
            .unwrap_or_else(|e| panic!("stderr's last line is not JSON ({e}): {}", self.stderr));
        assert_eq!(v["ok"], false, "{v}");
        v["error"].as_str().unwrap_or_else(|| panic!("no error string: {v}")).to_string()
    }
}

fn exec_in(f: &Fixture, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Out {
    let mut c = cmd(f);
    c.current_dir(dir);
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

fn exec(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Out {
    exec_in(f, &f.repo_path(), args, envs)
}

fn ok_in(f: &Fixture, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let o = exec_in(f, dir, args, envs);
    assert_eq!(o.code, 0, "thoth {args:?} exited {}\nstdout: {}\nstderr: {}", o.code, o.stdout, o.stderr);
    o.json()
}

fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    ok_in(f, &f.repo_path(), args, envs)
}

/// Runs `args` and asserts exit 1 with a `--json` error holding every needle.
fn refused(f: &Fixture, args: &[&str], envs: &[(&str, &str)], needles: &[&str]) -> String {
    let o = exec(f, args, envs);
    assert_eq!(o.code, 1, "{args:?} was not refused\nstdout: {}\nstderr: {}", o.stdout, o.stderr);
    let err = o.error();
    for n in needles {
        assert!(err.contains(n), "{args:?}: error lacks {n:?}: {err}");
    }
    err
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

// --- fixtures ---------------------------------------------------------------------

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let dir = cfg.display().to_string();
    ok(f, &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &dir, "--json"], &[]);
}

/// An initialised repository with the given Claude profiles, codex-a and one
/// approved green check.
fn setup_with(claude: &[&str]) -> Fixture {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    ok(&f, &["init", "--json"], &[]);
    for id in claude {
        add_profile(&f, id, "anthropic", "claude-code");
    }
    add_profile(&f, "codex-a", "openai", "codex");
    let check = format!("\"{FAKE_CHECK}\" exit 0");
    ok(&f, &["checks", "add", "green", "--cmd", &check, "--json"], &[]);
    ok(&f, &["checks", "approve", "--reason", "fixture checks", "--json"], &[]);
    f
}

fn setup() -> Fixture {
    setup_with(&["claude-a", "claude-b"])
}

fn spec_of(risk: &str) -> String {
    format!(
        r#"{{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"{risk}"}}"#
    )
}

/// A spec without a risk (normal).
const SPEC_NO_RISK: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[]}"#;

fn create_spec(f: &Fixture, spec: &str) -> String {
    let file = f.home_path().join(format!("spec-{}.json", rand_name()));
    std::fs::write(&file, spec).unwrap();
    let path = file.display().to_string();
    s(&ok(f, &["task", "create", "--title", "pipeline", "-f", &path, "--json"], &[])["task_id"])
}

fn create(f: &Fixture, risk: &str) -> String {
    create_spec(f, &spec_of(risk))
}

fn rand_name() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn worker_env(task: &str) -> Vec<(&str, &str)> {
    vec![("FAKE_TASK_ID", task), ("FAKE_CLAUDE_SCENARIO", "hooks"), ("FAKE_CODEX_SCENARIO", "review_pass")]
}

fn defaults_file(f: &Fixture) -> PathBuf {
    f.home_path().join(FILE)
}

/// `pipeline defaults set` with `flags`, a reason and --json; must exit 0.
fn set(f: &Fixture, flags: &[&str], reason: &str) -> Value {
    let mut args = vec!["pipeline", "defaults", "set"];
    args.extend_from_slice(flags);
    args.extend(["--reason", reason, "--json"]);
    let v = ok(f, &args, &[]);
    assert_eq!(v["ok"], true, "{v}");
    v
}

fn show_defaults(f: &Fixture) -> Value {
    ok(f, &["pipeline", "defaults", "show", "--json"], &[])
}

fn entry(text: &str) -> Value {
    let p: Vec<&str> = text.split(':').collect();
    json!({"profile": p[0], "model": p[1], "effort": p.get(2)})
}

fn entries(list: &[&str]) -> Value {
    Value::Array(list.iter().map(|e| entry(e)).collect())
}

/// `pipeline start TASK` + `flags` + `--reason REASON --json`.
fn start_args<'a>(task: &'a str, flags: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["pipeline", "start", task];
    a.extend_from_slice(flags);
    a.extend(["--reason", REASON, "--json"]);
    a
}

fn start(f: &Fixture, task: &str, flags: &[&str]) -> Value {
    let v = ok(f, &start_args(task, flags), &worker_env(task));
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["status"], "running", "{v}");
    v
}

fn log_of(doc: &Value) -> String {
    doc["dir"]
        .as_str()
        .and_then(|d| std::fs::read_to_string(Path::new(d).join("pipeline.log")).ok())
        .unwrap_or_default()
}

fn wait_stopped(f: &Fixture, task: &str) -> Value {
    let o = exec(f, &["pipeline", "show", task, "--wait", "240s", "--json"], &[]);
    let v = o.json();
    assert_eq!(o.code, 0, "still running after 240s: {v:#}\n{}", log_of(&v));
    assert_eq!(v["status"], "stopped", "{v:#}");
    v
}

fn no_pipeline(f: &Fixture, task: &str) {
    let o = exec(f, &["pipeline", "show", task, "--json"], &[]);
    assert_eq!(o.code, 1, "a pipeline exists for {task}: {}", o.stdout);
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    ok(f, &["events", "--since", "0", "--task", task, "--json"], &[])["events"].as_array().unwrap().clone()
}

fn runs_of(f: &Fixture, task: &str, role: &str) -> Vec<Value> {
    ok(f, &["status", task, "--json"], &[])["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["role"] == role)
        .cloned()
        .collect()
}

fn is_rfc3339(v: &Value) -> bool {
    let t = s(v);
    t.len() >= 20 && t.as_bytes()[4] == b'-' && t.as_bytes()[10] == b'T' && (t.ends_with('Z') || t.contains('+'))
}

// --- (1) set, show, replace one list, clear -----------------------------------------

#[test]
fn set_keeps_order_and_duplicates_replaces_only_the_given_list_and_clear_empties_them() {
    let f = setup();
    assert!(!defaults_file(&f).exists());
    let v = set(&f, &["--implementer", IMPL_A, "--implementer", IMPL_B, "--implementer", IMPL_A, "--reviewer", REVIEW], "  usual pair  ");
    let path = s(&v["path"]);
    assert!(Path::new(&path).is_absolute(), "{path}");
    assert_eq!(Path::new(&path).file_name().unwrap(), FILE);
    assert!(defaults_file(&f).is_file());
    let mut replaced: Vec<String> = v["replaced"].as_array().unwrap().iter().map(s).collect();
    replaced.sort();
    assert_eq!(replaced, ["implementers", "reviewers"]);
    assert_eq!(v["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(v["implementers"]["reason"], "usual pair");

    // `show` works from any directory, also outside a repository
    let elsewhere = tempfile::tempdir().unwrap();
    let shown = ok_in(&f, elsewhere.path(), &["pipeline", "defaults", "show", "--json"], &[]);
    assert_eq!(shown["ok"], true);
    assert_eq!(shown["exists"], true);
    assert_eq!(shown["path"], path.as_str());
    assert_eq!(shown["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(shown["implementers"]["reason"], "usual pair");
    assert!(is_rfc3339(&shown["implementers"]["set_at"]), "{shown}");
    assert_eq!(shown["implementers"]["invalid"], json!([]));
    assert_eq!(shown["reviewers"]["entries"], entries(&[REVIEW]));
    assert_eq!(shown["reviewers"]["invalid"], json!([]));
    let impl_set_at = shown["implementers"]["set_at"].clone();
    // the human output lists the entries in stored order with the reason
    let human = exec(&f, &["pipeline", "defaults", "show"], &[]);
    assert_eq!(human.code, 0);
    assert!(human.stdout.contains(&format!("{IMPL_A}, {IMPL_B}, {IMPL_A}")), "{}", human.stdout);
    assert!(human.stdout.contains("usual pair"), "{}", human.stdout);

    // set --reviewer alone replaces only the reviewer list
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let v = set(&f, &["--reviewer", REVIEW_2, "--reviewer", REVIEW], "second reviewer first");
    assert_eq!(v["replaced"], json!(["reviewers"]));
    let shown = show_defaults(&f);
    assert_eq!(shown["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(shown["implementers"]["reason"], "usual pair");
    assert_eq!(shown["implementers"]["set_at"], impl_set_at);
    assert_eq!(shown["reviewers"]["entries"], entries(&[REVIEW_2, REVIEW]));
    assert_eq!(shown["reviewers"]["reason"], "second reviewer first");

    // clear --only reviewer
    let c = ok(&f, &["pipeline", "defaults", "clear", "--only", "reviewer", "--json"], &[]);
    assert_eq!(c["ok"], true);
    assert_eq!(c["path"], path.as_str());
    assert_eq!(c["cleared"], json!(["reviewers"]));
    assert_eq!(c["previous"]["reviewers"]["entries"], entries(&[REVIEW_2, REVIEW]));
    assert_eq!(c["previous"]["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(c["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(c["reviewers"], Value::Null);
    let shown = show_defaults(&f);
    assert_eq!(shown["reviewers"], Value::Null);
    assert_eq!(shown["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));

    // clear (both): the reviewer list was not stored, so only implementers is cleared
    let c = ok(&f, &["pipeline", "defaults", "clear", "--json"], &[]);
    assert_eq!(c["cleared"], json!(["implementers"]));
    assert_eq!(c["previous"]["implementers"]["entries"], entries(&[IMPL_A, IMPL_B, IMPL_A]));
    assert_eq!(c["previous"]["reviewers"], Value::Null);
    assert_eq!(c["implementers"], Value::Null);
    assert_eq!(c["reviewers"], Value::Null);
    let shown = show_defaults(&f);
    assert_eq!((shown["implementers"].clone(), shown["reviewers"].clone()), (Value::Null, Value::Null));

    // clearing again succeeds with nothing cleared
    let c = ok(&f, &["pipeline", "defaults", "clear", "--json"], &[]);
    assert_eq!(c["cleared"], json!([]));
    assert_eq!(c["previous"], json!({"implementers": null, "reviewers": null}));

    // none of this is a config setting
    let config = ok(&f, &["config", "list", "--json"], &[]).to_string();
    assert!(!config.contains("pipeline-defaults") && !config.contains("pipeline_defaults"), "{config}");
}

// --- (2) set refusals -----------------------------------------------------------------

#[test]
fn set_refusals_write_nothing() {
    let f = setup();
    set(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "first");
    let before = show_defaults(&f);
    let file_before = std::fs::read(defaults_file(&f)).unwrap();
    let base = ["pipeline", "defaults", "set"];
    let with = |flags: &[&str]| -> Vec<String> {
        let mut a: Vec<String> = base.iter().map(|x| x.to_string()).collect();
        a.extend(flags.iter().map(|x| x.to_string()));
        a.push("--json".into());
        a
    };
    let check = |args: Vec<String>, envs: &[(&str, &str)], needles: &[&str]| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        refused(&f, &args, envs, needles);
        assert_eq!(show_defaults(&f), before, "{args:?} changed the stored lists");
        assert_eq!(std::fs::read(defaults_file(&f)).unwrap(), file_before, "{args:?} rewrote the file");
    };
    check(with(&["--reason", "r"]), &[], &["--implementer", "--reviewer"]);
    check(with(&["--implementer", IMPL_B]), &[], &["--reason"]);
    check(with(&["--implementer", IMPL_B, "--reason", "   "]), &[], &["--reason"]);
    check(with(&["--implementer", IMPL_B, "--reason", "r"]), &[("THOTH_RUN_ID", "01FAKERUN")], &["THOTH_RUN_ID"]);
    for (flag, bad, why) in [
        ("--implementer", "nobody:sonnet:medium", "no such profile: nobody"),
        ("--implementer", "claude-b:auto:medium", "auto was removed"),
        ("--reviewer", "auto:gpt-5.6-luna:medium", "auto was removed"),
        ("--implementer", "claude-b:sonnet", "needs an effort"),
        ("--reviewer", "codex-a:gpt-5.6-luna:turbo", "invalid effort level"),
    ] {
        let label = format!("invalid {flag} entry 1 of 1 ('{bad}'): ");
        check(with(&[flag, bad, "--reason", "r"]), &[], &[&label, why]);
    }
    // a bad entry later in a list, and one in the other list, refuse the whole call
    check(
        with(&["--implementer", IMPL_B, "--implementer", "claude-b:sonnet:bogus", "--reason", "r"]),
        &[],
        &["invalid --implementer entry 2 of 2 ('claude-b:sonnet:bogus')", "invalid effort level"],
    );
    check(
        with(&["--implementer", IMPL_B, "--reviewer", REVIEW, "--reviewer", "codex-a", "--reason", "r"]),
        &[],
        &["invalid --reviewer entry 2 of 2 ('codex-a')", "two or three parts"],
    );
    // clear refuses inside a run as well
    let o = exec(&f, &["pipeline", "defaults", "clear", "--json"], &[("THOTH_RUN_ID", "01FAKERUN")]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("THOTH_RUN_ID"), "{}", o.stderr);
    assert_eq!(show_defaults(&f), before);
}

// --- (3) nothing stored -----------------------------------------------------------------

#[test]
fn a_left_out_list_with_nothing_stored_is_refused_naming_the_flag_and_defaults_set() {
    let f = setup();
    let task = create(&f, "normal");
    let err = refused(&f, &start_args(&task, &["--reviewer", REVIEW]), &[], &["the implementer list is empty"]);
    assert!(err.contains("--implementer PROFILE:MODEL[:EFFORT]") && err.contains("thoth pipeline defaults set"), "{err}");
    let err = refused(&f, &start_args(&task, &["--implementer", IMPL_B]), &[], &["the reviewer list is empty"]);
    assert!(err.contains("--reviewer PROFILE:MODEL[:EFFORT]") && err.contains("thoth pipeline defaults set"), "{err}");
    // a stored implementer list does not fill the reviewer list
    set(&f, &["--implementer", IMPL_B], "implementer only");
    refused(&f, &start_args(&task, &[]), &[], &["the reviewer list is empty", "thoth pipeline defaults set"]);
    // the refusal comes before the not-initialised refusal
    let fresh = tempfile::tempdir().unwrap();
    let o = exec_in(&f, fresh.path(), &start_args("01ANYTASK", &["--implementer", IMPL_B]), &[]);
    assert_eq!(o.code, 1);
    assert!(o.error().contains("the reviewer list is empty"), "{}", o.stderr);
    no_pipeline(&f, &task);
    assert_eq!(ok(&f, &["status", &task, "--json"], &[])["runs"], json!([]));
}

// --- (4) both lists from the stored defaults --------------------------------------------

#[test]
fn a_normal_task_without_list_flags_runs_to_pass_on_the_stored_lists() {
    let f = setup();
    set(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "light path pair");
    let stored = show_defaults(&f);
    let task = create_spec(&f, SPEC_NO_RISK);
    assert_eq!(ok(&f, &["status", &task, "--json"], &[])["risk"], "normal");
    let started = start(&f, &task, &[]);
    let st = &started["settings"];
    assert_eq!(st["implementers"], entries(&[IMPL_A]));
    assert_eq!(st["reviewers"], entries(&[REVIEW]));
    assert_eq!(st["implementers_source"], "default");
    assert_eq!(st["reviewers_source"], "default");
    assert_eq!(st["implementers_default"], json!({"reason": "light path pair", "set_at": stored["implementers"]["set_at"]}));
    assert_eq!(st["reviewers_default"], json!({"reason": "light path pair", "set_at": stored["reviewers"]["set_at"]}));
    assert_eq!(st["reason"], REASON, "the start's own reason is kept");

    let doc = wait_stopped(&f, &task);
    assert_eq!(doc["stop"]["reason"], "pass", "{doc:#}\n{}", log_of(&doc));
    for key in ["implementers_source", "reviewers_source", "implementers_default", "reviewers_default"] {
        assert_eq!(doc["settings"][key], st[key], "pipeline show {key}");
    }
    let human = exec(&f, &["pipeline", "show", &task], &[]);
    assert!(human.stdout.contains("stored default"), "{}", human.stdout);
    let ev = events(&f, &task);
    let started_ev = ev.iter().find(|e| e["type"] == "pipeline_started").unwrap();
    assert_eq!(started_ev["detail"]["settings"]["implementers_source"], "default");
    assert_eq!(started_ev["detail"]["settings"]["reviewers_default"], st["reviewers_default"]);
    // the dispatch carries the start's reason, not the stored list's
    let dispatch = ev.iter().find(|e| e["type"] == "pipeline_step" && e["detail"]["kind"] == "dispatch").unwrap();
    let argv = dispatch["detail"]["argv"].to_string();
    assert!(argv.contains(REASON) && !argv.contains("light path pair"), "{argv}");

    let impls = runs_of(&f, &task, "implementer");
    assert!(!impls.is_empty());
    assert!(impls.iter().all(|r| r["profile"] == "claude-a"), "{impls:?}");
    let revs = runs_of(&f, &task, "reviewer");
    assert!(!revs.is_empty());
    assert!(revs.iter().all(|r| r["profile"] == "codex-a"), "{revs:?}");
}

// --- (5) a flag beats a stored default ---------------------------------------------------

#[test]
fn an_explicit_flag_beats_the_stored_default_for_its_own_list_only() {
    let f = setup();
    set(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "stored pair");
    let task = create(&f, "normal");
    let started = start(&f, &task, &["--implementer", IMPL_B]);
    let st = &started["settings"];
    assert_eq!(st["implementers"], entries(&[IMPL_B]));
    assert_eq!(st["implementers_source"], "flag");
    assert_eq!(st["implementers_default"], Value::Null);
    assert_eq!(st["reviewers"], entries(&[REVIEW]));
    assert_eq!(st["reviewers_source"], "default");
    assert_eq!(st["reviewers_default"]["reason"], "stored pair");
    let doc = wait_stopped(&f, &task);
    assert_eq!(doc["stop"]["reason"], "pass", "{doc:#}\n{}", log_of(&doc));
    let impls = runs_of(&f, &task, "implementer");
    assert!(!impls.is_empty());
    assert!(impls.iter().all(|r| r["profile"] == "claude-b"), "{impls:?}");
}

// --- (6) a stored entry that no longer validates -----------------------------------------

#[test]
fn a_stored_entry_that_no_longer_validates_refuses_the_start_and_shows_as_invalid() {
    let f = setup();
    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "from the other machine");
    // a second home where claude-b is not registered
    let g = setup_with(&["claude-a"]);
    std::fs::copy(defaults_file(&f), defaults_file(&g)).unwrap();
    let task = create(&g, "normal");
    let err = refused(
        &g,
        &start_args(&task, &[]),
        &[],
        &["invalid stored default --implementer entry 1 of 1 ('claude-b:sonnet:medium')", "no such profile: claude-b"],
    );
    assert!(err.contains("thoth pipeline defaults set") && err.contains("--implementer PROFILE:MODEL[:EFFORT]"), "{err}");
    no_pipeline(&g, &task);
    assert!(!events(&g, &task).iter().any(|e| s(&e["type"]).starts_with("pipeline_")));
    assert_eq!(ok(&g, &["status", &task, "--json"], &[])["runs"], json!([]));

    let shown = show_defaults(&g);
    assert_eq!(shown["implementers"]["invalid"].as_array().unwrap().len(), 1, "{shown}");
    let bad = &shown["implementers"]["invalid"][0];
    assert_eq!(bad["entry_index"], 1);
    assert_eq!(bad["entry"], IMPL_B);
    assert!(s(&bad["error"]).contains("no such profile: claude-b"), "{bad}");
    assert_eq!(shown["reviewers"]["invalid"], json!([]));
    let human = exec(&g, &["pipeline", "defaults", "show"], &[]);
    assert!(human.stdout.contains("no such profile: claude-b"), "{}", human.stdout);

    // a home with no registered profile at all: every entry is invalid, and
    // show creates nothing there
    let bare = tempfile::tempdir().unwrap();
    std::fs::copy(defaults_file(&f), bare.path().join(FILE)).unwrap();
    let o = exec(&f, &["pipeline", "defaults", "show", "--json"], &[("THOTH_HOME", &bare.path().display().to_string())]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["implementers"]["invalid"][0]["entry"], IMPL_B);
    assert_eq!(v["reviewers"]["invalid"][0]["entry"], REVIEW);
    let names: Vec<String> = std::fs::read_dir(bare.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(names, [FILE], "show created something");

    // show on a home without a file creates no file or folder either
    let empty = tempfile::tempdir().unwrap();
    let home = empty.path().join("not-yet");
    let o = exec(&f, &["pipeline", "defaults", "show", "--json"], &[("THOTH_HOME", &home.display().to_string())]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(v["exists"], false);
    assert_eq!((v["implementers"].clone(), v["reviewers"].clone()), (Value::Null, Value::Null));
    assert!(!home.exists());
}

// --- (7) the risk rule -------------------------------------------------------------------

#[test]
fn a_stored_default_fills_a_list_only_at_low_or_normal_risk() {
    let f = setup();
    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "stored pair");
    for risk in ["important", "high"] {
        let task = create(&f, risk);
        for flags in [vec![], vec!["--implementer", IMPL_B], vec!["--reviewer", REVIEW]] {
            let err = refused(&f, &start_args(&task, &flags), &[], &[risk, "low or normal risk"]);
            let missing = if flags.first() == Some(&"--implementer") { "--reviewer" } else { "--implementer" };
            assert!(err.contains(missing), "{err}");
            no_pipeline(&f, &task);
        }
        let v = start(&f, &task, &["--implementer", IMPL_B, "--reviewer", REVIEW]);
        assert_eq!(v["settings"]["implementers_source"], "flag");
        assert_eq!(v["settings"]["reviewers_source"], "flag");
        wait_stopped(&f, &task);
    }
    // at these risks a stored entry that no longer validates is not what is reported
    std::fs::write(
        defaults_file(&f),
        json!({
            "implementers": {"entries": [{"profile": "nobody", "model": "sonnet", "effort": "medium"}], "reason": "r", "set_at": "2026-11-22T00:00:00Z"},
            "reviewers": null
        })
        .to_string(),
    )
    .unwrap();
    let task = create(&f, "high");
    let err = refused(&f, &start_args(&task, &["--reviewer", REVIEW]), &[], &["high", "low or normal risk"]);
    assert!(!err.contains("no such profile"), "{err}");

    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "stored pair");
    let task = create(&f, "low");
    let v = start(&f, &task, &[]);
    assert_eq!(v["settings"]["implementers_source"], "default");
    assert_eq!(v["settings"]["reviewers_source"], "default");
    wait_stopped(&f, &task);
}

/// SMALL-18 C4: the refusal says `were left out` for both list flags and
/// `was left out` for one.
#[test]
fn the_risk_refusal_agrees_with_the_number_of_flags_left_out() {
    let f = setup();
    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "stored pair");
    let task = create(&f, "high");
    let both = refused(&f, &start_args(&task, &[]), &[], &["--implementer and --reviewer were left out"]);
    assert!(!both.contains("was left out"), "{both}");
    refused(&f, &start_args(&task, &["--implementer", IMPL_B]), &[], &["--reviewer was left out"]);
    no_pipeline(&f, &task);
}

// --- (8) both flags given: a broken defaults file changes nothing ---------------------------

#[test]
fn with_both_flags_a_garbage_defaults_file_changes_nothing() {
    let f = setup();
    std::fs::write(defaults_file(&f), "{ this is not json").unwrap();
    let flags = ["--implementer", IMPL_B, "--reviewer", REVIEW, "--max-attempts", "3"];
    let task = create(&f, "normal");
    let with_garbage = start(&f, &task, &flags);
    let a = &with_garbage["settings"];
    assert_eq!(a["implementers_source"], "flag");
    assert_eq!(a["reviewers_source"], "flag");
    assert_eq!(a["implementers_default"], Value::Null);
    assert_eq!(a["reviewers_default"], Value::Null);
    wait_stopped(&f, &task);

    std::fs::remove_file(defaults_file(&f)).unwrap();
    let task2 = create(&f, "normal");
    let without = start(&f, &task2, &flags);
    assert_eq!(with_garbage["settings"], without["settings"]);
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&with_garbage), keys(&without));
    wait_stopped(&f, &task2);
}

// --- (9) a garbage file -------------------------------------------------------------------

#[test]
fn a_garbage_file_is_named_by_show_and_start_and_clear_repairs_it() {
    let f = setup();
    let path = defaults_file(&f);
    let task = create(&f, "normal");
    for garbage in [
        "{ this is not json".to_string(),
        json!({"implementers": {"entries": [], "reason": "r", "set_at": "2026-11-22T00:00:00Z"}, "reviewers": null}).to_string(),
    ] {
        std::fs::write(&path, &garbage).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        refused(&f, &["pipeline", "defaults", "show", "--json"], &[], &[&name]);
        refused(&f, &start_args(&task, &["--implementer", IMPL_B]), &[], &[&name]);
        refused(&f, &["pipeline", "defaults", "clear", "--only", "implementer", "--json"], &[], &[&name]);
        refused(&f, &["pipeline", "defaults", "set", "--implementer", IMPL_B, "--reason", "r", "--json"], &[], &[&name]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage, "a refusal rewrote the file");
        no_pipeline(&f, &task);

        let c = ok(&f, &["pipeline", "defaults", "clear", "--json"], &[]);
        assert!(c["warning"].as_str().is_some_and(|w| !w.is_empty()), "{c}");
        assert_eq!(c["previous"], json!({"implementers": null, "reviewers": null}));
        assert_eq!((c["implementers"].clone(), c["reviewers"].clone()), (Value::Null, Value::Null));
        let shown = show_defaults(&f);
        assert_eq!(shown["exists"], true);
        assert_eq!((shown["implementers"].clone(), shown["reviewers"].clone()), (Value::Null, Value::Null));
    }
}

// --- (10) MCP ------------------------------------------------------------------------------

struct McpServer {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl McpServer {
    fn start(f: &Fixture, envs: &[(&str, &str)]) -> Self {
        use std::process::Stdio;
        let mut c = common::scrubbed(std::process::Command::new(env!("CARGO_BIN_EXE_thoth")));
        c.arg("mcp").arg("--default-repo").arg(f.repo_path()).arg("--max-wait").arg("5s").current_dir(f.repo_path());
        for name in std::env::vars_os().map(|(name, _)| name) {
            let upper = name.to_string_lossy().to_uppercase();
            // GITENV-1: with GIT_CONFIG_KEY_n the count and values go too, so git never sees a count without its keys
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) || upper == "GIT_CONFIG_COUNT" || upper.starts_with("GIT_CONFIG_VALUE_") {
                c.env_remove(&name);
            }
        }
        for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME", "CLAUDE_PLUGIN_ROOT", "THOTH_PIPELINE_ID", "AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY"] {
            c.env_remove(var);
        }
        c.env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .env("FAKE_CHECK_DIR", check_dir(f));
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
fn mcp_pipeline_start_takes_both_lists_as_optional_and_uses_the_stored_defaults() {
    let f = setup();
    let task = create(&f, "normal");
    let mut server = McpServer::start(&f, &worker_env(&task));
    let list = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let tool = tools.iter().find(|t| t["name"] == "thoth_pipeline_start").unwrap();
    let mut required: Vec<String> = tool["inputSchema"]["required"].as_array().unwrap().iter().map(s).collect();
    required.sort();
    assert_eq!(required, ["reason", "task"]);
    assert!(s(&tool["description"]).contains("stored default"), "{tool}");
    assert!(!tools.iter().any(|t| s(&t["name"]).contains("defaults")), "no defaults tool");

    // nothing stored: a tool error naming defaults set, nothing started
    let r = server.call(2, "thoth_pipeline_start", json!({"task": task, "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r.to_string().contains("thoth pipeline defaults set"), "{r}");
    no_pipeline(&f, &task);

    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "mcp pair");
    // an empty array is still a tool error before anything starts
    let r = server.call(3, "thoth_pipeline_start", json!({"task": task, "implementers": [], "reason": REASON}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    no_pipeline(&f, &task);

    let r = server.call(4, "thoth_pipeline_start", json!({"task": task, "reason": REASON}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let started = &r["result"]["structuredContent"];
    assert_eq!(started["settings"]["implementers_source"], "default");
    assert_eq!(started["settings"]["reviewers_source"], "default");
    assert_eq!(started["settings"]["implementers"], entries(&[IMPL_B]));
    let doc = wait_stopped(&f, &task);
    assert_eq!(doc["stop"]["reason"], "pass", "{doc:#}\n{}", log_of(&doc));
    let r = server.call(5, "thoth_pipeline_show", json!({"task": task}));
    assert_eq!(r["result"]["structuredContent"]["settings"]["reviewers_default"]["reason"], "mcp pair");
}

// --- TESTGAP-1 (LIGHT-1 gaps) ---------------------------------------------------------------

/// A spec with a `task_type` label and a risk.
fn spec_typed(risk: &str, task_type: &str) -> String {
    format!(
        r#"{{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"{risk}","task_type":"{task_type}"}}"#
    )
}

/// What a file or folder at the defaults path holds, to compare before and after a refusal.
fn snapshot(path: &Path) -> String {
    if path.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        format!("folder:{names:?}")
    } else {
        format!("file:{}", std::fs::read_to_string(path).unwrap())
    }
}

#[test]
fn a_defaults_file_error_names_the_absolute_path_and_not_only_the_file_name() {
    let f = setup();
    let stored = set(&f, &["--implementer", IMPL_A, "--reviewer", REVIEW], "usual pair");
    let path_text = s(&stored["path"]);
    let path = PathBuf::from(&path_text);
    assert!(path.is_absolute(), "{path_text}");
    assert!(path.is_file(), "{path_text}");
    let task = create(&f, "normal");

    let not_json = "{ this is not json".to_string();
    let no_entry = json!({"implementers": {"entries": [], "reason": "r", "set_at": "2026-11-22T00:00:00Z"}, "reviewers": null}).to_string();
    // (how to break the file, the words every error holds, whether the folder state)
    for (label, parse_words, is_folder) in [
        ("not json", "cannot parse the pipeline defaults file", false),
        ("no entry", "cannot parse the pipeline defaults file", false),
        ("folder", "cannot read the pipeline defaults file", true),
    ] {
        if path.is_dir() {
            std::fs::remove_dir(&path).unwrap();
        }
        if is_folder {
            let _ = std::fs::remove_file(&path);
            std::fs::create_dir(&path).unwrap();
        } else {
            let text = if label == "not json" { &not_json } else { &no_entry };
            std::fs::write(&path, text).unwrap();
        }
        let before = snapshot(&path);

        let mut commands: Vec<Vec<&str>> = vec![vec!["pipeline", "defaults", "show", "--json"]];
        if !is_folder {
            commands.push(vec!["pipeline", "defaults", "clear", "--only", "implementer", "--json"]);
        }
        commands.push(vec!["pipeline", "defaults", "set", "--implementer", IMPL_B, "--reason", "r", "--json"]);
        commands.push(start_args(&task, &["--implementer", IMPL_B]));
        for args in &commands {
            let err = refused(&f, args, &[], &[parse_words, &path_text]);
            assert!(err.contains(&path_text), "[{label}] {args:?}: the error holds only a part of the path: {err}");
            assert_eq!(snapshot(&path), before, "[{label}] {args:?} touched the defaults file");
        }
        no_pipeline(&f, &task);
    }
}

#[test]
fn an_important_or_high_task_gets_its_own_refusal_before_the_light_path_rule() {
    let f = setup();
    set(&f, &["--implementer", IMPL_B, "--reviewer", REVIEW], "stored pair");
    for risk in ["important", "high"] {
        // (a) a document task
        let docs = create_spec(&f, &spec_typed(risk, "docs"));
        let err = refused(&f, &start_args(&docs, &[]), &[], &["is a document task", "thoth task accept"]);
        assert!(!err.contains("low or normal risk"), "[{risk}] docs: {err}");
        no_pipeline(&f, &docs);

        // (b) a closed code task
        let closed = create(&f, risk);
        ok(&f, &["task", "close", &closed, "--reason", "closed for the refusal order test", "--json"], &[]);
        let err = refused(&f, &start_args(&closed, &[]), &[], &["cancelled"]);
        assert!(!err.contains("low or normal risk"), "[{risk}] closed: {err}");
        no_pipeline(&f, &closed);

        // (c) a running pipeline, at high risk only
        if risk == "high" {
            let task = create(&f, risk);
            // a high risk task is dispatched only after an APPROVE spec review: record one
            let approve = f.home_path().join(format!("approve-{}.json", rand_name()));
            std::fs::write(&approve, r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}]}"#).unwrap();
            ok(&f, &["spec", "review", "record", &task, "-f", &approve.display().to_string(), "--json"], &[]);
            let slow = [("FAKE_TASK_ID", task.as_str()), ("FAKE_CLAUDE_SCENARIO", "slow"), ("FAKE_CODEX_SCENARIO", "review_pass")];
            let v = ok(&f, &start_args(&task, &["--implementer", IMPL_B, "--reviewer", REVIEW]), &slow);
            assert_eq!(v["status"], "running", "{v}");
            let err = refused(&f, &start_args(&task, &[]), &[], &["already has a running pipeline"]);
            assert!(!err.contains("low or normal risk"), "[{risk}] running: {err}");
            // end that pipeline: wait for the implementer run to exist, stop the pipeline, cancel the run, wait
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let run = loop {
                let st = ok(&f, &["status", &task, "--json"], &[]);
                if let Some(r) = st["runs"].as_array().and_then(|r| r.first()) {
                    break s(&r["run_id"]);
                }
                assert!(std::time::Instant::now() < deadline, "no run appeared: {st}");
                std::thread::sleep(std::time::Duration::from_millis(300));
            };
            ok(&f, &["pipeline", "stop", &task, "--json"], &[]);
            ok(&f, &["cancel", &run, "--json"], &[]);
            wait_stopped(&f, &task);
        }
    }
}
