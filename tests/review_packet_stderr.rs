//! SMALL-11 (2): the review packet shows a check's stderr when its stdout is
//! empty, instead of `(no stored output)`; the evidence file a codex reviewer
//! reads for such a row starts with a header line and holds the whole stderr.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::PathBuf;

#[cfg(windows)]
const SHELL: &str = "cmd";
#[cfg(not(windows))]
const SHELL: &str = "sh";

/// The value `config set-secret` stores; the check that prints it to stderr
/// must show it masked everywhere.
const SECRET: &str = "dummy-asl-key-stderr-4242";
const ASL: &str = "aistupidlevel-api-key";

/// (stem, Windows body, Unix body). cmd prints the space before a trailing
/// redirection (`echo x 1>&2` prints `x `), so a body whose line a test
/// compares exactly puts the redirection first.
const SCRIPTS: &[(&str, &str, &str)] = &[
    (
        "onlyerr",
        "1>&2 echo first-err-line\r\n1>&2 echo second-err-line",
        "echo first-err-line >&2\necho second-err-line >&2",
    ),
    (
        "both",
        "echo the-stdout-line\r\necho the-stderr-line 1>&2",
        "echo the-stdout-line\necho the-stderr-line >&2",
    ),
    ("silent", "exit /b 0", "exit 0"),
    (
        "secreterr",
        "echo key is dummy-asl-key-stderr-4242 1>&2",
        "echo key is dummy-asl-key-stderr-4242 >&2",
    ),
];

fn commit_scripts(f: &Fixture) {
    let repo = f.repo_path();
    let mut names = Vec::new();
    for (stem, win, unix) in SCRIPTS {
        let name = if cfg!(windows) { format!("{stem}.cmd") } else { format!("{stem}.sh") };
        let body = if cfg!(windows) {
            format!("@echo off\r\n{win}\r\n")
        } else {
            format!("#!/bin/sh\n{unix}\n")
        };
        std::fs::write(repo.join(&name), body).unwrap();
        names.push(name);
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
}

fn script(stem: &str) -> String {
    if cfg!(windows) { format!(r".\{stem}.cmd") } else { format!("./{stem}.sh") }
}

fn json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{args:?}: not JSON ({e})"))
}

fn text(f: &Fixture, args: &[&str]) -> String {
    let mut c = f.cmd();
    c.args(args);
    String::from_utf8(f.run_ok(c, &args.join(" "))).unwrap()
}

/// A task with a `hooks` candidate at in_review, four approved checks
/// (stderr only, both streams, silent, a secret on stderr) and `verify`
/// run once. Returns the task id.
fn verified(f: &Fixture) -> String {
    commit_scripts(f);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_codex_profile("codex-a");
    let mut c = f.cmd();
    c.args(["config", "set-secret", ASL]).write_stdin(format!("{SECRET}\n"));
    c.assert().success();

    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    for (stem, _, _) in SCRIPTS {
        f.cmd()
            .args(["checks", "add", &format!("c-{stem}"), "--cmd", &script(stem), "--shell", SHELL, "--json"])
            .assert()
            .success();
    }
    f.cmd()
        .args(["checks", "approve", "--reason", "test approval", "--json"])
        .assert()
        .success();
    let v = json(f, &["verify", &run_id, "--json"]);
    assert_eq!(v["ok"], true, "{v}");
    task_id
}

fn row<'a>(packet: &'a Value, stem: &str) -> &'a Value {
    let label = format!("verify: {}", script(stem));
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["label"] == label.as_str())
        .unwrap_or_else(|| panic!("no evidence row {label}: {packet}"))
}

/// The Markdown lines of one index entry: its line and everything indented
/// under it.
fn entry(markdown: &str, code: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in markdown.lines() {
        if line.starts_with(&format!("- {code} | ")) {
            inside = true;
            out.push(line);
        } else if inside && line.starts_with("  ") {
            out.push(line);
        } else if inside {
            break;
        }
    }
    assert!(!out.is_empty(), "no index entry for {code}:\n{markdown}");
    out.join("\n")
}

const MARKER: &str = "  (stdout was empty; last lines of stderr:)";

#[test]
fn the_json_packet_gives_the_stderr_tail_and_names_the_stream() {
    let f = common::fixture();
    let task = verified(&f);
    let p = json(&f, &["review", "packet", &task, "--json"]);

    let only = row(&p, "onlyerr");
    assert_eq!(only["output_tail"], "first-err-line\nsecond-err-line", "{only}");
    assert_eq!(only["output_stream"], "stderr", "{only}");

    let both = row(&p, "both");
    assert_eq!(both["output_tail"], "the-stdout-line", "{both}");
    assert_eq!(both["output_stream"], "stdout", "{both}");

    let silent = row(&p, "silent");
    assert_eq!(silent["output_tail"], "", "{silent}");
    assert_eq!(silent["output_stream"], Value::Null, "{silent}");

    // a row that is not a command has a null stream
    let diff = p["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "diff")
        .expect("a diff row");
    assert_eq!(diff["output_stream"], Value::Null, "{diff}");
    assert_eq!(diff["output_tail"], Value::Null, "{diff}");
}

#[test]
fn the_markdown_packet_shows_the_stderr_under_a_marker_line_only_for_stderr_only_rows() {
    let f = common::fixture();
    let task = verified(&f);
    let p = json(&f, &["review", "packet", &task, "--json"]);
    let md = text(&f, &["review", "packet", &task]);

    let only = entry(&md, row(&p, "onlyerr")["code"].as_str().unwrap());
    assert!(only.contains(MARKER), "{only}");
    assert!(only.contains("first-err-line") && only.contains("second-err-line"), "{only}");
    assert!(!only.contains("(no stored output)"), "{only}");
    let lines: Vec<&str> = only.lines().collect();
    assert_eq!(lines[1], MARKER, "the marker is the line under the index line:\n{only}");

    let both = entry(&md, row(&p, "both")["code"].as_str().unwrap());
    assert!(both.contains("the-stdout-line"), "{both}");
    assert!(!both.contains("the-stderr-line") && !both.contains(MARKER), "{both}");

    let silent = entry(&md, row(&p, "silent")["code"].as_str().unwrap());
    assert!(silent.contains("(no stored output)"), "{silent}");
    assert!(!silent.contains(MARKER), "{silent}");
}

#[test]
fn a_secret_on_stderr_is_masked_in_the_json_and_the_markdown() {
    let f = common::fixture();
    let task = verified(&f);
    let json_text = text(&f, &["review", "packet", &task, "--json"]);
    let md = text(&f, &["review", "packet", &task]);
    for t in [&json_text, &md] {
        assert!(!t.contains(SECRET), "the stored secret is in the packet");
        assert!(t.contains("[redacted:"), "{t}");
    }
    let p: Value = serde_json::from_str(json_text.trim()).unwrap();
    let r = row(&p, "secreterr");
    assert_eq!(r["output_stream"], "stderr", "{r}");
    assert!(r["output_tail"].as_str().unwrap().starts_with("key is [redacted:"), "{r}");
}

#[test]
fn a_codex_reviewer_reads_the_stderr_in_the_evidence_file_under_a_header_line() {
    let f = common::fixture();
    let task = verified(&f);
    let p = json(&f, &["review", "packet", &task, "--json"]);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass").args([
        "review", "start", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort",
        "medium", "--json",
    ]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    let wt = PathBuf::from(r["worktree"].as_str().unwrap());
    let input = wt.join(".agent").join("review-input");
    let file = |stem: &str| {
        let code = row(&p, stem)["code"].as_str().unwrap().to_string();
        std::fs::read_to_string(input.join("evidence").join(format!("{code}.txt"))).unwrap()
    };

    let header = "[thoth: this command printed nothing on stdout; its stderr follows]";
    let only = file("onlyerr");
    assert!(only.starts_with(&format!("{header}\n")), "{only:?}");
    assert!(only.contains("first-err-line") && only.contains("second-err-line"), "{only:?}");
    let both = file("both");
    assert!(!both.contains(header) && !both.contains("the-stderr-line"), "{both:?}");
    assert!(both.contains("the-stdout-line"), "{both:?}");
    let silent = file("silent");
    assert!(!silent.contains(header), "{silent:?}");
    let secret = file("secreterr");
    assert!(secret.starts_with(&format!("{header}\n")), "{secret:?}");
    assert!(!secret.contains(SECRET) && secret.contains("[redacted:"), "{secret:?}");
    let packet_md = std::fs::read_to_string(input.join("packet.md")).unwrap();
    assert!(packet_md.contains(MARKER), "{packet_md}");
}
