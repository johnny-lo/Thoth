//! `thoth doctor` (SETUP-1). Every run uses the fake CLIs, a temporary
//! THOTH_HOME and a temporary repo; nothing here reaches a real claude, codex
//! or agy, or the network.

mod common;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");

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

fn plugin_list(version: &str, enabled: bool) -> String {
    json!([
        {"id":"superpowers@anthropic-plugin-directory","version":"6.4.2-8ca22dba9a94","scope":"user","enabled":true,"installPath":"y"},
        {"id":"thoth@thoth","version":version,"scope":"user","enabled":enabled,"installPath":"x"}
    ])
    .to_string()
}

/// `init`, profile `claude-a`, and approved check `t1` (`git --version`),
/// once per fixture; then a `doctor` command with the fakes, a thoth plugin
/// of this version and a clean shell environment, without arguments.
fn healthy_base(f: &common::Fixture) -> assert_cmd::Command {
    if !f.state_db().exists() {
        f.cmd().arg("init").assert().success();
        f.add_profile("claude-a");
        f.cmd()
            .args(["checks", "add", "t1", "--cmd", "git --version"])
            .assert()
            .success();
        f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    }
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_PLUGINS", plugin_list(&format!("{VERSION}-abc123"), true));
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c
}

/// `healthy_base` with `doctor --json`.
fn healthy(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = healthy_base(f);
    c.args(["doctor", "--json"]);
    c
}

fn run(mut c: assert_cmd::Command) -> (i32, Value) {
    let out = c.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap(), v)
}

fn items(v: &Value) -> BTreeMap<String, Value> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["id"].as_str().unwrap().to_string(), i.clone()))
        .collect()
}

fn statuses(v: &Value) -> BTreeMap<String, String> {
    items(v)
        .into_iter()
        .map(|(k, i)| (k, i["status"].as_str().unwrap().to_string()))
        .collect()
}

/// Step (1) of every perturbation test: the healthy run, exit 0, all ok.
fn baseline(f: &common::Fixture) -> BTreeMap<String, String> {
    let (code, v) = run(healthy(f));
    assert_eq!(code, 0, "{v:#}");
    let s = statuses(&v);
    assert!(s.values().all(|st| st == "ok"), "{v:#}");
    s
}

/// Step (3): every item of the baseline other than `changed` kept its status.
fn others_unchanged(base: &BTreeMap<String, String>, v: &Value, changed: &[&str]) {
    let now = statuses(v);
    for (id, st) in base {
        if changed.contains(&id.as_str()) {
            continue;
        }
        assert_eq!(now.get(id), Some(st), "item {id} changed: {v:#}");
    }
}

fn fix(item: &Value) -> String {
    item["fix"].as_str().unwrap_or_else(|| panic!("no fix: {item}")).to_string()
}

#[test]
fn all_ok() {
    let f = common::fixture();
    let (code, v) = run(healthy(&f));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], VERSION);
    let ids: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "state_dir",
            "cli_claude",
            "cli_codex",
            "cli_agy",
            "profiles",
            "profile:claude-a",
            "repo",
            "checks",
            "config",
            "plugin"
        ]
    );
    for i in v["items"].as_array().unwrap() {
        assert_eq!(i["status"], "ok", "{i}");
        assert_eq!(i["fix"], Value::Null, "{i}");
    }
    let checks = &items(&v)["checks"]["data"];
    assert_eq!(checks["total"], 1);
    assert_eq!(checks["approved"], 1);
    assert_eq!(checks["pending"], 0);
    for k in ["missing_program", "stale", "unreadable"] {
        assert_eq!(checks[k], json!([]), "{k}");
    }
}

#[test]
fn missing_cli() {
    let f = common::fixture();
    let base = baseline(&f);
    let gone = f.home.path().join("no-such-dir").join("claude-missing");
    let mut c = healthy(&f);
    c.env("THOTH_CLAUDE_BIN", &gone);
    let (code, v) = run(c);
    assert_eq!(code, 1, "{v:#}");
    let it = items(&v);
    assert_eq!(it["cli_claude"]["status"], "fail");
    assert!(fix(&it["cli_claude"]).contains("npm install -g @anthropic-ai/claude-code"));
    assert_eq!(it["plugin"]["status"], "unknown");
    assert_eq!(it["profile:claude-a"]["status"], "unknown");
    others_unchanged(&base, &v, &["cli_claude", "plugin", "profile:claude-a"]);

    // No agy profile: a missing agy only warns.
    let mut c = healthy(&f);
    c.env("THOTH_AGY_BIN", f.home.path().join("no-such-dir").join("agy-missing"));
    let (code, v) = run(c);
    assert_eq!(code, 0, "{v:#}");
    let it = items(&v);
    assert_eq!(it["cli_agy"]["status"], "warn");
    assert!(fix(&it["cli_agy"]).contains("antigravity.google"));
    others_unchanged(&base, &v, &["cli_agy"]);
}

#[test]
fn profile_logged_out() {
    let f = common::fixture();
    let base = baseline(&f);
    f.add_codex_profile("codex-a");
    let doctor = |shell: &str| {
        let mut c = healthy(&f);
        c.env("FAKE_CODEX_LOGGED_OUT", "1").args(["--shell", shell]);
        run(c)
    };

    let (code, v) = doctor("powershell");
    assert_eq!(code, 1, "{v:#}");
    let it = items(&v);
    let codex = &it["profile:codex-a"];
    assert_eq!(codex["status"], "fail");
    assert_eq!(codex["data"]["logged_in"], false);
    let fx = fix(codex);
    assert_eq!(fx, "Log in: thoth profile login codex-a");
    assert_eq!(it["profile:claude-a"]["status"], "ok");
    others_unchanged(&base, &v, &[]);

    let (code, v) = doctor("bash");
    assert_eq!(code, 1);
    let fx = fix(&items(&v)["profile:codex-a"]);
    assert_eq!(fx, "Log in: thoth profile login codex-a");
    assert_eq!(items(&v)["profile:claude-a"]["status"], "ok");

    let (code, v) = doctor("cmd");
    assert_eq!(code, 1);
    let fx = fix(&items(&v)["profile:codex-a"]);
    assert_eq!(fx, "Log in: thoth profile login codex-a");
    assert_eq!(items(&v)["profile:claude-a"]["status"], "ok");
}

/// Every path under `dir`, relative, sorted.
fn tree(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(root).unwrap().to_path_buf());
            if p.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn old_agentctl_dir_only() {
    let f = common::fixture();
    baseline(&f);
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".agentctl")).unwrap();
    std::fs::write(home.path().join(".agentctl").join("global.db"), "").unwrap();
    let before = tree(home.path());

    let out = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .args(["doctor", "--json"])
        .current_dir(f.repo.path())
        .env("USERPROFILE", home.path())
        .env("HOME", home.path())
        .env_remove("THOTH_HOME")
        .env_remove("AGENTCTL_HOME")
        // Never the real CLIs.
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("three ways"), "{stderr}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.is_object());
    let sd = &items(&v)["state_dir"];
    assert_eq!(sd["status"], "fail");
    assert_eq!(
        sd["data"]["old_dir_only"],
        home.path().join(".agentctl").display().to_string()
    );
    assert_eq!(sd["data"]["path"], home.path().join(".thoth").display().to_string());
    assert_eq!(sd["data"]["source"], "default");
    #[cfg(windows)]
    assert!(fix(sd).contains("setx THOTH_HOME \""), "{sd}");
    #[cfg(not(windows))]
    assert!(fix(sd).contains("export THOTH_HOME=\""), "{sd}");

    assert_eq!(tree(home.path()), before);
    assert_eq!(std::fs::read(home.path().join(".agentctl").join("global.db")).unwrap(), b"");
}

#[test]
fn broken_config() {
    let f = common::fixture();
    let base = baseline(&f);
    let cfg = f.home.path().join("config.json");
    std::fs::write(&cfg, "broken").unwrap();
    for (shell, want) in [("powershell", "Remove-Item \""), ("bash", "rm \""), ("cmd", "del \"")] {
        let mut c = healthy(&f);
        c.args(["--shell", shell]);
        let (code, v) = run(c);
        assert_eq!(code, 0, "{v:#}");
        let config = &items(&v)["config"];
        assert_eq!(config["status"], "warn");
        assert!(fix(config).contains(want), "{shell}: {config}");
        others_unchanged(&base, &v, &["config"]);
    }
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "broken");

    let text = r#"{"live_quota":true,"typo_key":1}"#;
    std::fs::write(&cfg, text).unwrap();
    let (code, v) = run(healthy(&f));
    assert_eq!(code, 0);
    let config = &items(&v)["config"];
    assert_eq!(config["status"], "warn");
    assert_eq!(config["data"]["unknown_keys"], json!(["typo_key"]));
    others_unchanged(&base, &v, &["config"]);
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
}

#[test]
fn plugin() {
    let f = common::fixture();
    let base = baseline(&f);

    let mut c = healthy(&f);
    c.env("FAKE_CLAUDE_PLUGINS", "[]");
    let (code, v) = run(c);
    assert_eq!(code, 1);
    let p = &items(&v)["plugin"];
    assert_eq!(p["status"], "fail");
    assert!(fix(p).contains("claude plugin install thoth@thoth"), "{p}");
    others_unchanged(&base, &v, &["plugin"]);

    let mut c = healthy(&f);
    c.env("FAKE_CLAUDE_PLUGINS", plugin_list("0.0.1", true));
    let (code, v) = run(c);
    assert_eq!(code, 1);
    let p = &items(&v)["plugin"];
    assert_eq!(p["status"], "fail");
    assert_eq!(p["data"]["installed_version"], "0.0.1");
    assert_eq!(p["data"]["expected_version"], VERSION);
    assert!(fix(p).contains("claude plugin update thoth@thoth"), "{p}");
    others_unchanged(&base, &v, &["plugin"]);

    let mut c = healthy(&f);
    c.env("FAKE_CLAUDE_PLUGINS", plugin_list(VERSION, false));
    let (code, v) = run(c);
    assert_eq!(code, 1);
    let p = &items(&v)["plugin"];
    assert_eq!(p["status"], "fail");
    assert!(fix(p).contains("claude plugin enable thoth@thoth"), "{p}");
    others_unchanged(&base, &v, &["plugin"]);

    let mut c = healthy(&f);
    c.env("THOTH_CLAUDE_BIN", f.home.path().join("no-such-dir").join("claude-missing"));
    let (code, v) = run(c);
    assert_eq!(code, 1);
    assert_eq!(items(&v)["plugin"]["status"], "unknown");
    others_unchanged(&base, &v, &["plugin", "cli_claude", "profile:claude-a"]);
}

/// The healthy fixture plus approved check `t3`, whose program is nowhere.
fn add_missing_program_check(f: &common::Fixture) {
    f.cmd()
        .args(["checks", "add", "t3", "--cmd", "nosuchexe-zz a.txt"])
        .assert()
        .success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
}

#[test]
fn checks_missing_program() {
    let f = common::fixture();
    let base = baseline(&f);
    add_missing_program_check(&f);
    let out = f.cmd().args(["checks", "list", "--json"]).output().unwrap();
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    let t3 = list["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check_id"] == "t3")
        .unwrap();
    assert_eq!(t3["status"], "approved", "{list:#}");

    for (shell, want) in [
        ("powershell", "$env:Path = \"DIR;$env:Path\""),
        ("bash", "export PATH=\"DIR:$PATH\""),
    ] {
        let mut c = healthy(&f);
        c.args(["--shell", shell]);
        let (code, v) = run(c);
        assert_eq!(code, 1, "{v:#}");
        let ch = &items(&v)["checks"];
        assert_eq!(ch["status"], "fail");
        assert_eq!(
            ch["data"]["missing_program"],
            json!([{"check_id":"t3","program":"nosuchexe-zz"}])
        );
        assert_eq!(ch["data"]["stale"], json!([]));
        assert!(ch["detail"].as_str().unwrap().contains("nosuchexe-zz"), "{ch}");
        assert!(fix(ch).contains(want), "{shell}: {ch}");
        others_unchanged(&base, &v, &["checks"]);
    }
}

fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut dirs = vec![dir.to_path_buf()];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(dirs).unwrap()
}

#[test]
fn checks_missing_program_named_once_in_the_fix() {
    let f = common::fixture();
    let base = baseline(&f);
    for (id, cmd) in [
        ("t3", "nosuchexe-zz a"),
        ("t4", "git --version"),
        ("t5", "nosuchexe-zz b"),
    ] {
        f.cmd().args(["checks", "add", id, "--cmd", cmd]).assert().success();
    }
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let (code, v) = run(healthy(&f));
    assert_eq!(code, 1, "{v:#}");
    let ch = &items(&v)["checks"];
    assert_eq!(
        ch["data"]["missing_program"],
        json!([
            {"check_id":"t3","program":"nosuchexe-zz"},
            {"check_id":"t5","program":"nosuchexe-zz"}
        ])
    );
    assert_eq!(fix(ch).matches("nosuchexe-zz").count(), 1, "{ch}");
    others_unchanged(&base, &v, &["checks"]);
}

#[test]
fn checks_missing_program_named_once_when_not_adjacent_in_the_list() {
    let f = common::fixture();
    baseline(&f);
    for (id, cmd) in [
        ("t3", "nosuchexe-zz a"),
        ("t4", "nosuchexe-yy a"),
        ("t5", "nosuchexe-zz b"),
    ] {
        f.cmd().args(["checks", "add", id, "--cmd", cmd]).assert().success();
    }
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let (_, v) = run(healthy(&f));
    let ch = &items(&v)["checks"];
    assert_eq!(ch["data"]["missing_program"].as_array().unwrap().len(), 3, "{ch}");
    assert_eq!(fix(ch).matches("nosuchexe-zz").count(), 1, "{ch}");
    assert_eq!(fix(ch).matches("nosuchexe-yy").count(), 1, "{ch}");
}

/// A state directory that is not there, and a half-set AGENTCTL_HOME: the
/// detail names the path and where it came from.
fn doctor_in(home: &Path, f: &common::Fixture, vars: &[(&str, &Path)]) -> Value {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.args(["doctor", "--json"])
        .current_dir(f.repo.path())
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env_remove("THOTH_HOME")
        .env_remove("AGENTCTL_HOME")
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"));
    for (k, v) in vars {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn state_dir_details_name_the_path_and_the_source() {
    let f = common::fixture();
    baseline(&f);
    let home = tempfile::tempdir().unwrap();

    let missing = home.path().join("elsewhere");
    let v = doctor_in(home.path(), &f, &[("THOTH_HOME", &missing)]);
    let sd = &items(&v)["state_dir"];
    assert_eq!(sd["status"], "warn");
    let d = sd["detail"].as_str().unwrap();
    assert!(d.contains(&missing.display().to_string()) && d.contains("THOTH_HOME"), "{d}");

    let v = doctor_in(home.path(), &f, &[]);
    let sd = &items(&v)["state_dir"];
    assert_eq!(sd["status"], "warn");
    let d = sd["detail"].as_str().unwrap();
    assert!(d.contains(&home.path().join(".thoth").display().to_string()), "{d}");
    assert!(!d.contains("THOTH_HOME"), "{d}");
    assert!(d.contains("default"), "{d}");

    let old = home.path().join("oldstate");
    let v = doctor_in(home.path(), &f, &[("AGENTCTL_HOME", &old)]);
    let sd = &items(&v)["state_dir"];
    assert_eq!(sd["status"], "fail");
    let d = sd["detail"].as_str().unwrap();
    assert!(d.contains(&old.display().to_string()), "{d}");
    assert!(d.contains(&home.path().join(".thoth").display().to_string()), "{d}");
}

#[test]
fn checks_stale() {
    let f = common::fixture();
    let base = baseline(&f);
    let t = tempfile::tempdir().unwrap();
    f.cmd()
        .args(["checks", "add", "t2", "--cmd", "tool-zz"])
        .assert()
        .success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT approved_detail FROM checks WHERE check_id = 't2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["exe"]["sha256"], Value::Null, "{detail}");

    #[cfg(windows)]
    let (tool, suffix) = (t.path().join("tool-zz.exe"), "tool-zz.exe");
    #[cfg(not(windows))]
    let (tool, suffix) = (t.path().join("tool-zz"), "tool-zz");
    std::fs::write(&tool, "anything").unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = path_with(t.path());

    let mut c = healthy(&f);
    c.env("PATH", &path);
    let (code, v) = run(c);
    assert_eq!(code, 0, "{v:#}");
    let ch = &items(&v)["checks"];
    assert_eq!(ch["status"], "warn");
    assert_eq!(ch["data"]["missing_program"], json!([]));
    let stale = ch["data"]["stale"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{ch}");
    assert_eq!(stale[0]["check_id"], "t2");
    let changed: Vec<&str> = stale[0]["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(changed.contains(&"exe=tool-zz"), "{changed:?}");
    assert!(
        changed.iter().any(|c| c.starts_with("exe=") && c.ends_with(suffix) && *c != "exe=tool-zz"),
        "{changed:?}"
    );
    assert!(fix(ch).contains("thoth checks approve"), "{ch}");
    others_unchanged(&base, &v, &["checks"]);

    f.cmd().env("PATH", &path).args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let mut c = healthy(&f);
    c.env("PATH", &path);
    let (code, v) = run(c);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(items(&v)["checks"]["status"], "ok", "{v:#}");
}

#[test]
fn checks_stale_input() {
    let f = common::fixture();
    let base = baseline(&f);
    f.cmd()
        .args(["checks", "add", "t1", "--cmd", "git --version", "--input", "a.txt"])
        .assert()
        .success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    std::fs::write(f.repo.path().join("a.txt"), "changed\n").unwrap();
    let (code, v) = run(healthy(&f));
    assert_eq!(code, 0, "{v:#}");
    let ch = &items(&v)["checks"];
    assert_eq!(ch["status"], "warn");
    assert_eq!(ch["data"]["stale"][0]["changed"], json!(["input=a.txt"]), "{ch}");
    others_unchanged(&base, &v, &["checks"]);
}

#[test]
fn shell_detection() {
    let f = common::fixture();
    let base = baseline(&f);
    let detect = |vars: &[(&str, &str)], extra: &[&str]| {
        let mut c = healthy(&f);
        for (k, val) in vars {
            c.env(k, val);
        }
        c.args(extra);
        let (_, v) = run(c);
        others_unchanged(&base, &v, &[]);
        (
            v["shell"].as_str().unwrap().to_string(),
            v["shell_source"].as_str().unwrap().to_string(),
        )
    };
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());

    #[cfg(windows)]
    {
        assert_eq!(detect(&[("MSYSTEM", "MINGW64")], &[]), pair("bash", "env"));
        assert_eq!(detect(&[("SHLVL", "1")], &[]), pair("bash", "env"));
        assert_eq!(
            detect(
                &[(
                    "PSModulePath",
                    r"C:\Users\x\Documents\WindowsPowerShell\Modules;C:\Program Files\WindowsPowerShell\Modules"
                )],
                &[]
            ),
            pair("powershell", "env")
        );
        assert_eq!(
            detect(
                &[
                    ("PSModulePath", r"C:\Program Files\WindowsPowerShell\Modules"),
                    ("PROMPT", "$P$G")
                ],
                &[]
            ),
            pair("cmd", "env")
        );
        assert_eq!(detect(&[], &[]), pair("powershell", "fallback"));
    }
    #[cfg(not(windows))]
    {
        assert_eq!(detect(&[("SHELL", "/bin/bash")], &[]), pair("bash", "env"));
        assert_eq!(detect(&[("SHELL", "/usr/bin/zsh")], &[]), pair("sh", "env"));
        assert_eq!(detect(&[], &[]), pair("sh", "fallback"));
    }
    assert_eq!(
        detect(&[("MSYSTEM", "MINGW64")], &["--shell", "cmd"]),
        pair("cmd", "flag")
    );
    let mut c = healthy(&f);
    c.args(["--shell", "pwsh"]);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn lang() {
    let f = common::fixture();
    baseline(&f);
    let human = |lang: Option<&str>| {
        let mut c = healthy_base(&f);
        if let Some(l) = lang {
            c.env("THOTH_LANG", l);
        }
        c.arg("doctor");
        let out = c.output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        String::from_utf8(out.stdout).unwrap()
    };
    let zh = human(Some("zh-TW"));
    assert!(zh.contains("Thoth 狀態目錄"), "{zh}");
    assert!(!zh.contains("state directory"), "{zh}");
    let en = human(None);
    assert!(en.contains("state directory"), "{en}");
    assert!(!en.contains("狀態目錄"), "{en}");

    let json_out = |lang: Option<&str>| {
        let mut c = healthy(&f);
        if let Some(l) = lang {
            c.env("THOTH_LANG", l);
        }
        c.output().unwrap().stdout
    };
    assert_eq!(json_out(Some("zh-TW")), json_out(None));
}

#[test]
fn checks_path_only_is_reported_without_changing_the_status() {
    let f = common::fixture();
    // 1.
    let base = baseline(&f);
    // 2.
    f.cmd()
        .args(["checks", "add", "t2", "--cmd", "git --version", "--exe-pin", "path"])
        .assert()
        .success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    // 3.
    let (code, v) = run(healthy(&f));
    assert_eq!(code, 0, "{v:#}");
    let checks = &items(&v)["checks"];
    assert_eq!(checks["status"], "ok", "{checks:#}");
    assert_eq!(checks["fix"], Value::Null, "{checks:#}");
    let data = &checks["data"];
    assert_eq!(data["total"], 2, "{checks:#}");
    assert_eq!(data["approved"], 2, "{checks:#}");
    assert_eq!(data["path_only"], json!(["t2"]), "{checks:#}");
    for k in ["missing_program", "stale", "unreadable"] {
        assert_eq!(data[k], json!([]), "{k}");
    }
    let detail = checks["detail"].as_str().unwrap();
    assert!(detail.contains("t2: program pinned by path only"), "{detail}");
    assert!(!detail.contains("t1: program pinned"), "{detail}");
    others_unchanged(&base, &v, &[]);
    // 4.
    let mut c = healthy_base(&f);
    c.env("THOTH_LANG", "zh-TW").arg("doctor");
    let out = c.output().unwrap();
    let zh = String::from_utf8(out.stdout).unwrap();
    assert!(zh.contains("t2：程式只綁路徑、不綁內容"), "{zh}");
}

struct Mcp {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Mcp {
    fn start(f: &common::Fixture) -> Mcp {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("FAKE_CLAUDE_PLUGINS", plugin_list(VERSION, true))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, input, output }
    }

    fn send(&mut self, id: u64, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.input, "{msg}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp() {
    let f = common::fixture();
    baseline(&f);
    let mut s = Mcp::start(&f);
    let init = s.send(1, "initialize", json!({"protocolVersion":"2025-06-18"}));
    assert_eq!(init["result"]["serverInfo"]["name"], "thoth");
    let list = s.send(2, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 35);
    let doctor = tools.iter().find(|t| t["name"] == "thoth_doctor").unwrap();
    assert_eq!(doctor["annotations"]["readOnlyHint"], true);

    let call = |s: &mut Mcp, id: u64, args: Value| {
        s.send(id, "tools/call", json!({"name":"thoth_doctor","arguments":args}))["result"].clone()
    };
    let r = call(&mut s, 3, json!({}));
    assert_eq!(r["isError"], false, "{r:#}");
    assert_eq!(r["structuredContent"]["ok"], true, "{r:#}");

    add_missing_program_check(&f);
    let r = call(&mut s, 4, json!({}));
    assert_eq!(r["isError"], false, "{r:#}");
    assert_eq!(r["structuredContent"]["ok"], false);
    assert_eq!(items(&r["structuredContent"])["checks"]["status"], "fail");

    let r = call(&mut s, 5, json!({"shell":"bash"}));
    assert_eq!(r["structuredContent"]["shell"], "bash", "{r:#}");
    assert_eq!(r["structuredContent"]["shell_source"], "flag");
}
