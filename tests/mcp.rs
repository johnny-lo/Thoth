mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &common::Fixture, extra: &[&str]) -> Self {
        Self::start_scenario(f, extra, "ok")
    }

    fn start_scenario(f: &common::Fixture, extra: &[&str], scenario: &str) -> Self {
        Self::start_scenario_task(f, extra, scenario, None)
    }

    fn start_scenario_task(
        f: &common::Fixture,
        extra: &[&str],
        scenario: &str,
        task: Option<&str>,
    ) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .args(extra)
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("FAKE_SCENARIO", scenario)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(task) = task {
            c.env("FAKE_TASK_ID", task);
        } else {
            c.env_remove("FAKE_TASK_ID");
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    /// AGY-1: a server whose agy launches go to fake-agy with `data` as
    /// agy's data directory, logging argv to `log`.
    fn start_agy(f: &common::Fixture, scenario: &str, task: &str, data: &std::path::Path, log: &std::path::Path) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", task)
            .env("FAKE_AGY_DATA_DIR", data)
            .env("FAKE_AGY_ARGS_LOG", log)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
fn old_and_new_protocols_and_bad_input() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    let old=s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    assert_eq!(old["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(old["result"]["serverInfo"]["name"], "thoth");
    writeln!(s.input, "not json").unwrap();
    s.input.flush().unwrap();
    let mut line = String::new();
    s.output.read_line(&mut line).unwrap();
    let bad: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(bad["jsonrpc"], "2.0");
    assert_eq!(bad["error"]["code"], -32700);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let discover =
        s.send(&json!({"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":meta}}));
    assert_eq!(discover["result"]["resultType"], "complete");
    let list = s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 35);
    // Only the tool names are checked: thoth_checks_approve is the one that
    // approves, and none offers protect or unprotect.
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in &names {
        assert_eq!(name.contains("approve"), *name == "thoth_checks_approve", "{name}");
        assert!(!name.contains("protect"), "{name}");
    }
    for t in tools {
        for key in t["inputSchema"]["properties"].as_object().unwrap().keys() {
            assert!(!key.contains("approve"), "{key}");
            assert!(!key.contains("protect"), "{key}");
        }
    }
    // SR-3 added thoth_spec_review_confirm after thoth_spec_review_start, so
    // everything from thoth_resume on sits one place later.
    assert_eq!(
        names[23..],
        [
            "thoth_verify_start",
            "thoth_job_status",
            "thoth_checks_list",
            "thoth_checks_add",
            "thoth_checks_approve",
            "thoth_checks_remove",
            "thoth_config_list",
            "thoth_doctor",
            "thoth_spec_lint",
            "thoth_task_accept",
            "thoth_task_result",
            "thoth_guide"
        ]
    );
    assert_eq!(names[22], "thoth_cancel");
    assert_eq!(names[17], "thoth_spec_review_record");
    assert_eq!(names[18], "thoth_spec_review_show");
    assert_eq!(names[19], "thoth_spec_review_start");
    assert_eq!(names[20], "thoth_spec_review_confirm");
    let read_only = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"].clone()
    };
    assert_eq!(read_only("thoth_checks_list"), true);
    assert_eq!(read_only("thoth_job_status"), true);
    assert_eq!(read_only("thoth_verify_start"), false);
    assert_eq!(read_only("thoth_checks_add"), false);
    assert_eq!(read_only("thoth_checks_remove"), false);
    assert_eq!(read_only("thoth_checks_approve"), false);
    assert_eq!(read_only("thoth_task_result"), true);
    assert_eq!(read_only("thoth_task_accept"), false);
    let description = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let verify = description("thoth_verify_start");
    assert!(verify.contains("approved checks") && verify.contains("not the commands the worker declared"));
    assert!(verify.contains("thoth_job_status"));
    let add = description("thoth_checks_add");
    assert!(add.contains("pending") && add.contains("`thoth checks approve`"));
    assert!(add.contains("until it is approved with thoth_checks_approve"));
    let instructions = old["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("thoth_verify_start") && instructions.contains("thoth_job_status"));
    assert!(instructions.contains("The commander approves with thoth_checks_approve"));

    let fallback=s.send(&json!({"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}));
    assert_eq!(fallback["result"]["protocolVersion"], "2025-11-25");
    writeln!(
        s.input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    s.input.flush().unwrap();
    let ping = s.send(&json!({"jsonrpc":"2.0","id":5,"method":"ping","params":{}}));
    assert_eq!(ping["id"], 5);

    let new_meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let new_list =
        s.send(&json!({"jsonrpc":"2.0","id":6,"method":"tools/list","params":{"_meta":new_meta}}));
    assert_eq!(new_list["result"]["resultType"], "complete");
    assert!(discover["result"]["supportedVersions"]
        .as_array()
        .unwrap()
        .contains(&json!("2026-07-28")));
    let unsupported=s.send(&json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}));
    assert_eq!(unsupported["error"]["code"], -32022);
    assert!(unsupported["error"]["data"]["supported"]
        .as_array()
        .unwrap()
        .contains(&json!("2026-07-28")));
    let missing_caps=s.send(&json!({"jsonrpc":"2.0","id":8,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}));
    assert_eq!(missing_caps["error"]["code"], -32602);
    let unknown = s.send(&json!({"jsonrpc":"2.0","id":9,"method":"not/a/method","params":{}}));
    assert_eq!(unknown["error"]["code"], -32601);
}

#[test]
fn new_generation_list_results_carry_ttl_and_cache_scope() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let list =
        s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":meta}}));
    let r = &list["result"];
    assert!(r["ttlMs"].is_number());
    assert_eq!(r["ttlMs"].as_u64(), Some(0));
    assert_eq!(r["cacheScope"], "private");
    assert_eq!(r["resultType"], "complete");
    assert!(!r["tools"].as_array().unwrap().is_empty());
    let discover =
        s.send(&json!({"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":meta}}));
    let d = &discover["result"];
    assert!(d["ttlMs"].is_number());
    assert_eq!(d["ttlMs"].as_u64(), Some(0));
    assert_eq!(d["cacheScope"], "private");
    assert!(d["supportedVersions"].is_array());
    let call = s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"thoth_guide","arguments":{},"_meta":meta}}));
    assert_eq!(call["result"]["resultType"], "complete");
    assert!(call["result"].get("ttlMs").is_none());
    assert!(call["result"].get("cacheScope").is_none());
}

#[test]
fn old_generation_list_result_has_no_cache_fields() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}));
    let list = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    assert!(list["result"].get("ttlMs").is_none());
    assert!(list["result"].get("cacheScope").is_none());
    assert!(list["result"].get("resultType").is_none());
}

#[test]
fn invalid_utf8_is_a_parse_error_and_server_continues() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    s.input.write_all(&[0xff, b'\n']).unwrap();
    s.input.flush().unwrap();
    let mut line = String::new();
    s.output.read_line(&mut line).unwrap();
    let bad: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(bad["jsonrpc"], "2.0");
    assert_eq!(bad["error"]["code"], -32700);
    let ping = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"ping","params":{}}));
    assert_eq!(ping["result"], json!({}));
}

#[test]
fn new_generation_ping_is_validated_and_decorated() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    let ping = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"ping","params":{"_meta":meta}}));
    assert_eq!(ping["result"]["resultType"], "complete");
    assert_eq!(
        ping["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "thoth"
    );
    let unsupported = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"ping","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}));
    assert_eq!(unsupported["error"]["code"], -32022);
}

#[test]
fn negative_wait_timeout_is_rejected_as_that_parameter() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    let response = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"thoth_wait","arguments":{"task":"01NOTREAL","timeout_seconds":-1}}}));
    assert_eq!(response["result"]["isError"], true);
    assert!(response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("timeout_seconds"));
}

#[test]
fn init_and_status_are_cli_documents() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[]);
    let init=s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"thoth_init","arguments":{}}}));
    assert_eq!(init["result"]["isError"], false);
    assert_eq!(init["result"]["structuredContent"]["ok"], true);
    let status=s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"thoth_status","arguments":{}}}));
    assert_eq!(status["result"]["isError"], false);
    let rejected=s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"thoth_checks_approve","arguments":{}}}));
    assert_eq!(rejected["result"]["isError"], true);
    assert!(rejected["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("missing parameter reason"));
    let rejected = s.call(4, "thoth_checks_protect", json!({}));
    assert_eq!(rejected["error"]["code"], -32602);
    let rejected = s.call(5, "thoth_checks_unprotect", json!({}));
    assert_eq!(rejected["error"]["code"], -32602);
}

#[test]
fn worker_environment_refuses_before_stdout() {
    let f = common::fixture();
    let out = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .arg("mcp")
        .env("THOTH_HOME", f.home.path())
        .env("THOTH_RUN_ID", "run")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());
}

#[test]
fn eof_exits_cleanly_and_help_lists_mcp() {
    let f = common::fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .arg("mcp")
        .env("THOTH_HOME", f.home.path())
        .env_remove("THOTH_RUN_ID")
        .env_remove("THOTH_INBOX_DIR")
        .env_remove("THOTH_EXE")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let out = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("mcp"));
}

#[test]
fn mcp_only_commander_round_trip() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let mut s = Server::start_scenario(&f, &[], "ok");
    assert_eq!(
        s.call(1, "thoth_init", json!({}))["result"]["isError"],
        false
    );
    let spec = json!({
        "goal":"exercise MCP",
        "acceptance_criteria":["AC1: MCP round trip works"],
        "constraints":[], "non_goals":[], "excluded_alternatives":[], "risk":"low"
    });
    let created = s.call(
        2,
        "thoth_task_create",
        json!({"title":"MCP round trip","spec":spec}),
    );
    let task = created["result"]["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(s);
    let mut s = Server::start_scenario_task(&f, &[], "ok", Some(&task));
    let dispatched = s.call(
        3,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"medium"}),
    );
    assert_eq!(dispatched["result"]["isError"], false);
    let mut id = 4;
    loop {
        let waited = s.call(id, "thoth_wait", json!({"task":task,"timeout_seconds":5}));
        id += 1;
        assert_eq!(waited["result"]["isError"], false);
        if waited["result"]["structuredContent"]["status"] == "in_review" {
            break;
        }
        if waited["result"]["structuredContent"]["status"] == "unverified" {
            let events = s.call(id, "thoth_events", json!({"task":task}));
            panic!("worker became unverified: {events}");
        }
        assert_eq!(
            waited["result"]["structuredContent"]["timed_out"], true,
            "{waited}"
        );
    }
    let evidence = s.call(id, "thoth_evidence_list", json!({"task":task}));
    id += 1;
    let codes: Vec<String> = evidence["result"]["structuredContent"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap().to_owned())
        .collect();
    assert!(!codes.is_empty());
    let packet = s.call(id, "thoth_review_packet", json!({"task":task}));
    id += 1;
    let packet_path = packet["result"]["structuredContent"]["path"]
        .as_str()
        .unwrap();
    assert!(std::path::Path::new(packet_path).exists());
    assert!(std::fs::read_to_string(packet_path)
        .unwrap()
        .contains("AC1"));
    let review = json!({"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":[codes[0]]}],"extra_findings":[]}});
    let recorded = s.call(
        id,
        "thoth_review_record",
        json!({"task":task,"review":review}),
    );
    id += 1;
    assert_eq!(recorded["result"]["isError"], false, "{recorded}");
    let status = s.call(id, "thoth_status", json!({"task":task}));
    assert_eq!(status["result"]["structuredContent"]["status"], "pass");
    let tmp = f.home.path().join("mcp/tmp");
    assert!(tmp
        .read_dir()
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true));
}

#[test]
fn a_document_task_is_read_and_accepted_over_mcp() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let mut s = Server::start_scenario(&f, &[], "ok");
    assert_eq!(s.call(1, "thoth_init", json!({}))["result"]["isError"], false);
    let spec = json!({
        "goal":"write the spec",
        "acceptance_criteria":["AC1: the spec is written"],
        "constraints":[], "non_goals":[], "excluded_alternatives":[], "risk":"low",
        "task_type":"docs"
    });
    let task = s.call(2, "thoth_task_create", json!({"title":"doc","spec":spec}))["result"]
        ["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(s);
    let mut s = Server::start_scenario_task(&f, &[], "ok", Some(&task));
    let dispatched = s.call(
        3,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"medium"}),
    );
    assert_eq!(dispatched["result"]["isError"], false, "{dispatched}");
    f.wait_for_task_status(&task, &["in_review"], 120);

    let result = s.call(4, "thoth_task_result", json!({"task":task}));
    assert_eq!(result["result"]["isError"], false, "{result}");
    let doc = &result["result"]["structuredContent"];
    assert_eq!(doc["summary"], "fake worker completed the task", "{doc}");
    assert_eq!(doc["task_status"], "in_review");

    let accepted = s.call(5, "thoth_task_accept", json!({"task":task,"reason":"read it"}));
    assert_eq!(accepted["result"]["isError"], false, "{accepted}");
    assert_eq!(accepted["result"]["structuredContent"]["status"], "accepted");
    let status = s.call(6, "thoth_status", json!({}));
    assert_eq!(status["result"]["isError"], false, "{status}");
    let listed = status["result"]["structuredContent"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["task_id"] == task.as_str());
    assert!(!listed, "{status}");
}

#[test]
fn bounded_wait_errors_and_parameter_validation() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let mut s = Server::start_scenario(&f, &["--max-wait", "2s"], "slow");
    s.call(1, "thoth_init", json!({}));
    let spec = json!({"goal":"slow","acceptance_criteria":["AC1: stop"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"});
    let task = s.call(2, "thoth_task_create", json!({"title":"slow","spec":spec}))["result"]
        ["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let runs_before: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    let invalid = s.call(
        3,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","no_such_param":true}),
    );
    assert_eq!(invalid["result"]["isError"], true);
    let message = invalid["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(
        message.contains("accepted parameters")
            && message.contains("profile")
            && message.contains("repo")
    );
    let runs_after: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs_before, runs_after);
    assert_eq!(
        s.call(4, "thoth_status", json!({"task":"--help"}))["result"]["isError"],
        true
    );
    assert_eq!(
        s.call(5, "thoth_status", json!({"repo":"relative"}))["result"]["isError"],
        true
    );
    let dispatch = s.call(
        6,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"medium"}),
    );
    let run = dispatch["result"]["structuredContent"]["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let timed = s.call(7, "thoth_wait", json!({"run":run,"timeout_seconds":1}));
    assert_eq!(timed["result"]["isError"], false);
    assert_eq!(timed["result"]["structuredContent"]["timed_out"], true);
    let over = s.call(8, "thoth_wait", json!({"run":run,"timeout_seconds":3}));
    assert_eq!(over["result"]["isError"], true);
    let cancelled = s.call(9, "thoth_cancel", json!({"run":run}));
    assert_eq!(cancelled["result"]["isError"], false, "{cancelled}");
}

#[test]
fn cli_errors_are_forwarded_and_large_results_are_saved() {
    let f = common::fixture();
    let mut s = Server::start(&f, &["--max-result-bytes", "300"]);
    let uninitialised = s.call(1, "thoth_status", json!({}));
    assert_eq!(uninitialised["result"]["isError"], true);
    assert!(uninitialised["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("is not initialised"));
    s.call(2, "thoth_init", json!({}));
    let missing = s.call(3, "thoth_status", json!({"task":"01DOESNOTEXIST"}));
    assert_eq!(missing["result"]["isError"], true);
    assert_eq!(missing["result"]["structuredContent"]["exit_code"], 1);
    assert!(!missing["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .is_empty());
    let spec = json!({"goal":"make events","acceptance_criteria":["AC1: event exists"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"});
    s.call(
        4,
        "thoth_task_create",
        json!({"title":"large event one","spec":spec}),
    );
    s.call(
        5,
        "thoth_task_create",
        json!({"title":"large event two","spec":spec}),
    );
    let events = s.call(6, "thoth_events", json!({}));
    assert_eq!(events["result"]["isError"], false);
    assert_eq!(
        events["result"]["structuredContent"]["too_large"], true,
        "{events}"
    );
    let path = events["result"]["structuredContent"]["path"]
        .as_str()
        .unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(saved.is_object());
}

#[test]
fn dispatch_and_review_start_accept_an_effort_parameter() {
    let f = common::fixture();
    f.add_profile("claude-a");
    f.cmd().arg("init").assert().success();
    let (task, _) = f.create_task(common::Fixture::SPEC);
    let mut s = Server::start_scenario_task(&f, &[], "ok", Some(&task));
    let list = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    for name in ["thoth_dispatch", "thoth_review_start"] {
        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
        assert!(
            tool["inputSchema"]["properties"].get("effort").is_some(),
            "{name}: {tool}"
        );
    }
    let dispatched = s.call(
        2,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"high"}),
    );
    assert_eq!(dispatched["result"]["isError"], false, "{dispatched}");
    assert_eq!(
        dispatched["result"]["structuredContent"]["effort_requested"],
        "high",
        "{dispatched}"
    );
    // let the run finish before the temporary state directory goes away
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
}

// AGY-1 (AC2, AC4): the three launch tools and `thoth_wait` with an agy
// profile, through the server's plain CLI forwarding. ------------------------

/// A fixture with an agy profile; returns (fixture, agy data dir, argv log).
fn agy_fixture() -> (common::Fixture, std::path::PathBuf, std::path::PathBuf) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.home.path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let log = f.home_path().join("fake-agy-args.jsonl");
    (f, cfg, log)
}

/// Every `-p` launch fake-agy logged, oldest first.
fn agy_launches(log: &std::path::Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["args"][0] == "-p")
        .map(|v| v["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect())
        .collect()
}

fn has_run(args: &[String], seq: &[&str]) -> bool {
    args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

/// `thoth_wait` on a run until it is no longer timing out.
fn wait_run(s: &mut Server, id: &mut u64, run: &str) -> Value {
    for _ in 0..12 {
        let waited = s.call(*id, "thoth_wait", json!({"run": run, "timeout_seconds": 15}));
        *id += 1;
        assert_eq!(waited["result"]["isError"], false, "{waited}");
        if waited["result"]["structuredContent"]["timed_out"] == false {
            return waited["result"]["structuredContent"].clone();
        }
    }
    panic!("run {run} never finished");
}

#[test]
fn mcp_dispatch_passes_agy_effort_by_the_model_name_rule_and_wait_lists_unknown_tools() {
    let (f, cfg, log) = agy_fixture();
    // a carried level, left out: recorded, no flag; the run's unknown tool reaches thoth_wait
    let (task, _) = f.create_task(common::Fixture::SPEC);
    let mut s = Server::start_agy(&f, "unknown_tool", &task, &cfg, &log);
    let d = s.call(1, "thoth_dispatch", json!({"task": task, "profile": "agy-a", "model": "gemini-3.8-flash-high"}));
    assert_eq!(d["result"]["isError"], false, "{d}");
    let sc = &d["result"]["structuredContent"];
    assert_eq!(sc["effort"], "high", "{d}");
    assert_eq!(sc["effort_requested"], Value::Null, "{d}");
    assert!(sc["effort_note"].as_str().unwrap().contains("carried by the model variant"), "{d}");
    let run = sc["run_id"].as_str().unwrap().to_string();
    let mut id = 2;
    let w = wait_run(&mut s, &mut id, &run);
    assert_eq!(w["status"], "completed", "{w}");
    assert_eq!(w["tools_unknown"], json!(["frobnicate_file"]), "{w}");
    assert_eq!(w["task_status"], "in_review", "{w}");
    let all = agy_launches(&log);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(!all[0].iter().any(|a| a == "--effort"), "{:?}", all[0]);
    drop(s);

    // a bare name needs effort: left out is a tool error and no run; given, the flag is sent
    let (task2, _) = f.create_task(common::Fixture::SPEC);
    let mut s = Server::start_agy(&f, "ok", &task2, &cfg, &log);
    let refused = s.call(1, "thoth_dispatch", json!({"task": task2, "profile": "agy-a", "model": "claude-sonnet-4-6"}));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused["result"]["structuredContent"]["error"].as_str().unwrap().contains("--effort is missing"), "{refused}");
    let status = s.call(2, "thoth_status", json!({"task": task2}));
    assert_eq!(status["result"]["structuredContent"]["runs"].as_array().map(Vec::len), Some(0), "{status}");
    let d = s.call(3, "thoth_dispatch", json!({"task": task2, "profile": "agy-a", "model": "claude-sonnet-4-6", "effort": "medium"}));
    assert_eq!(d["result"]["isError"], false, "{d}");
    assert_eq!(d["result"]["structuredContent"]["effort"], "medium", "{d}");
    let run2 = d["result"]["structuredContent"]["run_id"].as_str().unwrap().to_string();
    let mut id = 4;
    let w = wait_run(&mut s, &mut id, &run2);
    assert_eq!(w["status"], "completed", "{w}");
    assert_eq!(w["tools_unknown"], json!([]), "{w}");
    let all = agy_launches(&log);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(has_run(&all[1], &["--model", "claude-sonnet-4-6", "--effort", "medium", "--dangerously-skip-permissions"]), "{:?}", all[1]);
}

#[test]
fn mcp_review_start_and_spec_review_start_follow_the_agy_effort_rule() {
    let (f, cfg, log) = agy_fixture();
    f.add_profile("claude-b");
    // a candidate from a Claude implementer, reviewed by agy with a carried level
    let (task, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    f.dispatch(&task, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    let mut s = Server::start_agy(&f, "review_pass", &task, &cfg, &log);
    let r = s.call(1, "thoth_review_start", json!({"task": task, "profile": "agy-a", "model": "gemini-3.8-flash-low"}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["effort"], "low", "{r}");
    assert_eq!(sc["effort_requested"], Value::Null, "{r}");
    assert!(sc["effort_note"].as_str().unwrap().contains("carried by the model variant gemini-3.8-flash-low"), "{r}");
    let reviewer = sc["run_id"].as_str().unwrap().to_string();
    let mut id = 2;
    let w = wait_run(&mut s, &mut id, &reviewer);
    assert_eq!(w["verdict"], "PASS", "{w}");
    assert_eq!(w["tools_unknown"], json!([]), "{w}");
    let all = agy_launches(&log);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(!all[0].iter().any(|a| a == "--effort"), "{:?}", all[0]);
    drop(s);

    // a spec review on a bare name: refused without effort, sent with it
    let (task2, _) = f.create_task(common::Fixture::SPEC);
    let mut s = Server::start_agy(&f, "spec_review_approve", &task2, &cfg, &log);
    let refused = s.call(1, "thoth_spec_review_start", json!({"task": task2, "profile": "agy-a", "model": "claude-sonnet-4-6"}));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused["result"]["structuredContent"]["error"].as_str().unwrap().contains("--effort is missing"), "{refused}");
    let status = s.call(2, "thoth_status", json!({"task": task2}));
    assert_eq!(status["result"]["structuredContent"]["runs"].as_array().map(Vec::len), Some(0), "{status}");
    let started = s.call(3, "thoth_spec_review_start", json!({"task": task2, "profile": "agy-a", "model": "claude-sonnet-4-6", "effort": "xhigh"}));
    assert_eq!(started["result"]["isError"], false, "{started}");
    assert_eq!(started["result"]["structuredContent"]["effort"], "xhigh", "{started}");
    assert_eq!(started["result"]["structuredContent"]["effort_requested"], "xhigh", "{started}");
    let run = started["result"]["structuredContent"]["run_id"].as_str().unwrap().to_string();
    let mut id = 4;
    let w = wait_run(&mut s, &mut id, &run);
    assert_eq!(w["verdict"], "APPROVE", "{w}");
    let all = agy_launches(&log);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(has_run(&all[1], &["--model", "claude-sonnet-4-6", "--effort", "xhigh", "--dangerously-skip-permissions"]), "{:?}", all[1]);
}

#[test]
fn checks_add_takes_exe_pin_and_list_reports_it() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut s = Server::start(&f, &[]);
    let added = s.call(1, "thoth_checks_add", json!({"id":"gitv","cmd":"git --version","exe_pin":"path"}));
    assert_eq!(added["result"]["isError"], false, "{added}");
    assert_eq!(added["result"]["structuredContent"]["exe_pin"], "path", "{added}");
    assert_eq!(added["result"]["structuredContent"]["status"], "pending", "{added}");
    let list = s.call(2, "thoth_checks_list", json!({}));
    let checks = list["result"]["structuredContent"]["checks"].as_array().unwrap().clone();
    assert_eq!(checks.len(), 1, "{list}");
    assert_eq!(checks[0]["exe_pin"], "path", "{list}");
    // deliberately wrong: an unknown value, and a program that does not resolve
    let bad = s.call(3, "thoth_checks_add", json!({"id":"bad","cmd":"git --version","exe_pin":"bytes"}));
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(
        bad["result"]["structuredContent"]["error"].as_str().unwrap().contains("--exe-pin must be hash or path"),
        "{bad}"
    );
    let bad = s.call(4, "thoth_checks_add", json!({"id":"bad","cmd":"nosuchexe-zz","exe_pin":"path"}));
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    assert!(bad["result"]["structuredContent"]["error"].as_str().unwrap().contains("does not resolve"), "{bad}");
    let list = s.call(5, "thoth_checks_list", json!({}));
    assert_eq!(list["result"]["structuredContent"]["checks"].as_array().unwrap().len(), 1, "{list}");
}

// DT-2 (T8), RJ-1: `reason` on the two launch tools, and the require-reason
// gate, which asks for a reason on every launch.

fn require_reason_on(f: &common::Fixture) {
    f.cmd()
        .args(["config", "set", "require-reason", "on", "--json"])
        .assert()
        .success();
}

/// The detail of the run's `launch_choice` event.
fn launch_choice_detail(f: &common::Fixture, run_id: &str) -> Value {
    let text: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'launch_choice'",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn reason_is_a_launch_tool_parameter_and_require_reason_refuses_without_it() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task, _) = f.create_task(common::Fixture::SPEC);
    require_reason_on(&f);
    let mut s = Server::start_scenario_task(&f, &[], "ok", Some(&task));
    let list = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    for name in ["thoth_dispatch", "thoth_review_start"] {
        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
        assert!(
            tool["inputSchema"]["properties"].get("reason").is_some(),
            "{name}: {tool}"
        );
    }
    let refused = s.call(
        2,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"medium"}),
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(refused["result"]["structuredContent"]["exit_code"], 1, "{refused}");
    assert!(
        refused["result"]["structuredContent"]["error"].as_str().unwrap().contains("--reason"),
        "{refused}"
    );
    let dispatched = s.call(
        3,
        "thoth_dispatch",
        json!({"task":task,"profile":"claude-a","model":"sonnet","effort":"medium","reason":"via mcp"}),
    );
    assert_eq!(dispatched["result"]["isError"], false, "{dispatched}");
    let run_id = dispatched["result"]["structuredContent"]["run_id"].as_str().unwrap().to_string();
    let detail = launch_choice_detail(&f, &run_id);
    assert_eq!(detail["reason"], "via mcp", "{detail}");
    // let the run finish before the temporary state directory goes away
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
}

/// BASE-1 AC3: `thoth_task_create` takes `base`; without it the task starts at
/// the HEAD of the worktree the server runs the command in.
#[test]
fn task_create_starts_at_base_or_at_the_head_of_the_repo_worktree() {
    let f = common::fixture();
    let repo = f.repo_path();
    f.cmd().arg("init").assert().success();
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("wt");
    common::git(&repo, &["worktree", "add", "-b", "long", wt.to_str().unwrap()]);
    std::fs::write(wt.join("long.txt"), "only on long\n").unwrap();
    common::git(&wt, &["add", "long.txt"]);
    common::git(&wt, &["commit", "-m", "long only"]);
    let wt_head = common::git(&wt, &["rev-parse", "HEAD"]);
    let main_head = common::git(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(wt_head, main_head);

    let mut s = Server::start(&f, &["--default-repo", wt.to_str().unwrap()]);
    let list = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "thoth_task_create")
        .unwrap()
        .clone();
    assert_eq!(tool["inputSchema"]["properties"]["base"], json!({"type":"string"}));
    assert!(!tool["inputSchema"]["required"].as_array().unwrap().contains(&json!("base")));

    let spec = json!({"goal":"start somewhere","acceptance_criteria":["AC1: starts"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"});
    let base_of = |v: &Value| {
        assert_eq!(v["result"]["isError"], false, "{v}");
        v["result"]["structuredContent"]["base_commit"].as_str().unwrap().to_string()
    };
    let v = s.call(2, "thoth_task_create", json!({"title":"wt","spec":spec}));
    assert_eq!(base_of(&v), wt_head);
    let v = s.call(3, "thoth_task_create", json!({"title":"main","spec":spec,"base":"main"}));
    assert_eq!(base_of(&v), main_head);
    let repo_abs = repo.to_str().unwrap();
    let v = s.call(4, "thoth_task_create", json!({"title":"repo","spec":spec,"repo":repo_abs}));
    assert_eq!(base_of(&v), main_head);

    let count = |s: &mut Server, id: u64| {
        let status = s.call(id, "thoth_status", json!({}));
        assert_eq!(status["result"]["isError"], false, "{status}");
        status["result"]["structuredContent"]["tasks"].as_array().unwrap().len()
    };
    let before = count(&mut s, 5);
    assert_eq!(before, 3);
    for (id, bad) in [(6, "nosuch"), (7, "-x")] {
        let v = s.call(id, "thoth_task_create", json!({"title":"bad","spec":spec,"base":bad}));
        assert_eq!(v["result"]["isError"], true, "{bad}: {v}");
    }
    assert_eq!(count(&mut s, 8), before);
}
