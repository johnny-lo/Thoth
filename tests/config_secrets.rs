//! CFG-2: the two score API keys kept by Thoth in `$THOTH_HOME/secrets.json`.
//! Every run uses a temporary THOTH_HOME, the fake CLIs, dummy keys and
//! file:// score fixtures; nothing here reaches a real CLI or the network,
//! and no dummy value may ever appear on stdout, stderr or in any file of
//! the state directory other than secrets.json itself.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

const FAKE_CLAUDE: &str = env!("CARGO_BIN_EXE_fake-claude");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ASL: &str = "aistupidlevel-api-key";
const AA: &str = "artificialanalysis-api-key";

/// The shell-detection variables, cleared so each test decides them.
const SHELL_VARS: [&str; 7] = [
    "MSYSTEM",
    "SHLVL",
    "PROMPT",
    "PSModulePath",
    "PSExecutionPolicyPreference",
    "SHELL",
    "THOTH_LANG",
];

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

fn setting<'a>(v: &'a Value, key: &str) -> &'a Value {
    v["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).unwrap()
}

fn secrets_json(f: &common::Fixture) -> PathBuf {
    f.home_path().join("secrets.json")
}

fn parsed_secrets(f: &common::Fixture) -> Value {
    serde_json::from_str(&fs::read_to_string(secrets_json(f)).unwrap()).unwrap()
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn plugin_list() -> String {
    json!([{"id":"thoth@thoth","version":format!("{VERSION}-abc123"),"scope":"user","enabled":true,"installPath":"x"}])
        .to_string()
}

/// A `doctor` command with the fakes, a plugin of this version and a clean
/// shell environment.
fn doctor_cmd(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_PLUGINS", plugin_list());
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c
}

/// `init`, profile `claude-a` and the approved check `t1`.
fn healthy_repo(f: &common::Fixture) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.cmd().args(["checks", "add", "t1", "--cmd", "git --version"]).assert().success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
}

fn item<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap()
}

/// The AI Stupid Level fixture as a file:// URL (as tests/models.rs does).
fn fixture_url() -> String {
    let p = fs::canonicalize("tests/fixtures/aistupidlevel/v1-models.json").unwrap();
    let shown = p.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{shown}")
}

/// The Artificial Analysis fixture pages as a file:// URL prefix (as
/// tests/models.rs does).
fn aa_fixture_url() -> String {
    let p = fs::canonicalize("tests/fixtures/artificialanalysis").unwrap();
    let shown = p.to_string_lossy();
    let shown = shown
        .strip_prefix(r"\\?\")
        .unwrap_or(&shown)
        .replace('\\', "/");
    format!("file:///{shown}/page-")
}

/// Fails when any file under THOTH_HOME other than secrets.json holds `needle`.
fn assert_absent_everywhere(f: &common::Fixture, needle: &str) {
    fn walk(dir: &Path, needle: &[u8]) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, needle);
                continue;
            }
            if path.file_name().is_some_and(|n| n == "secrets.json") {
                continue;
            }
            let bytes = fs::read(&path).unwrap_or_default();
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "{} contains the dummy key",
                path.display()
            );
        }
    }
    walk(&f.home_path(), needle.as_bytes());
}

/// Every `warning:` line of `stderr` is the Agent Arena one (the fixture
/// points THOTH_ARENA_URL at a file that does not exist), and there is one.
fn assert_only_arena_warns(stderr: &str) {
    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains("warning:")).collect();
    assert!(!warnings.is_empty(), "{stderr}");
    for line in warnings {
        assert!(line.starts_with("warning: Agent Arena refresh failed"), "{stderr}");
    }
}

/// stdout and stderr of `out` contain none of `dummies`.
fn assert_quiet(out: &Output, dummies: &[&str]) {
    let text = format!("{}\n{}", stdout_of(out), stderr_of(out));
    for d in dummies {
        assert!(!text.contains(d), "a dummy key leaked into the output:\n{text}");
    }
}

/// `config set-secret KEY` fed `stdin`, with `env` set on top of the fixture's.
fn set_secret(f: &common::Fixture, key: &str, stdin: &str, json: bool, env: &[(&str, &str)]) -> Output {
    let mut c = f.cmd();
    c.args(["config", "set-secret", key]);
    if json {
        c.arg("--json");
    }
    for (k, v) in env {
        c.env(k, v);
    }
    c.write_stdin(stdin);
    c.output().unwrap()
}

#[test]
fn set_secret_reads_stdin_stores_a_private_file_and_never_prints_the_value() {
    let f = common::fixture();
    let dummies = ["dummy-asl-key-1", "dummy-asl-key-2", "dummy-asl-key-3", "dummy-asl-env"];
    let before = file_names(&f.home_path());

    let out = set_secret(&f, ASL, "dummy-asl-key-1\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["key"], ASL);
    assert_eq!(v["value"], "set");
    assert_eq!(v["source"], "file");
    assert_eq!(v["previous"], "unset");
    assert!(v["shadowed_by"].is_null(), "{v}");
    assert!(v["path"].as_str().unwrap().ends_with("secrets.json"), "{v}");
    let stored = parsed_secrets(&f);
    assert_eq!(stored.as_object().unwrap().len(), 1, "{stored}");
    assert_eq!(stored["aistupidlevel_api_key"], "dummy-asl-key-1");
    #[cfg(unix)]
    assert_eq!(mode_of(&secrets_json(&f)), 0o600);
    let mut want = before.clone();
    want.push("secrets.json".to_string());
    want.sort();
    assert_eq!(file_names(&f.home_path()), want);

    let out = f.cmd().args(["config", "get", ASL, "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(json_of(&out), json!({"key": ASL, "value": "set"}));

    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    let v = json_of(&out);
    assert_eq!(setting(&v, ASL)["value"], "set");
    assert_eq!(setting(&v, ASL)["source"], "file");
    let sf = &v["secrets_file"];
    assert_eq!(sf["exists"], true, "{sf}");
    assert_eq!(sf["ok"], true, "{sf}");
    assert!(sf["error"].is_null(), "{sf}");
    assert_eq!(sf["unknown_keys"], json!([]), "{sf}");
    assert_eq!(sf["read"], true, "{sf}");
    #[cfg(unix)]
    assert_eq!(sf["private"], true, "{sf}");
    #[cfg(windows)]
    assert!(sf["private"].is_null(), "{sf}");

    let out = f.cmd().args(["config", "list"]).output().unwrap();
    assert_quiet(&out, &dummies);
    assert!(stdout_of(&out).contains("aistupidlevel-api-key: set (secrets.json)"), "{}", stdout_of(&out));
    assert_eq!(stderr_of(&out), "");

    let out = set_secret(&f, ASL, "dummy-asl-key-2\r\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(json_of(&out)["previous"], "set");
    assert_eq!(parsed_secrets(&f)["aistupidlevel_api_key"], "dummy-asl-key-2");
    #[cfg(unix)]
    assert_eq!(mode_of(&secrets_json(&f)), 0o600);

    let out = f
        .cmd()
        .env("AISTUPIDLEVEL_API_KEY", "dummy-asl-env")
        .args(["config", "list", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    let v = json_of(&out);
    assert_eq!(setting(&v, ASL)["value"], "set");
    assert_eq!(setting(&v, ASL)["source"], "env");

    let out = set_secret(&f, ASL, "dummy-asl-key-3\n", true, &[("AISTUPIDLEVEL_API_KEY", "dummy-asl-env")]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(json_of(&out)["shadowed_by"], "AISTUPIDLEVEL_API_KEY");
    assert_eq!(stderr_of(&out), "");

    let out = set_secret(&f, ASL, "dummy-asl-key-3\n", false, &[("AISTUPIDLEVEL_API_KEY", "dummy-asl-env")]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "set-secret aistupidlevel-api-key: set (secrets.json)\n");
    assert!(stderr_of(&out).contains("note: AISTUPIDLEVEL_API_KEY is set"), "{}", stderr_of(&out));

    for d in dummies {
        assert_absent_everywhere(&f, d);
    }
}

#[test]
fn set_secret_refuses_bad_input_and_argv_values_without_writing() {
    let f = common::fixture();
    let before = file_names(&f.home_path());
    let long = format!("{}\n", "a".repeat(1025));
    // (arguments after `config`, stdin, extra environment, stderr must contain)
    type Case<'a> = (Vec<&'a str>, &'a str, Option<(&'a str, &'a str)>, &'a str);
    let cases: Vec<Case> = vec![
        (vec!["set-secret", ASL], "", None, "nothing was entered"),
        (vec!["set-secret", ASL], "\n", None, "nothing was entered"),
        (vec!["set-secret", ASL], "   \n", None, "nothing was entered"),
        (vec!["set-secret", ASL], "dummy with space\n", None, "single token"),
        (vec!["set-secret", ASL], "dummy\x07bell\n", None, "single token"),
        (vec!["set-secret", ASL], "dummy-line-1\ndummy-line-2\n", None, "exactly one line"),
        (vec!["set-secret", ASL], &long, None, "1024 bytes"),
        (vec!["set-secret", "nope"], "dummy\n", None, "records-dir"),
        (vec!["set-secret", "claude-bin"], "dummy\n", None, "thoth config set claude-bin"),
        (vec!["set-secret", "home"], "dummy\n", None, "home cannot be stored in config.json: set THOTH_HOME instead"),
        (vec!["set-secret", ASL], "dummy-in-run\n", Some(("THOTH_RUN_ID", "01RUN")), "THOTH_RUN_ID"),
        (vec!["set-secret", ASL, "dummy-on-argv"], "", None, "command line"),
        (vec!["set-secret", ASL, "-dummy-dash"], "", None, "command line"),
        (vec!["set", ASL, "dummy-via-set"], "", None, "set-secret"),
    ];
    for (args, stdin, env, needle) in cases {
        let mut c = f.cmd();
        c.arg("config").args(&args);
        if let Some((k, v)) = env {
            c.env(k, v);
        }
        c.write_stdin(stdin);
        let out = c.output().unwrap();
        let e = stderr_of(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {e}");
        assert!(e.contains(needle), "{args:?}: {e}");
        assert_quiet(&out, &["dummy", "aaaa"]);
        if args[1] == "nope" {
            assert!(!e.contains("nope"), "{e}");
        }
        if args[0] == "set" {
            assert!(e.contains("AISTUPIDLEVEL_API_KEY"), "{e}");
        }
        assert!(!secrets_json(&f).exists(), "{args:?} created secrets.json");
        assert_eq!(file_names(&f.home_path()), before, "{args:?}");
    }

    fs::write(secrets_json(&f), "broken").unwrap();
    let out = set_secret(&f, ASL, "dummy-after-broken\n", false, &[]);
    assert_quiet(&out, &["dummy"]);
    assert_eq!(out.status.code(), Some(1));
    let e = stderr_of(&out);
    assert!(e.contains("secrets.json"), "{e}");
    assert!(e.contains("not a valid JSON object"), "{e}");
    assert_eq!(fs::read_to_string(secrets_json(&f)).unwrap(), "broken");
    assert!(file_names(&f.home_path()).iter().all(|n| !n.contains(".tmp-")));
    assert_absent_everywhere(&f, "dummy");
}

#[test]
fn unset_secret_removes_one_key_and_keeps_the_file() {
    let f = common::fixture();
    let dummies = ["dummy-asl", "dummy-aa", "dummy-aa-env"];
    let out = set_secret(&f, ASL, "dummy-asl\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let out = set_secret(&f, AA, "dummy-aa\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    let out = f.cmd().args(["config", "unset-secret", ASL, "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["previous"], "set");
    assert_eq!(v["value"], "unset");
    assert_eq!(v["source"], "default");
    let stored = parsed_secrets(&f);
    assert_eq!(stored.as_object().unwrap().keys().collect::<Vec<_>>(), ["artificialanalysis_api_key"]);
    #[cfg(unix)]
    assert_eq!(mode_of(&secrets_json(&f)), 0o600);

    let out = f.cmd().args(["config", "unset-secret", ASL, "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(json_of(&out)["previous"], "unset");

    let out = f.cmd().args(["config", "unset-secret", AA]).output().unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(stdout_of(&out), "unset-secret artificialanalysis-api-key: now unset (default)\n");
    assert!(secrets_json(&f).exists());
    assert_eq!(parsed_secrets(&f), json!({}));

    let out = f
        .cmd()
        .env("ARTIFICIALANALYSIS_API_KEY", "dummy-aa-env")
        .args(["config", "unset-secret", AA, "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["value"], "set");
    assert_eq!(v["source"], "env");

    let content = fs::read_to_string(secrets_json(&f)).unwrap();
    type Bad<'a> = (Vec<&'a str>, Option<(&'a str, &'a str)>, &'a str);
    let bad: Vec<Bad> = vec![
        (vec!["unset-secret", ASL], Some(("THOTH_RUN_ID", "01RUN")), "THOTH_RUN_ID"),
        (vec!["unset-secret", "claude-bin"], None, "thoth config unset claude-bin"),
        (vec!["unset-secret", "nope"], None, "records-dir"),
        (vec!["unset", ASL], None, "unset-secret"),
        (vec!["unset-secret", "home"], None, "home is not a secret: use thoth config unset home"),
    ];
    for (args, env, needle) in bad {
        let mut c = f.cmd();
        c.arg("config").args(&args);
        if let Some((k, v)) = env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        let e = stderr_of(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {e}");
        assert!(e.contains(needle), "{args:?}: {e}");
        assert_quiet(&out, &dummies);
        assert_eq!(fs::read_to_string(secrets_json(&f)).unwrap(), content, "{args:?}");
    }
    for d in dummies {
        assert_absent_everywhere(&f, d);
    }

    let g = common::fixture();
    let out = g.cmd().args(["config", "unset-secret", ASL, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["previous"], "unset");
    assert!(!secrets_json(&g).exists());
}

#[test]
fn a_broken_odd_or_exposed_secrets_file_is_reported_by_list_and_doctor_and_ignored_by_models() {
    let f = common::fixture();
    let path = secrets_json(&f);
    let shown = path.display().to_string();
    let dummies = ["dummy-asl-odd", "dummy-asl-ok", "dummy-aa-ok"];

    fs::write(&path, "broken").unwrap();
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["secrets_file"]["ok"], false, "{v}");
    assert!(v["secrets_file"]["error"].as_str().is_some_and(|e| !e.is_empty()), "{v}");
    for key in [ASL, AA] {
        assert_eq!(setting(&v, key)["value"], "unset");
        assert_eq!(setting(&v, key)["source"], "default");
    }
    assert_eq!(stderr_of(&out), "");
    let out = f.cmd().args(["config", "list"]).output().unwrap();
    let h = stdout_of(&out);
    assert!(h.contains(&format!("warning: {shown} cannot be used (")), "{h}");
    assert!(h.contains("the keys stored in it are ignored"), "{h}");

    healthy_repo(&f);
    let empty = tempfile::tempdir().unwrap();
    let out = f
        .cmd()
        .env("PATH", empty.path())
        .args(["models", "--refresh"])
        .output()
        .unwrap();
    // RJ-2: no usable key is a note, not an error; Agent Arena is still tried.
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let e = stderr_of(&out);
    assert!(e.contains("AISTUPIDLEVEL_API_KEY"), "{e}");
    assert!(e.contains("set-secret"), "{e}");
    assert!(e.contains(&format!("{shown} cannot be used (")), "{e}");
    assert!(e.contains("see thoth config list"), "{e}");
    assert_only_arena_warns(&e);

    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    let d = json_of(&out);
    let c = item(&d, "config");
    assert_eq!(c["status"], "warn", "{d:#}");
    assert_eq!(c["data"]["secrets_file"]["ok"], false, "{c}");
    assert!(c["fix"].as_str().unwrap().contains("secrets.json"), "{c}");
    assert!(!stderr_of(&out).contains("warning:"), "{}", stderr_of(&out));

    fs::write(&path, r#"{"aistupidlevel_api_key":"dummy-asl-odd","typo":1,"artificialanalysis_api_key":5}"#).unwrap();
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    let v = json_of(&out);
    assert_eq!(v["secrets_file"]["ok"], true, "{v}");
    assert_eq!(v["secrets_file"]["unknown_keys"], json!(["typo"]), "{v}");
    assert_eq!(setting(&v, ASL)["value"], "set");
    assert_eq!(setting(&v, ASL)["source"], "file");
    assert_eq!(setting(&v, AA)["value"], "unset");
    assert_eq!(setting(&v, AA)["source"], "default");
    let out = f.cmd().args(["config", "list"]).output().unwrap();
    assert_quiet(&out, &dummies);
    let h = stdout_of(&out);
    assert!(h.contains(&format!("warning: {shown} has unknown keys: typo (ignored)")), "{h}");
    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    assert_quiet(&out, &dummies);
    let d = json_of(&out);
    let c = item(&d, "config");
    assert_eq!(c["status"], "warn", "{d:#}");
    assert_eq!(c["data"]["secrets_file"]["unknown_keys"], json!(["typo"]), "{c}");
    assert_eq!(c["data"]["secrets"][ASL], json!({"value": "set", "source": "file"}), "{c}");
    assert_eq!(c["data"]["secrets"][AA]["value"], "unset", "{c}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&path, r#"{"aistupidlevel_api_key":"dummy-asl-ok"}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
        assert_quiet(&out, &dummies);
        assert_eq!(json_of(&out)["secrets_file"]["private"], false);
        let out = f.cmd().args(["config", "list"]).output().unwrap();
        assert_quiet(&out, &dummies);
        assert!(stdout_of(&out).contains("is readable by other users; run chmod 600 "), "{}", stdout_of(&out));
        // In a run the file is not read: its mode is still in the JSON, but
        // the human output carries only the note.
        let out = f.cmd().env("THOTH_RUN_ID", "01RUN").args(["config", "list", "--json"]).output().unwrap();
        assert_quiet(&out, &dummies);
        assert_eq!(json_of(&out)["secrets_file"]["private"], false);
        let out = f.cmd().env("THOTH_RUN_ID", "01RUN").args(["config", "list"]).output().unwrap();
        assert_quiet(&out, &dummies);
        let h = stdout_of(&out);
        assert!(h.contains("is not read while THOTH_RUN_ID is set"), "{h}");
        assert!(!h.contains("is readable by other users"), "{h}");
        let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
        assert_quiet(&out, &dummies);
        let d = json_of(&out);
        let c = item(&d, "config");
        assert_eq!(c["status"], "warn", "{d:#}");
        assert!(c["fix"].as_str().unwrap().contains("chmod 600"), "{c}");

        let out = set_secret(&f, AA, "dummy-aa-ok\n", true, &[]);
        assert_quiet(&out, &dummies);
        assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
        assert_eq!(mode_of(&path), 0o600);
        let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
        assert_quiet(&out, &dummies);
        let d = json_of(&out);
        let c = item(&d, "config");
        assert_eq!(c["status"], "ok", "{d:#}");
        assert!(c["fix"].is_null(), "{c}");
        for key in [ASL, AA] {
            assert_eq!(c["data"]["secrets"][key], json!({"value": "set", "source": "file"}), "{c}");
        }
        let out = doctor_cmd(&f).env("THOTH_LANG", "zh-TW").arg("doctor").output().unwrap();
        assert_quiet(&out, &dummies);
    }
    for d in dummies {
        assert_absent_everywhere(&f, d);
    }
}

/// RJ-2 with CFG-2: no key anywhere is a note naming both variables and
/// `thoth config set-secret`; Agent Arena is still refreshed (and fails here,
/// offline, with only its warning).
#[test]
fn models_refresh_without_any_key_notes_set_secret_and_still_tries_agent_arena() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let out = f.cmd().args(["models", "--refresh"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let e = stderr_of(&out);
    assert!(e.contains("note: AI Stupid Level and Artificial Analysis were skipped"), "{e}");
    assert!(e.contains("AISTUPIDLEVEL_API_KEY"), "{e}");
    assert!(e.contains("ARTIFICIALANALYSIS_API_KEY"), "{e}");
    assert!(e.contains("thoth config set-secret"), "{e}");
    assert!(!e.contains("THOTH_RUN_ID"), "{e}");
    assert!(!e.contains("cannot be used"), "{e}");
    assert_only_arena_warns(&e);
    assert!(!f.home_path().join("model-scores.json").exists());
    assert!(!f.home_path().join("model-scores-aa.json").exists());
}

#[test]
fn models_refresh_uses_the_stored_key_but_a_worker_does_not() {
    let f = common::fixture();
    let dummies = ["dummy-asl-stored"];
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let out = set_secret(&f, ASL, "dummy-asl-stored\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let cache = f.home_path().join("model-scores.json");

    let out = f
        .cmd()
        .env("THOTH_SCORES_URL", fixture_url())
        .args(["models", "--refresh", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(json_of(&out)["cache"]["refreshed"], true);

    fs::remove_file(&cache).unwrap();
    let out = f
        .cmd()
        .env("THOTH_SCORES_URL", fixture_url())
        .env("AISTUPIDLEVEL_API_KEY", "   ")
        .args(["models", "--refresh", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert_eq!(json_of(&out)["cache"]["refreshed"], true);

    f.cmd().args(["config", "set", "scores-auto-refresh", "on"]).assert().success();
    fs::remove_file(&cache).unwrap();
    let out = f
        .cmd()
        .env("THOTH_SCORES_URL", fixture_url())
        .args(["models", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    // RJ-2: Agent Arena needs no key, so the automatic refresh tries it too.
    assert_eq!(v["auto_refresh"]["attempted"], json!(["AI Stupid Level", "Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["refreshed"], json!(["AI Stupid Level"]), "{v}");
    assert_eq!(v["cache"]["refreshed"], true, "{v}");

    fs::remove_file(&cache).unwrap();
    let empty = tempfile::tempdir().unwrap();
    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .env("PATH", empty.path())
        .args(["models", "--refresh"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    // RJ-2: no usable key is a note, not an error; Agent Arena is still tried.
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let e = stderr_of(&out);
    assert!(e.contains("AISTUPIDLEVEL_API_KEY"), "{e}");
    assert!(e.contains("secrets.json is not read while THOTH_RUN_ID is set"), "{e}");
    assert_only_arena_warns(&e);
    assert!(!cache.exists());

    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["config", "list", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    let v = json_of(&out);
    assert_eq!(setting(&v, ASL)["value"], "unset", "{v}");
    assert_eq!(setting(&v, ASL)["source"], "default", "{v}");
    assert_eq!(v["secrets_file"]["read"], false, "{v}");
    assert_eq!(v["secrets_file"]["exists"], true, "{v}");
    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["config", "list"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert!(stdout_of(&out).contains("is not read while THOTH_RUN_ID is set"), "{}", stdout_of(&out));

    let out = f
        .cmd()
        .env("THOTH_SCORES_URL", fixture_url())
        .args(["report", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    for d in dummies {
        assert_absent_everywhere(&f, d);
    }
}

/// The Artificial Analysis key stored only in secrets.json (the fixture
/// clears ARTIFICIALANALYSIS_API_KEY) is enough for `models --refresh` to
/// fetch it; THOTH_AA_URL points at the local fixture pages.
#[test]
fn models_refresh_uses_the_stored_artificial_analysis_key() {
    let f = common::fixture();
    let dummies = ["dummy-aa-stored"];
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let out = set_secret(&f, AA, "dummy-aa-stored\n", true, &[]);
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    let out = f
        .cmd()
        .env("THOTH_AA_URL", aa_fixture_url())
        .args(["models", "--refresh", "--json"])
        .output()
        .unwrap();
    assert_quiet(&out, &dummies);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["external"]["artificial_analysis"]["cache"]["refreshed"], true, "{v}");
    assert!(f.home_path().join("model-scores-aa.json").exists());
    assert!(!f.home_path().join("model-scores.json").exists());
    let e = stderr_of(&out);
    assert!(!e.contains("ARTIFICIALANALYSIS_API_KEY"), "{e}");
    assert_only_arena_warns(&e);

    for d in dummies {
        assert_absent_everywhere(&f, d);
    }
}

/// While THOTH_RUN_ID is set `config list` does not open secrets.json: the
/// JSON says it was not read and carries no verdict on its content, and the
/// text output says only that, even for a broken file or one with unknown
/// keys (or, on Unix, one others can read).
#[test]
fn config_list_in_a_run_says_only_that_secrets_json_is_not_read() {
    let f = common::fixture();
    let path = secrets_json(&f);
    let shown = path.display().to_string();
    let dummies = ["dummy-asl-run"];
    for content in ["broken", r#"{"aistupidlevel_api_key":"dummy-asl-run","typo":1}"#] {
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let out = f.cmd().env("THOTH_RUN_ID", "01RUN").args(["config", "list", "--json"]).output().unwrap();
        assert_quiet(&out, &dummies);
        assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
        let v = json_of(&out);
        let s = &v["secrets_file"];
        assert_eq!(s["read"], false, "{v}");
        assert_eq!(s["exists"], true, "{v}");
        assert!(s["ok"].is_null() && s["error"].is_null() && s["unknown_keys"].is_null(), "{v}");
        #[cfg(unix)]
        assert_eq!(s["private"], false, "{v}");
        #[cfg(not(unix))]
        assert!(s["private"].is_null(), "{v}");
        assert_eq!(setting(&v, ASL)["value"], "unset", "{v}");

        let out = f.cmd().env("THOTH_RUN_ID", "01RUN").args(["config", "list"]).output().unwrap();
        assert_quiet(&out, &dummies);
        assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
        let h = stdout_of(&out);
        let about: Vec<&str> = h.lines().filter(|l| l.contains(&shown)).collect();
        assert_eq!(about, [format!("note: {shown} is not read while THOTH_RUN_ID is set")], "{h}");
        assert!(!h.contains("warning:"), "{h}");
    }
    assert_absent_everywhere(&f, dummies[0]);
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &common::Fixture, extra_env: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", FAKE_CLAUDE)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("ARTIFICIALANALYSIS_API_KEY")
            .env_remove("AISTUPIDLEVEL_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_config_list_shows_a_stored_key_as_set_and_offers_no_secret_tool() {
    let f = common::fixture();
    let dummy = "dummy-mcp-key";
    let out = set_secret(&f, ASL, "dummy-mcp-key\n", true, &[]);
    assert_quiet(&out, &[dummy]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    let mut s = Server::start(&f, &[]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    let list = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    for t in list["result"]["tools"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        assert!(!name.contains("secret"), "{name}");
    }
    let res = s.call(3, "thoth_config_list", json!({}));
    assert_eq!(res["result"]["isError"], false, "{res}");
    let sc = &res["result"]["structuredContent"];
    let key = sc["settings"].as_array().unwrap().iter().find(|x| x["key"] == ASL).unwrap();
    assert_eq!(key["value"], "set", "{key}");
    assert_eq!(key["source"], "file", "{key}");
    assert_eq!(sc["secrets_file"]["exists"], true, "{sc}");
    assert!(!res.to_string().contains(dummy), "{res}");

    let res = s.call(4, "thoth_config_set_secret", json!({"key": ASL}));
    assert!(res["result"]["isError"] == true || res["error"].is_object(), "{res}");
    assert!(!res.to_string().contains(dummy), "{res}");
    drop(s);
    assert_absent_everywhere(&f, dummy);
}
