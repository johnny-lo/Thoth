//! RES-1b: source checking through CLI output, the three offline harnesses,
//! their argv logs and the files they exchange with Thoth.
mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const TOOLS: &str = "Bash,Read,Glob,Grep";
const CODEX: &str = "gpt-5.6-luna";
const AGY: &str = "gemini-3.8-flash-low";
const SPEC: &str = r#"{"goal":"research retries","task_type":"research","acceptance_criteria":["AC1: report retry behaviour"],"constraints":[],"non_goals":[],"excluded_alternatives":[]}"#;
/// `AGY_REVIEWER_TOOLS` (AGY-1: `read_file` beside `view_file`).
const AGY_TOOLS: &[&str] = &[
    "read_file",
    "view_file",
    "list_dir",
    "find_by_name",
    "grep_search",
    "finish",
    "wait",
    "wait_5_seconds",
];

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    f
}

fn json_ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    serde_json::from_slice(&f.run_ok(c, &args.join(" "))).unwrap()
}

fn wait(f: &Fixture, run: &str) -> Value {
    json_ok(f, &["wait", "--run", run, "--timeout", "120s", "--json"])
}

fn events(f: &Fixture, task: &str, ty: &str) -> Vec<Value> {
    json_ok(f, &["events", "--since", "0", "--task", task, "--json"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == ty)
        .cloned()
        .collect()
}

fn file_url(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    let s = s
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

fn pages(f: &Fixture, count: usize) -> (Vec<PathBuf>, Value) {
    let paths: Vec<_> = (1..=count)
        .map(|i| f.home_path().join(format!("page {i} #%.txt")))
        .collect();
    for (i, p) in paths.iter().enumerate() {
        std::fs::write(p, format!("prefix\nquote {}\nsuffix", i + 1)).unwrap();
    }
    let sources = json!(paths
        .iter()
        .enumerate()
        .map(|(i, p)| json!({"url":file_url(p), "quote":format!("quote {}", i+1)}))
        .collect::<Vec<_>>());
    (paths, sources)
}

fn candidate(f: &Fixture, sources: &Value) -> (String, Value) {
    let (task, _) = f.create_task(SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_SOURCES", sources.to_string())
        .args([
            "dispatch",
            &task,
            "--profile",
            "claude-b",
            "--model",
            "sonnet",
            "--effort",
            "medium",
            "--json",
        ]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch research")).unwrap();
    assert_eq!(
        wait(f, d["run_id"].as_str().unwrap())["task_status"],
        "in_review"
    );
    (task, d)
}

fn start(f: &Fixture, task: &str, profile: &str, scenario: &str, env: &[(&str, &str)]) -> Value {
    let model = match profile {
        "codex-a" => CODEX,
        "agy-a" => AGY,
        _ => "sonnet",
    };
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .args([
            "review",
            "start",
            task,
            "--profile",
            profile,
            "--model",
            model,
            "--effort",
            "medium",
            "--json",
        ]);
    for (k, v) in env {
        c.env(k, v);
    }
    serde_json::from_slice(&f.run_ok(c, "review start")).unwrap()
}

fn argv(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .rfind(|v| {
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "exec" || a == "-p")
        })
        .unwrap()["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn pair(args: &[String], a: &str, b: &str) -> bool {
    args.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[test]
fn codex_source_check_pass_records_sources_claims_events_and_read_only_web_args() {
    let f = setup();
    let (_, sources) = pages(&f, 2);
    let (task, d) = candidate(&f, &sources);
    let log = f.home_path().join("codex-args.jsonl");
    let prompt = f.home_path().join("prompt.txt");
    let r = start(
        &f,
        &task,
        "codex-a",
        "source_check",
        &[
            ("FAKE_CODEX_ARGS_LOG", log.to_str().unwrap()),
            ("FAKE_PROMPT_COPY", prompt.to_str().unwrap()),
            ("FAKE_CODEX_WEB_SEARCH", "1"),
        ],
    );
    assert_eq!(r["target_run_id"], d["run_id"]);
    assert_eq!(r["allowed_tools"], TOOLS);
    let w = wait(&f, r["run_id"].as_str().unwrap());
    assert_eq!(w["verdict"], "PASS", "{w}");
    assert_eq!(w["task_status"], "pass");
    let args = argv(&log);
    let sandbox = args
        .windows(2)
        .position(|w| w == ["-s", "read-only"])
        .unwrap();
    assert_eq!(
        &args[sandbox + 2..sandbox + 4],
        ["-c", "web_search=\"live\""]
    );
    assert!(!args.iter().any(|a| a.contains("network_access")));
    assert!(args.contains(&"--output-schema".to_string()));
    let show = json_ok(&f, &["review", "show", &task, "--json"]);
    assert_eq!(show["verdict"], "PASS");
    assert_eq!(show["findings"]["sources"].as_array().unwrap().len(), 2);
    for s in show["findings"]["sources"].as_array().unwrap() {
        assert_eq!(s["verdict"], "supported");
    }
    let completion = show["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "completion")
        .unwrap();
    assert_eq!(completion["sources"], json!([]));
    for (i, source) in sources.as_array().unwrap().iter().enumerate() {
        let claim = show["claims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["text"] == format!("research claim {}", i + 1))
            .unwrap();
        assert_eq!(claim["sources"], json!([source]));
    }
    let detail = &events(&f, &task, "verdict")[0]["detail"];
    assert_eq!(detail["sources"], show["findings"]["sources"]);
    assert_eq!(detail["criteria"], json!([]));
    assert_eq!(
        events(&f, &task, "review_started")[0]["detail"]["allowed_tools"],
        TOOLS
    );
    let usage = events(&f, &task, "run_usage");
    assert_eq!(
        usage.last().unwrap()["detail"]["unexpected_item_types"],
        json!([])
    );
    let final_event = events(&f, &task, "status_change").into_iter()
        .rfind(|e| e["run_id"] == r["run_id"] && e["detail"]["entity"] == "run")
        .unwrap();
    assert!(final_event["detail"]["note"].as_str().unwrap().contains("PASS over 2 sources"));
    let text = std::fs::read_to_string(prompt).unwrap();
    assert!(!text.contains("## Evidence") && !text.contains("## Rules"));
    let source_line = text
        .lines()
        .skip_while(|l| *l != "## Sources to check")
        .nth(1)
        .unwrap();
    let listed: Value = serde_json::from_str(source_line).unwrap();
    assert_eq!(listed[0]["claim"], 2);
    assert_eq!(listed[1]["claim"], 3);
    assert_eq!(listed[0]["id"], "S1");
    assert_eq!(listed[1]["url"], sources[1]["url"]);
    let mut cmd = f.cmd();
    cmd.args(["review", "show", &task]);
    let human = String::from_utf8(f.run_ok(cmd, "review show")).unwrap();
    assert!(human.contains(&format!(
        "  S1  supported  {}",
        sources[0]["url"].as_str().unwrap()
    )));
}

fn reject_and_rework(count: usize) {
    let f = setup();
    let (paths, sources) = pages(&f, count);
    let (task, _) = candidate(&f, &sources);
    std::fs::write(&paths[1], "different page").unwrap();
    if count == 3 {
        std::fs::remove_file(&paths[2]).unwrap();
    }
    let r = start(&f, &task, "codex-a", "source_check", &[]);
    let w = wait(&f, r["run_id"].as_str().unwrap());
    assert_eq!(w["verdict"], "REJECT");
    assert_eq!(w["task_status"], "reject");
    let prompt = f.home_path().join("rework.txt");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_SOURCES", sources.to_string())
        .env("FAKE_PROMPT_COPY", &prompt)
        .args([
            "dispatch",
            &task,
            "--profile",
            "claude-b",
            "--model",
            "sonnet",
            "--effort",
            "medium",
            "--json",
        ]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "rework dispatch")).unwrap();
    assert_eq!(d["folded_findings"], count - 1);
    assert_eq!(
        wait(&f, d["run_id"].as_str().unwrap())["task_status"],
        "in_review"
    );
    let text = std::fs::read_to_string(prompt).unwrap();
    assert!(
        text.contains(sources[1]["url"].as_str().unwrap())
            && text.contains("quote not found in page")
    );
    assert!(text.contains("claim 3 (S2): research claim 2"));
    assert!(!text.contains(sources[0]["url"].as_str().unwrap()));
    if count == 3 {
        assert!(text.contains(sources[2]["url"].as_str().unwrap()));
        assert!(text.contains("unreachable:") && text.contains("replace the source"));
    }
}

#[test]
fn unsupported_source_rejects_and_rework_names_the_claim_and_source() {
    reject_and_rework(2);
}

#[test]
fn rejection_also_tells_rework_to_replace_unreachable_sources() {
    reject_and_rework(3);
}

#[test]
fn unreachable_source_without_unsupported_source_is_unverified() {
    let f = setup();
    let (paths, sources) = pages(&f, 2);
    let (task, _) = candidate(&f, &sources);
    std::fs::remove_file(&paths[1]).unwrap();
    let r = start(&f, &task, "codex-a", "source_check", &[]);
    let w = wait(&f, r["run_id"].as_str().unwrap());
    assert_eq!(w["verdict"], "UNVERIFIED");
    assert_eq!(w["task_status"], "unverified");
    let show = json_ok(&f, &["review", "show", &task, "--json"]);
    assert_eq!(show["findings"]["sources"][1]["verdict"], "unreachable");
    assert!(!show["findings"]["sources"][1]["reason"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[test]
fn malformed_and_inconsistent_source_checks_are_invalid() {
    let f = setup();
    let (paths, sources) = pages(&f, 2);
    for profile in ["codex-a", "claude-a", "agy-a"] {
        let cfg = add_agy(&f);
        let (task, _) = candidate(&f, &sources);
        let r = start(
            &f,
            &task,
            profile,
            "source_check_bad",
            &[("FAKE_AGY_DATA_DIR", cfg.to_str().unwrap())],
        );
        let w = wait(&f, r["run_id"].as_str().unwrap());
        assert_eq!(w["task_status"], "unverified", "{profile}: {w}");
        let invalid = events(&f, &task, "review_invalid");
        assert!(["read", "schema"]
            .contains(&invalid.last().unwrap()["detail"]["stage"].as_str().unwrap()));
        assert_eq!(
            json_ok(&f, &["review", "show", &task, "--json"])["review_count"],
            0
        );
    }
    let (task, _) = candidate(&f, &sources);
    std::fs::write(&paths[1], "no quote").unwrap();
    let r = start(
        &f,
        &task,
        "codex-a",
        "source_check",
        &[("FAKE_SOURCE_CHECK_VERDICT", "PASS")],
    );
    assert_eq!(
        wait(&f, r["run_id"].as_str().unwrap())["task_status"],
        "unverified"
    );
    assert_eq!(
        events(&f, &task, "review_invalid").last().unwrap()["detail"]["stage"],
        "consistency"
    );
}

#[test]
fn same_provider_source_checker_uses_existing_reason_gate_and_claude_web_tools() {
    let f = setup();
    let (_, sources) = pages(&f, 2);
    let (task, _) = candidate(&f, &sources);
    let log = f.home_path().join("claude-args.jsonl");
    let r = start(
        &f,
        &task,
        "claude-a",
        "source_check",
        &[("FAKE_CLAUDE_ARGS_LOG", log.to_str().unwrap())],
    );
    let tools = format!("{TOOLS},WebFetch,WebSearch");
    assert_eq!(r["allowed_tools"], tools);
    assert_eq!(wait(&f, r["run_id"].as_str().unwrap())["verdict"], "PASS");
    assert!(pair(&argv(&log), "--tools", &tools));
    assert_eq!(
        events(&f, &task, "review_started")[0]["detail"]["allowed_tools"],
        tools
    );
}

#[test]
fn latest_blocked_delivery_without_research_claim_refuses_before_any_write() {
    let f = setup();
    let (_, sources) = pages(&f, 2);
    let (task, d) = candidate(&f, &sources);
    let file = f.home_path().join("blocked.json");
    std::fs::write(
        &file,
        json!({"task_id":task,"status":"blocked","summary":"blocked",
        "claims":[{"kind":"completion","text":"blocked", "evidence":[]}]})
        .to_string(),
    )
    .unwrap();
    f.cmd()
        .args(["submit", "-f"])
        .arg(&file)
        .args(["--run", d["run_id"].as_str().unwrap()])
        .assert()
        .success();
    let before = json_ok(&f, &["events", "--since", "0", "--task", &task, "--json"]);
    let out = f
        .cmd()
        .args([
            "review",
            "start",
            &task,
            "--profile",
            "codex-a",
            "--model",
            CODEX,
            "--effort",
            "medium",
        ])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let error = String::from_utf8(out).unwrap();
    for phrase in ["research claim", "task result", "task close", "dispatch"] {
        assert!(error.contains(phrase), "{error}");
    }
    assert_eq!(
        before,
        json_ok(&f, &["events", "--since", "0", "--task", &task, "--json"])
    );
    let review_worktree = PathBuf::from(format!("{}-review", d["worktree"].as_str().unwrap()));
    assert!(!review_worktree.exists(), "{}", review_worktree.display());
}

fn check_doc(sources: &Value) -> Value {
    json!({"verdict":"PASS", "findings":{"sources":sources.as_array().unwrap().iter().enumerate().map(|(i,s)|
        json!({"id":format!("S{}", i+1),"url":s["url"],"verdict":"supported","reason":""})).collect::<Vec<_>>(),"extra_findings":[]}})
}

#[test]
fn external_source_check_records_pass_and_refuses_wrong_urls_criteria_coverage_and_citations() {
    let f = setup();
    let (_, sources) = pages(&f, 2);
    let (task, _) = candidate(&f, &sources);
    let file = f.home_path().join("check.json");
    let base = check_doc(&sources);
    let mut variants = Vec::new();
    let mut v = base.clone();
    v["findings"]["sources"][0]["url"] = json!("file:///wrong");
    variants.push((v, "url", "schema"));
    let mut v = base.clone();
    v["findings"]["criteria"] = json!([{"id":"AC1", "verdict":"met", "evidence":[]}]);
    variants.push((v, "criteria", "schema"));
    let mut v = base.clone();
    v["findings"]["sources"].as_array_mut().unwrap().pop();
    variants.push((v, "S2", "coverage"));
    let mut v = base.clone();
    let duplicate = v["findings"]["sources"][0].clone();
    v["findings"]["sources"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    variants.push((v, "more than once", "coverage"));
    let mut v = base.clone();
    let mut unknown = v["findings"]["sources"][0].clone();
    unknown["id"] = json!("S3");
    v["findings"]["sources"]
        .as_array_mut()
        .unwrap()
        .push(unknown);
    variants.push((v, "unknown source", "coverage"));
    let mut v = base.clone();
    v["findings"]["extra_findings"] = json!([{"note":"n", "evidence":["E999999"]}]);
    variants.push((v, "E999999", "citation"));
    for (v, message, stage) in variants {
        std::fs::write(&file, v.to_string()).unwrap();
        let out = f
            .cmd()
            .args(["review", "record", &task, "-f"])
            .arg(&file)
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        assert!(String::from_utf8(out).unwrap().contains(message));
        assert_eq!(
            events(&f, &task, "review_invalid").last().unwrap()["detail"]["stage"],
            stage
        );
        assert_eq!(
            json_ok(&f, &["review", "show", &task, "--json"])["review_count"],
            0
        );
    }
    std::fs::write(&file, base.to_string()).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", &task, "-f"])
        .arg(&file)
        .arg("--json");
    let r: Value = serde_json::from_slice(&f.run_ok(c, "review record")).unwrap();
    assert_eq!(r["task_status"], "pass");
    let show = json_ok(&f, &["review", "show", &task, "--json"]);
    assert_eq!(show["review_count"], 1);
    assert_eq!(show["source"], "external");
    assert_eq!(show["findings"]["sources"], base["findings"]["sources"]);
    assert_eq!(
        events(&f, &task, "verdict")[0]["detail"]["sources"],
        base["findings"]["sources"]
    );
}

fn add_agy(f: &Fixture) -> PathBuf {
    let cfg = f.home_path().join("profiles").join("agy-a");
    if !cfg.exists() {
        std::fs::create_dir_all(&cfg).unwrap();
        f.cmd()
            .args([
                "profile",
                "add",
                "agy-a",
                "--provider",
                "google",
                "--harness",
                "antigravity",
                "--config-dir",
            ])
            .arg(&cfg)
            .assert()
            .success();
    }
    cfg
}

#[test]
fn agy_source_checker_gets_only_read_and_web_tools_and_structured_schema() {
    let f = setup();
    let cfg = add_agy(&f);
    let (_, sources) = pages(&f, 2);
    let (task, _) = candidate(&f, &sources);
    let log = f.home_path().join("agy-args.jsonl");
    let r = start(
        &f,
        &task,
        "agy-a",
        "source_check",
        &[
            ("FAKE_AGY_DATA_DIR", cfg.to_str().unwrap()),
            ("FAKE_AGY_ARGS_LOG", log.to_str().unwrap()),
        ],
    );
    assert_eq!(wait(&f, r["run_id"].as_str().unwrap())["verdict"], "PASS");
    let mut tools: Vec<_> = AGY_TOOLS.iter().map(|s| s.to_string()).collect();
    tools.extend(["search_web".into(), "read_url_content".into()]);
    assert_eq!(r["allowed_tools"], tools.join(","));
    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(r["worktree"].as_str().unwrap()).join(".agent/agy-hook.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(policy["allowed_tools"], json!(tools));
    assert!(argv(&log).contains(&"--json-schema".to_string()));
}

#[test]
fn research_spec_review_keeps_network_off_and_code_review_keeps_original_tools() {
    let f = setup();
    let (research, _) = f.create_task(SPEC);
    let log = f.home_path().join("spec-args.jsonl");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &research)
        .env("FAKE_CODEX_ARGS_LOG", &log)
        .args([
            "spec",
            "review",
            "start",
            &research,
            "--profile",
            "codex-a",
            "--model",
            CODEX,
            "--effort",
            "medium",
            "--json",
        ]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "spec review start")).unwrap();
    assert_eq!(r["allowed_tools"], TOOLS);
    wait(&f, r["run_id"].as_str().unwrap());
    assert!(!argv(&log)
        .iter()
        .any(|a| a.contains("web_search") || a.contains("network_access")));
    let (task, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .args([
            "dispatch",
            &task,
            "--profile",
            "claude-b",
            "--model",
            "sonnet",
            "--effort",
            "medium",
            "--json",
        ]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch code")).unwrap();
    assert_eq!(
        wait(&f, d["run_id"].as_str().unwrap())["task_status"],
        "in_review"
    );
    let r = start(&f, &task, "claude-a", "review_pass", &[]);
    assert_eq!(r["allowed_tools"], TOOLS);
    assert_eq!(wait(&f, r["run_id"].as_str().unwrap())["verdict"], "PASS");
}
