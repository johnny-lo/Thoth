mod common;

use serde_json::Value;

/// A command whose executable resolves nowhere, so these tests do not depend on
/// what is installed. `a.txt` is a tracked file of the fixture repo.
const CMD: &str = "nosuchexe-zz a.txt";

fn json_of(out: Vec<u8>) -> Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn initialised() -> common::Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn add(f: &common::Fixture, id: &str) -> Value {
    json_of(
        f.cmd()
            .args(["checks", "add", id, "--cmd", CMD, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
}

fn list(f: &common::Fixture) -> Value {
    json_of(
        f.cmd()
            .args(["checks", "list", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
}

fn check_rows(f: &common::Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM checks", [], |r| r.get(0))
        .unwrap()
}

fn event_types(f: &common::Fixture) -> Vec<String> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT type FROM events WHERE type LIKE 'check_%' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[test]
fn add_infers_the_input_and_the_check_starts_pending() {
    let f = initialised();
    let v = add(&f, "clippy-all");
    assert_eq!(v["ok"], true);
    assert_eq!(v["check_id"], "clippy-all");
    assert_eq!(v["shell"], "argv");
    assert_eq!(v["cwd"], ".");
    assert_eq!(v["status"], "pending");
    assert_eq!(v["inputs"], serde_json::json!(["a.txt"]));

    let l = list(&f);
    assert_eq!(l["checks"][0]["status"], "pending");
    assert_eq!(l["checks"][0]["approved_at"], Value::Null);
    assert_eq!(event_types(&f), ["check_added"]);
}

#[test]
fn approve_pins_the_check_and_an_edited_input_turns_it_stale() {
    let f = initialised();
    add(&f, "t1");

    let v = json_of(
        f.cmd()
            .args(["checks", "approve", "--reason", "test approval", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["approved"][0]["check_id"], "t1");
    assert_eq!(v["approved"][0]["source"], "main");
    assert_eq!(v["unchanged"], serde_json::json!([]));
    assert_eq!(list(&f)["checks"][0]["status"], "approved");

    // the pinned commit is the main tree's HEAD
    let commit: String = f
        .state_conn()
        .query_row("SELECT approved_commit FROM checks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(commit, common::git(&f.repo_path(), &["rev-parse", "HEAD"]));

    // a second approve has nothing to do
    let out = f
        .cmd()
        .args(["checks", "approve", "--reason", "test approval"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8(out).unwrap().trim(), "nothing to approve");

    // one byte of the input changes: stale, and the changed line is named
    std::fs::write(f.repo_path().join("a.txt"), "onf\n").unwrap();
    let l = list(&f);
    assert_eq!(l["checks"][0]["status"], "stale");
    assert_eq!(l["checks"][0]["changed"], serde_json::json!(["input=a.txt"]));

    // approving again re-pins it
    let v = json_of(
        f.cmd()
            .args(["checks", "approve", "--reason", "test approval", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["approved"].as_array().unwrap().len(), 1);
    assert_eq!(list(&f)["checks"][0]["status"], "approved");
    assert_eq!(
        event_types(&f),
        ["check_added", "check_approved", "check_approved"]
    );
}

#[test]
fn the_human_approve_output_names_the_command_and_each_input_hash_prefix() {
    let f = initialised();
    add(&f, "t1");
    let out = f
        .cmd()
        .args(["checks", "approve", "--reason", "test approval"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("t1"), "{text}");
    assert!(text.contains("nosuchexe-zz a.txt"), "{text}");
    assert!(text.contains("a.txt  "), "{text}");
    // the input's sha256 is shown as its first 12 hex characters
    let line = text.lines().find(|l| l.contains("input: a.txt")).unwrap();
    let shown = line.split_whitespace().last().unwrap();
    assert_eq!(shown.len(), 12, "{line}");
}

#[test]
fn add_refuses_bad_ids_shells_cwds_and_missing_inputs_and_writes_nothing() {
    let f = initialised();
    for args in [
        vec!["Test", "--cmd", "x"],
        vec!["a b", "--cmd", "x"],
        vec!["ok", "--cmd", "x", "--shell", "fish"],
        vec!["ok", "--cmd", "x", "--cwd", ".."],
        vec!["ok", "--cmd", "x", "--cwd", "a/../../b"],
        vec!["ok", "--cmd", "x", "--input", "no-such-file.txt"],
        vec!["ok", "--cmd", "x", "--input", "../a.txt"],
    ] {
        f.cmd()
            .arg("checks")
            .arg("add")
            .args(&args)
            .assert()
            .code(1);
    }
    assert_eq!(check_rows(&f), 0);
    assert!(event_types(&f).is_empty());

    // the missing file is listed in the message
    let err = f
        .cmd()
        .args(["checks", "add", "ok", "--cmd", "x", "--input", "nope.txt"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(err).unwrap().contains("nope.txt"));
}

#[test]
fn explicit_inputs_are_merged_with_inferred_ones_sorted_and_deduplicated() {
    let f = initialised();
    let v = json_of(
        f.cmd()
            .args([
                "checks", "add", "t", "--cmd", CMD, "--input", "b.txt", "--input", "a.txt",
                "--json",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["inputs"], serde_json::json!(["a.txt", "b.txt"]));
}

#[test]
fn adding_an_existing_id_replaces_the_row_and_clears_the_approval() {
    let f = initialised();
    add(&f, "t1");
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    assert_eq!(list(&f)["checks"][0]["status"], "approved");

    f.cmd()
        .args(["checks", "add", "t1", "--cmd", "nosuchexe-zz b.txt"])
        .assert()
        .success();
    let l = list(&f);
    assert_eq!(l["checks"].as_array().unwrap().len(), 1);
    assert_eq!(l["checks"][0]["status"], "pending");
    assert_eq!(l["checks"][0]["cmd"], "nosuchexe-zz b.txt");

    let conn = f.state_conn();
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type='check_added' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let d: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(d["replaced"], true);
}

#[test]
fn remove_deletes_the_row_writes_an_event_and_refuses_an_unknown_id() {
    let f = initialised();
    add(&f, "t1");
    f.cmd().args(["checks", "remove", "t1"]).assert().success();
    assert_eq!(check_rows(&f), 0);
    assert_eq!(event_types(&f), ["check_added", "check_removed"]);
    f.cmd().args(["checks", "remove", "t1"]).assert().code(1);
}

#[test]
fn a_run_environment_cannot_add_approve_or_remove() {
    let f = initialised();
    add(&f, "t1");
    let rows_before = check_rows(&f);
    let events_before = event_types(&f);
    for args in [
        vec!["checks", "add", "t2", "--cmd", "x"],
        vec!["checks", "approve"],
        vec!["checks", "remove", "t1"],
    ] {
        let err = f
            .cmd()
            .env("THOTH_RUN_ID", "01SOMERUN")
            .args(&args)
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        assert!(
            String::from_utf8(err).unwrap().contains("THOTH_RUN_ID"),
            "{args:?}"
        );
    }
    assert_eq!(check_rows(&f), rows_before);
    assert_eq!(event_types(&f), events_before);
    assert_eq!(list(&f)["checks"][0]["status"], "pending");
    // reading stays allowed
    f.cmd()
        .env("THOTH_RUN_ID", "01SOMERUN")
        .args(["checks", "list"])
        .assert()
        .success();
}

#[test]
fn approve_run_reads_the_runs_worktree_and_needs_a_settled_run() {
    let f = initialised();
    let (_task, run_id, wt) = f.seed_run("p1", common::Fixture::SPEC);
    add(&f, "t1");

    // the worker changed the input inside its worktree only
    std::fs::write(wt.join("a.txt"), "worker edit\n").unwrap();

    // a queued run is refused, and nothing is approved
    f.cmd()
        .args(["checks", "approve", "--run", &run_id[..10]])
        .assert()
        .code(1);
    assert_eq!(list(&f)["checks"][0]["status"], "pending");

    f.state_conn()
        .execute("UPDATE runs SET status = 'completed' WHERE run_id = ?1", [&run_id])
        .unwrap();
    let v = json_of(
        f.cmd()
            .args(["checks", "approve", "--run", &run_id[..10], "--reason", "test approval", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["approved"][0]["source"], "run");

    // pinned against the worktree's bytes, so the main tree differs: stale
    let l = list(&f);
    assert_eq!(l["checks"][0]["status"], "stale");
    assert_eq!(l["checks"][0]["changed"], serde_json::json!(["input=a.txt"]));

    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE type='check_approved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let d: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(d["source"], "run");
    assert_eq!(d["run_id"], run_id.as_str());

    f.cmd()
        .args(["checks", "approve", "--run", "NOSUCHRUN"])
        .assert()
        .code(1);
}

#[test]
fn a_spec_may_list_checks_and_unapproved_ones_only_warn() {
    let f = initialised();
    add(&f, "test");
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    // In the catalogue but pending: a check that is not in it at all is a
    // spec lint error (tests/spec_lint.rs), not a warning.
    add(&f, "clippy");
    let spec = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"checks":["test","clippy"]}"#;
    let file = f.home_path().join("s.json");
    std::fs::write(&file, spec).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&file)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json_of(out);
    let w = v["warnings"].as_array().unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(
        w[0],
        "check clippy is not approved yet; approve it with: thoth checks approve --reason \"WHY\""
    );

    // without --json the warning goes to stderr
    let err = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(err).unwrap().contains("check clippy is not approved yet"));

    // a bad list is refused
    for bad in [r#"["test","test"]"#, r#"[""]"#, r#"["Test"]"#] {
        std::fs::write(
            &file,
            format!(r#"{{"goal":"g","acceptance_criteria":["AC1: x"],"checks":{bad}}}"#),
        )
        .unwrap();
        f.cmd()
            .args(["task", "create", "-f"])
            .arg(&file)
            .assert()
            .code(1);
    }
}

#[test]
fn a_v2_state_db_is_upgraded_in_place_on_first_use() {
    let f = initialised();
    {
        let conn = f.state_conn();
        conn.execute_batch("DROP TABLE checks; DELETE FROM schema_version WHERE version >= 3;")
            .unwrap();
    }
    add(&f, "t1");
    assert_eq!(check_rows(&f), 1);
}

/// P7-1b / AC7: the runner reads the catalogue when it builds the worker's
/// prompt; a spec that lists ids gets exactly the approved ones among them.
#[test]
fn the_worker_prompt_lists_the_approved_checks_the_spec_names() {
    let f = initialised();
    f.add_profile("claude-b");
    add(&f, "listed");
    add(&f, "unlisted");
    add(&f, "pending");
    f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
    // added after the approval: not approved, so not in the prompt
    f.cmd()
        .args(["checks", "add", "pending", "--cmd", "nosuchexe-zz b.txt", "--json"])
        .assert()
        .success();
    let spec = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"checks":["listed","pending"]}"#;
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    f.wait_for(&task_id, "120s");
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    let at = prompt.find("## Checks the control plane will re-run\n").expect(&prompt);
    assert!(
        prompt[at..].starts_with(&format!(
            "## Checks the control plane will re-run\nlisted: {CMD} (argv)\nRun the tests the spec names"
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("unlisted:"), "{prompt}");
    assert!(at < prompt.find("write the file `.agent/result.json`").unwrap());
}

// ---- `--exe-pin path` -------------------------------------------------------

const PINME: &str = if cfg!(windows) { "pinme.exe" } else { "pinme" };

/// Copies the stand-in binary `src` into `dir` as the program `pinme`.
fn put_program(dir: &std::path::Path, src: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let to = dir.join(PINME);
    std::fs::copy(src, &to).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A folder `name` under the fixture's home (outside the repo and every
/// worktree) holding the program `pinme`, a copy of fake-claude.
fn program_dir(f: &common::Fixture, name: &str) -> std::path::PathBuf {
    let dir = f.home_path().join(name);
    put_program(&dir, env!("CARGO_BIN_EXE_fake-claude"));
    dir
}

/// `dirs` first, then the current PATH.
fn path_with(dirs: &[&std::path::Path]) -> std::ffi::OsString {
    let mut all: Vec<std::path::PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    if let Some(p) = std::env::var_os("PATH") {
        all.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(all).unwrap()
}

fn run_json(c: &mut assert_cmd::Command) -> Value {
    json_of(c.assert().success().get_output().stdout.clone())
}

fn row<'a>(l: &'a Value, id: &str) -> &'a Value {
    l["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {l}"))
}

fn list_with(f: &common::Fixture, path: &std::ffi::OsStr) -> Value {
    run_json(f.cmd().env("PATH", path).args(["checks", "list", "--json"]))
}

fn approved_detail(f: &common::Fixture, id: &str) -> Value {
    let d: String = f
        .state_conn()
        .query_row("SELECT approved_detail FROM checks WHERE check_id = ?1", [id], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&d).unwrap()
}

#[test]
fn a_path_only_check_pins_the_path_and_not_the_bytes() {
    // 1.
    let f = initialised();
    let bin = program_dir(&f, "bin");
    let path = path_with(&[&bin]);
    // 2.
    let v = run_json(f.cmd().env("PATH", &path).args(["checks", "add", "byhash", "--cmd", "pinme --version", "--json"]));
    assert_eq!(v["exe_pin"], "hash");
    let v = run_json(f.cmd().env("PATH", &path).args([
        "checks", "add", "bypath", "--cmd", "pinme --version", "--exe-pin", "path", "--json",
    ]));
    assert_eq!(v["ok"], true);
    assert_eq!(v["exe_pin"], "path");
    assert_eq!(v["status"], "pending");
    let out = f
        .cmd()
        .env("PATH", &path)
        .args(["checks", "add", "bypath2", "--cmd", "pinme --version", "--exe-pin", "path"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("program pinned by path only"), "{out}");
    // 3.
    let v = run_json(f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval", "--json"]));
    assert_eq!(v["approved"].as_array().unwrap().len(), 3, "{v}");
    let l = list_with(&f, &path);
    for id in ["byhash", "bypath", "bypath2"] {
        assert_eq!(row(&l, id)["status"], "approved", "{l}");
    }
    assert_eq!(row(&l, "bypath")["exe_pin"], "path");
    assert_eq!(row(&l, "byhash")["exe_pin"], "hash");
    let human = f.cmd().env("PATH", &path).args(["checks", "list"]).assert().success().get_output().stdout.clone();
    let human = String::from_utf8(human).unwrap();
    let line = human.lines().find(|l| l.starts_with("bypath  approved")).expect(&human);
    assert!(line.contains("exe pinned by path only"), "{line}");
    let line = human.lines().find(|l| l.starts_with("byhash  ")).expect(&human);
    assert!(!line.contains("exe pinned by path only"), "{line}");
    // 4.
    let d = approved_detail(&f, "byhash");
    let sha = d["exe"]["sha256"].as_str().unwrap();
    assert!(sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()), "{d}");
    assert!(d["exe"].get("pin").is_none(), "{d}");
    let d = approved_detail(&f, "bypath");
    assert_eq!(d["exe"]["sha256"], Value::Null, "{d}");
    assert_eq!(d["exe"]["pin"], "path", "{d}");
    let p = d["exe"]["path"].as_str().unwrap();
    assert!(std::path::Path::new(p).is_absolute(), "{d}");
    assert!(p.ends_with(PINME), "{d}");
    // 5.
    put_program(&bin, env!("CARGO_BIN_EXE_fake-codex"));
    let l = list_with(&f, &path);
    assert_eq!(row(&l, "byhash")["status"], "stale", "{l}");
    let changed = row(&l, "byhash")["changed"].as_array().unwrap().clone();
    assert_eq!(changed.len(), 1, "{l}");
    let c0 = changed[0].as_str().unwrap();
    assert!(c0.starts_with("exe=") && c0.contains("pinme"), "{l}");
    for id in ["bypath", "bypath2"] {
        assert_eq!(row(&l, id)["status"], "approved", "{l}");
        assert_eq!(row(&l, id)["changed"], serde_json::json!([]), "{l}");
    }
    // 6.
    let bin2 = program_dir(&f, "bin2");
    let l = list_with(&f, &path_with(&[&bin2]));
    assert_eq!(row(&l, "bypath")["status"], "stale", "{l}");
    let changed: Vec<&str> =
        row(&l, "bypath")["changed"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert_eq!(changed.len(), 2, "{l}");
    assert!(changed.iter().all(|c| c.starts_with("exe=") && c.contains("pinme")), "{l}");
    assert_ne!(changed[0], changed[1]);
}

#[test]
fn exe_pin_path_is_refused_for_in_tree_programs_worktrees_relative_paths_and_missing_programs() {
    let refused = |f: &common::Fixture, path: &std::ffi::OsStr, cmd: &str, pin: &str| -> String {
        let out = f
            .cmd()
            .env("PATH", path)
            .args(["checks", "add", "t", "--cmd", cmd, "--exe-pin", pin])
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        String::from_utf8(out).unwrap()
    };
    let plain = path_with(&[]);
    // 1.
    let f = initialised();
    let (tool, body) = if cfg!(windows) { ("tool.cmd", "@echo off\r\n") } else { ("tool", "#!/bin/sh\n") };
    std::fs::write(f.repo_path().join(tool), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f.repo_path().join(tool), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let err = refused(&f, &plain, "./tool", "path");
    assert!(err.contains("--exe-pin path is refused for"), "{err}");
    assert!(err.contains("inside the repository tree"), "{err}");
    // 2.
    let repo_bin = f.repo_path().join("bin");
    put_program(&repo_bin, env!("CARGO_BIN_EXE_fake-claude"));
    let err = refused(&f, &path_with(&[&repo_bin]), "pinme --version", "path");
    assert!(err.contains("inside the repository tree"), "{err}");
    // 3.
    let (_task, _run, wt) = f.seed_run("p1", common::Fixture::SPEC);
    let wt_bin = wt.join("bin");
    put_program(&wt_bin, env!("CARGO_BIN_EXE_fake-claude"));
    let err = refused(&f, &path_with(&[&wt_bin]), "pinme --version", "path");
    assert!(err.contains("inside a worktree"), "{err}");
    // 4.
    assert_eq!(f.home_path().parent(), f.repo_path().parent());
    program_dir(&f, "bin");
    let home_name = f.home_path().file_name().unwrap().to_string_lossy().into_owned();
    let rel = format!("../{home_name}/bin/pinme --version");
    let err = refused(&f, &plain, &rel, "path");
    assert!(err.contains("named by a relative path"), "{err}");
    // 5.
    let err = refused(&f, &plain, "nosuchexe-zz a.txt", "path");
    assert!(err.contains("does not resolve"), "{err}");
    // 6.
    let err = refused(&f, &plain, "nosuchexe-zz a.txt", "bytes");
    assert!(err.contains("--exe-pin must be hash or path"), "{err}");
    // 7.
    assert_eq!(check_rows(&f), 0);
    assert!(event_types(&f).is_empty(), "{:?}", event_types(&f));
    // 8.
    let v = run_json(f.cmd().env("PATH", &plain).args(["checks", "add", "t", "--cmd", "nosuchexe-zz a.txt", "--json"]));
    assert_eq!(v["exe_pin"], "hash");
    assert_eq!(check_rows(&f), 1);
    let v = run_json(f.cmd().env("PATH", &plain).args(["checks", "add", "rel", "--cmd", &rel, "--json"]));
    assert_eq!(v["exe_pin"], "hash");
    assert_eq!(check_rows(&f), 2);
}

#[test]
fn re_adding_with_exe_pin_path_clears_the_approval_and_needs_a_new_one() {
    // 1.
    let f = initialised();
    let bin = program_dir(&f, "bin");
    let path = path_with(&[&bin]);
    f.cmd().env("PATH", &path).args(["checks", "add", "pin1", "--cmd", "pinme --version"]).assert().success();
    f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
    let h1 = row(&list_with(&f, &path), "pin1")["approved_fingerprint"].as_str().unwrap().to_string();
    // 2.
    f.cmd()
        .env("PATH", &path)
        .args(["checks", "add", "pin1", "--cmd", "pinme --version", "--exe-pin", "path", "--json"])
        .assert()
        .success();
    let l = list_with(&f, &path);
    assert_eq!(row(&l, "pin1")["status"], "pending", "{l}");
    assert_eq!(row(&l, "pin1")["approved_at"], Value::Null, "{l}");
    assert_eq!(row(&l, "pin1")["exe_pin"], "path", "{l}");
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE type = 'check_added' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["replaced"], true, "{detail}");
    assert_eq!(detail["exe_pin"], "path", "{detail}");
    // 3.
    let out = f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval"]).assert().success().get_output().stdout.clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("exe:   ") && out.contains("(pinned by path only)"), "{out}");
    let l = list_with(&f, &path);
    assert_eq!(row(&l, "pin1")["status"], "approved", "{l}");
    assert_ne!(row(&l, "pin1")["approved_fingerprint"].as_str().unwrap(), h1);
}

#[test]
fn approve_refuses_a_path_only_check_whose_program_now_resolves_inside_the_repo() {
    // 1.
    let f = initialised();
    let home_bin = program_dir(&f, "bin");
    let home_only = path_with(&[&home_bin]);
    let v = run_json(f.cmd().env("PATH", &home_only).args([
        "checks", "add", "inrepo", "--cmd", "pinme --version", "--exe-pin", "path", "--json",
    ]));
    assert_eq!(v["status"], "pending");
    f.cmd().env("PATH", &home_only).args(["checks", "add", "other", "--cmd", CMD]).assert().success();
    // 2.
    let repo_bin = f.repo_path().join("bin");
    put_program(&repo_bin, env!("CARGO_BIN_EXE_fake-claude"));
    let both = path_with(&[&repo_bin, &home_bin]);
    let v = run_json(f.cmd().env("PATH", &both).args(["checks", "approve", "--reason", "test approval", "--json"]));
    let approved = v["approved"].as_array().unwrap();
    assert_eq!(approved.len(), 1, "{v}");
    assert_eq!(approved[0]["check_id"], "other", "{v}");
    let refused = v["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{v}");
    assert_eq!(refused[0]["check_id"], "inrepo", "{v}");
    assert!(refused[0]["reason"].as_str().unwrap().contains("inside the repository tree"), "{v}");
    // 3.
    let l = list_with(&f, &both);
    assert_eq!(row(&l, "inrepo")["status"], "pending", "{l}");
    assert_eq!(row(&l, "inrepo")["approved_at"], Value::Null, "{l}");
    assert_eq!(row(&l, "other")["status"], "approved", "{l}");
    let approvals: Vec<String> = {
        let conn = f.state_conn();
        let mut stmt = conn.prepare("SELECT detail FROM events WHERE type = 'check_approved' ORDER BY seq").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap()
    };
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(serde_json::from_str::<Value>(&approvals[0]).unwrap()["check_id"], "other");
    // 4.
    let out = f.cmd().env("PATH", &both).args(["checks", "approve", "--reason", "test approval"]).assert().code(0).get_output().stdout.clone();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("not approved inrepo:"), "{out}");
    assert!(out.contains("inside the repository tree"), "{out}");
    assert!(!out.contains("nothing to approve"), "{out}");
    assert_eq!(row(&list_with(&f, &both), "inrepo")["status"], "pending");
    // 5.
    let v = run_json(f.cmd().env("PATH", &home_only).args(["checks", "approve", "--reason", "test approval", "--json"]));
    let approved = v["approved"].as_array().unwrap();
    assert_eq!(approved.len(), 1, "{v}");
    assert_eq!(approved[0]["check_id"], "inrepo", "{v}");
    assert!(v.get("refused").is_none(), "{v}");
    assert_eq!(row(&list_with(&f, &home_only), "inrepo")["status"], "approved");
}
