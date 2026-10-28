//! RES-1a: a research task's worker gets its harness's web tools, delivers
//! `research` claims with sources, and Thoth checks their shape on ingest.
//! Always the three fakes (which open no URL) and a temporary THOTH_HOME.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const DEFAULT_TOOLS: &str = "Bash,Read,Write,Edit,Glob,Grep";
const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";
/// `AGY_IMPLEMENTER_TOOLS` (AGY-1: agy 1.4.4's `read_file`, `write_file`
/// and `edit_file` beside the 1.4.2 names), which the integration tests
/// cannot import.
const AGY_TOOLS: &[&str] = &[
    "run_command", "command_status", "send_command_input", "read_file", "view_file", "list_dir",
    "find_by_name", "grep_search", "write_file", "edit_file", "write_to_file",
    "replace_file_content", "multi_replace_file_content", "sed_file", "finish", "wait",
    "wait_5_seconds",
];
/// The four argv tokens that open a codex research worker's network.
const CODEX_NETWORK: [&str; 4] = [
    "-c", "sandbox_workspace_write.network_access=true",
    "-c", "web_search=\"live\"",
];

fn spec(task_type: &str) -> String {
    format!(
        r#"{{"goal":"find out how the uploader retries","acceptance_criteria":["AC1: the report is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"{task_type}"}}"#
    )
}

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

fn json_ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    serde_json::from_slice(&f.run_ok(c, &args.join(" "))).expect("valid json")
}

/// `file://` plus the absolute path with `/` separators: `file:///C:/...` on
/// Windows, `file:///tmp/...` on Linux (the path already starts with `/`).
fn file_url(p: &Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Waits until the task is one of `wanted` and its run has ended, so nothing
/// below races a live runner.
fn settle(f: &Fixture, task_id: &str, run_id: &str, wanted: &[&str]) -> String {
    let status = f.wait_for_task_status(task_id, wanted, 120);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let run: String = f
            .state_conn()
            .query_row("SELECT status FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
            .unwrap();
        if run != "queued" && run != "running" {
            return status;
        }
        assert!(std::time::Instant::now() < deadline, "run {run_id} is still '{run}'");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Every argv a fake logged to `log` (one JSON line each), oldest first.
fn logged_args(log: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn contains_run(args: &[String], run: &[&str]) -> bool {
    args.windows(run.len()).any(|w| w.iter().zip(run).all(|(a, b)| a == b))
}

/// Every event of `ty` on the task, oldest first, from `events --json`.
fn task_events(f: &Fixture, task_id: &str, ty: &str) -> Vec<Value> {
    json_ok(f, &["events", "--since", "0", "--task", task_id, "--json"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

/// Dispatches the claude-b fake with `scenario` and extra env; returns the
/// parsed `dispatch --json`.
fn dispatch_claude(f: &Fixture, task_id: &str, scenario: &str, env: &[(&str, &Path)]) -> Value {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .args(["dispatch", task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    for (k, v) in env {
        c.env(k, v);
    }
    serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap()
}

/// (a) the whole Claude path: web tools on the launch, sources through ingest,
/// `task result` and `review packet`.
#[test]
fn a_claude_research_task_gets_web_tools_and_its_sources_reach_result_and_packet() {
    let f = setup();
    let pages = tempfile::tempdir().unwrap();
    let (a, b) = (pages.path().join("a.txt"), pages.path().join("b.txt"));
    std::fs::write(&a, "Uploads retry three times.\n").unwrap();
    std::fs::write(&b, "Each attempt is logged.\n").unwrap();
    let (url_a, url_b) = (file_url(&a), file_url(&b));
    assert!(url_a.starts_with("file:///") && !url_a.starts_with("file:////"), "{url_a}");
    let sources = json!([
        {"url": url_a, "quote": "Uploads retry three times."},
        {"url": url_b, "quote": "Each attempt is logged."},
    ]);
    let log = f.home_path().join("claude-args.jsonl");

    let (task_id, _) = f.create_task(&spec("research"));
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_SOURCES", sources.to_string())
        .env("FAKE_CLAUDE_ARGS_LOG", &log)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &task_id, &run_id, &["in_review", "unverified"]), "in_review", "{}", f.explain_task(&task_id));

    let expected_tools = format!("{DEFAULT_TOOLS},WebFetch,WebSearch");
    assert_eq!(d["allowed_tools"], expected_tools.as_str(), "{d}");
    assert!(d["allowed_tools"].as_str().unwrap().ends_with(",WebFetch,WebSearch"));
    let status = json_ok(&f, &["status", &task_id, "--json"]);
    let runs = status["runs"].as_array().unwrap_or_else(|| panic!("no runs: {status}"));
    assert_eq!(runs[0]["allowed_tools"], expected_tools.as_str(), "{status}");

    let launches = logged_args(&log);
    let launch = launches.iter().find(|a| a.contains(&"--allowedTools".to_string())).expect("a worker launch");
    assert_eq!(arg_after(launch, "--allowedTools"), arg_after(launch, "--tools"), "{launch:?}");
    assert_eq!(arg_after(launch, "--allowedTools").as_deref(), Some(expected_tools.as_str()), "{launch:?}");
    assert!(launch.contains(&"--strict-mcp-config".to_string()), "{launch:?}");

    let r = json_ok(&f, &["task", "result", &task_id, "--json"]);
    let claims = r["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 3, "{r}");
    assert_eq!(claims[0]["kind"], "completion");
    assert_eq!(claims[0]["sources"], json!([]), "a claim without sources shows []");
    assert_eq!(claims[1]["kind"], "research");
    assert_eq!(claims[1]["text"], "research claim 1");
    assert_eq!(claims[1]["sources"], json!([sources[0]]), "{r}");
    assert_eq!(claims[2]["sources"], json!([sources[1]]), "{r}");

    let mut c = f.cmd();
    c.args(["task", "result", &task_id]);
    let text = String::from_utf8(f.run_ok(c, "task result")).unwrap();
    assert!(
        text.contains(&format!("- research: research claim 1 []\n    {url_a}  \"Uploads retry three times.\"")),
        "{text}"
    );

    let p = json_ok(&f, &["review", "packet", &task_id, "--json"]);
    let pc = p["claims"].as_array().unwrap();
    assert_eq!(pc[1]["sources"], json!([sources[0]]), "{p}");
    assert_eq!(pc[2]["sources"], json!([sources[1]]), "{p}");
    assert!(pc[0].get("sources").is_none() || pc[0]["sources"] == json!([]), "raw claims are copied: {p}");
    let mut c = f.cmd();
    c.args(["review", "packet", &task_id]);
    let md = String::from_utf8(f.run_ok(c, "review packet")).unwrap();
    assert!(md.contains(&format!("  - {url_a} — \"Uploads retry three times.\"")), "{md}");
    assert!(md.contains(&url_b), "{md}");
}

/// (b) and (g): the same fixture as a code task gets no web tool and no
/// research section; a research task's prompt has the section.
#[test]
fn a_code_task_gets_no_web_tools_and_no_research_section() {
    let f = setup();
    let log = f.home_path().join("claude-args.jsonl");
    let prompt = f.home_path().join("prompt-code.txt");
    let (task_id, _) = f.create_task(&spec("code"));
    let d = dispatch_claude(&f, &task_id, "ok", &[("FAKE_CLAUDE_ARGS_LOG", &log), ("FAKE_PROMPT_COPY", &prompt)]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &task_id, &run_id, &["in_review", "unverified"]), "in_review");
    assert_eq!(d["allowed_tools"], DEFAULT_TOOLS, "{d}");
    let status = json_ok(&f, &["status", &task_id, "--json"]);
    assert_eq!(status["runs"][0]["allowed_tools"], DEFAULT_TOOLS, "{status}");
    for launch in logged_args(&log) {
        assert!(!launch.iter().any(|a| a.contains("WebFetch") || a.contains("WebSearch")), "{launch:?}");
    }
    let code_prompt = std::fs::read_to_string(&prompt).unwrap();
    assert!(!code_prompt.contains("## Research claims"), "{code_prompt}");
    assert!(!code_prompt.contains(r#""kind":"research""#), "{code_prompt}");

    let research_prompt = f.home_path().join("prompt-research.txt");
    let (research, _) = f.create_task(&spec("research"));
    let d = dispatch_claude(&f, &research, "research_ok", &[("FAKE_PROMPT_COPY", &research_prompt)]);
    settle(&f, &research, d["run_id"].as_str().unwrap(), &["in_review", "unverified"]);
    let text = std::fs::read_to_string(&research_prompt).unwrap();
    assert!(text.contains("## Research claims"), "{text}");
    assert!(text.contains(r#""kind":"research""#), "{text}");
    let section = text.find("## Research claims").unwrap();
    let shape = text.find("`.agent/result.json` in the current directory with exactly this shape").unwrap();
    assert!(section < shape, "the section comes before the result.json shape");
}

/// (c) rule 7 and the empty-sources half of rule 1, from the fake's delivery.
#[test]
fn research_deliveries_without_sourced_claims_are_unverified() {
    let f = setup();
    let (task_id, _) = f.create_task(&spec("research"));
    let d = dispatch_claude(&f, &task_id, "research_unsourced", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &task_id, &run_id, &["in_review", "unverified"]), "unverified");
    let invalid = task_events(&f, &task_id, "result_invalid");
    let detail = &invalid.last().expect("a result_invalid event")["detail"];
    assert_eq!(detail["rule"], 7, "{detail}");
    assert!(detail["reason"].as_str().unwrap().contains("research"), "{detail}");

    let (task_id, _) = f.create_task(&spec("research"));
    let d = dispatch_claude(&f, &task_id, "research_no_sources", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &task_id, &run_id, &["in_review", "unverified"]), "unverified");
    let invalid = task_events(&f, &task_id, "result_invalid");
    let detail = &invalid.last().expect("a result_invalid event")["detail"];
    assert_eq!(detail["rule"], 1, "{detail}");
    assert!(detail["reason"].as_str().unwrap().contains("sources"), "{detail}");

    // a code task's delivery with no research claim is not rule 7's business
    let (code, _) = f.create_task(&spec("code"));
    let d = dispatch_claude(&f, &code, "research_unsourced", &[]);
    assert_eq!(settle(&f, &code, d["run_id"].as_str().unwrap(), &["in_review", "unverified"]), "in_review");
}

/// (d) the rest of rule 1, through `thoth submit -f`.
#[test]
fn submitted_research_results_with_a_bad_url_or_misplaced_sources_are_rule_1() {
    let f = setup();
    let (task_id, _) = f.create_task(&spec("research"));
    let d = dispatch_claude(&f, &task_id, "crash", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    settle(&f, &task_id, &run_id, &["interrupted"]);

    let submit = |name: &str, claims: Value| -> Value {
        let file: PathBuf = f.home_path().join(name);
        let body = json!({"task_id": task_id, "status": "completed", "summary": "by hand", "claims": claims});
        std::fs::write(&file, body.to_string()).unwrap();
        f.cmd().args(["submit", "-f"]).arg(&file).args(["--run", &run_id, "--json"]).assert().code(1);
        task_events(&f, &task_id, "result_invalid").pop().expect("a result_invalid event")["detail"].clone()
    };

    let detail = submit(
        "ftp.json",
        json!([{"kind": "research", "text": "X", "evidence": [], "sources": [{"url": "ftp://x", "quote": "q"}]}]),
    );
    assert_eq!(detail["rule"], 1, "{detail}");
    assert!(detail["reason"].as_str().unwrap().contains("url"), "{detail}");
    assert_eq!(f.task_status(&task_id), "unverified");

    let detail = submit(
        "misplaced.json",
        json!([
            {"kind": "completion", "text": "done", "sources": [{"url": "https://a.example", "quote": "q"}]},
            {"kind": "research", "text": "X", "sources": [{"url": "https://a.example", "quote": "q"}]},
        ]),
    );
    assert_eq!(detail["rule"], 1, "{detail}");
    assert!(detail["reason"].as_str().unwrap().contains("sources"), "{detail}");
}

/// (e) codex: the four network tokens after the sandbox pair for a research
/// implementer, only live web search for a source checker, none for code; a research run's
/// `web_search` item is expected, a code run's is not.
#[test]
fn a_codex_research_worker_has_its_network_opened_and_its_reviewer_does_not() {
    let f = setup();
    f.add_codex_profile("codex-a");
    let log = f.home_path().join("codex-args.jsonl");
    let dispatch = |task_id: &str, scenario: &str| -> Value {
        let mut c = f.cmd();
        c.env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", task_id)
            .env("FAKE_CODEX_ARGS_LOG", &log)
            .env("FAKE_CODEX_WEB_SEARCH", "1")
            .args(["dispatch", task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
        serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap()
    };
    let usage_of = |task_id: &str, run_id: &str| -> Value {
        task_events(&f, task_id, "run_usage")
            .into_iter()
            .rfind(|e| e["run_id"] == run_id)
            .expect("run_usage")["detail"]
            .clone()
    };

    let (research, _) = f.create_task(&spec("research"));
    let d = dispatch(&research, "research_ok");
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &research, &run_id, &["in_review", "unverified"]), "in_review", "{}", f.explain_task(&research));
    assert_eq!(d["allowed_tools"], DEFAULT_TOOLS, "{d}");
    let launches = logged_args(&log);
    let args = launches.iter().rfind(|a| a.first().map(String::as_str) == Some("exec")).unwrap();
    assert!(contains_run(args, &CODEX_NETWORK), "{args:?}");
    let sandbox = args.windows(2).position(|w| w == ["-s", "workspace-write"]).expect("the sandbox pair");
    let network = args.windows(4).position(|w| w == CODEX_NETWORK).unwrap();
    assert_eq!(network, sandbox + 2, "right after the sandbox pair: {args:?}");
    assert_eq!(usage_of(&research, &run_id)["unexpected_item_types"], json!([]), "web_search is expected");

    let (code, _) = f.create_task(&spec("code"));
    let before = logged_args(&log).len();
    let d = dispatch(&code, "ok");
    let code_run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(settle(&f, &code, &code_run, &["in_review", "unverified"]), "in_review");
    for args in &logged_args(&log)[before..] {
        assert!(!args.iter().any(|a| a.contains("network_access") || a.contains("web_search")), "{args:?}");
    }
    assert_eq!(usage_of(&code, &code_run)["unexpected_item_types"], json!(["web_search"]));

    let before = logged_args(&log).len();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &research)
        .env("FAKE_CODEX_ARGS_LOG", &log)
        .args(["review", "start", &research, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    let started: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    let rev = started["run_id"].as_str().unwrap().to_string();
    let mut c = f.cmd();
    c.args(["wait", "--run", &rev, "--timeout", "120s", "--json"]);
    f.run_ok(c, "wait --run");
    let reviews = logged_args(&log);
    let review_args = reviews[before..].iter().find(|a| a.first().map(String::as_str) == Some("exec")).expect("the reviewer launch");
    assert!(contains_run(review_args, &["-s", "read-only"]), "{review_args:?}");
    assert!(contains_run(review_args, &["-c", "web_search=\"live\""]), "{review_args:?}");
    assert!(!review_args.iter().any(|a| a.contains("network_access")), "{review_args:?}");
}

/// (f) agy: the policy file the hook reads lists the two web tools for a
/// research task, on top of the implementer list, and never a subagent tool.
#[test]
fn an_agy_research_worker_has_the_two_web_tools_in_its_policy() {
    let f = setup();
    let cfg = f.home_path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let dispatch = |task_id: &str, scenario: &str| -> Value {
        let mut c = f.cmd();
        c.env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", task_id)
            .env("FAKE_AGY_DATA_DIR", &cfg)
            .args(["dispatch", task_id, "--profile", "agy-a", "--model", AGY_MODEL, "--json"]);
        serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap()
    };
    let policy = |d: &Value| -> Vec<String> {
        let wt = PathBuf::from(d["worktree"].as_str().unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap()).unwrap();
        v["allowed_tools"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect()
    };

    let (research, _) = f.create_task(&spec("research"));
    let d = dispatch(&research, "research_ok");
    assert_eq!(settle(&f, &research, d["run_id"].as_str().unwrap(), &["in_review", "unverified"]), "in_review", "{}", f.explain_task(&research));
    let mut expected: Vec<String> = AGY_TOOLS.iter().map(|t| t.to_string()).collect();
    expected.extend(["search_web".to_string(), "read_url_content".to_string()]);
    assert_eq!(d["allowed_tools"], expected.join(","), "{d}");
    let listed = policy(&d);
    assert_eq!(listed, expected);
    for never in ["browser_subagent", "invoke_subagent", "call_mcp_tool"] {
        assert!(!listed.contains(&never.to_string()), "{listed:?}");
    }

    let (code, _) = f.create_task(&spec("code"));
    let d = dispatch(&code, "ok");
    settle(&f, &code, d["run_id"].as_str().unwrap(), &["in_review", "unverified"]);
    assert_eq!(d["allowed_tools"], AGY_TOOLS.join(","), "{d}");
    let listed = policy(&d);
    assert!(!listed.contains(&"search_web".to_string()) && !listed.contains(&"read_url_content".to_string()), "{listed:?}");
}
