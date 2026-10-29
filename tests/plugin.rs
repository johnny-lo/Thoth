use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

const MARKETPLACE: &str = ".claude-plugin/marketplace.json";
const MANIFEST: &str = "plugins/thoth/.claude-plugin/plugin.json";
const MCP: &str = "plugins/thoth/.mcp.json";
const SKILL: &str = "plugins/thoth/skills/thoth-dispatch/SKILL.md";

fn read_json(path: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

#[test]
fn ac1_marketplace_has_only_the_public_plugin_fields() {
    let value = read_json(MARKETPLACE);
    assert_eq!(keys(&value), BTreeSet::from(["name", "owner", "plugins"]));
    assert_eq!(value["name"], "thoth");
    assert_eq!(value["owner"], json!({"name":"Thoth contributors"}));
    let plugins = value["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        keys(&plugins[0]),
        BTreeSet::from(["description", "name", "source"])
    );
    assert_eq!(plugins[0]["name"], "thoth");
    assert_eq!(plugins[0]["source"], "./plugins/thoth");
    let description = plugins[0]["description"].as_str().unwrap();
    assert!(!description.trim().is_empty());
    assert_eq!(description.matches('.').count(), 1);
    assert!(description.ends_with('.'));
}

#[test]
fn ac2_manifest_matches_package_and_omits_unsettled_metadata() {
    let value = read_json(MANIFEST);
    assert_eq!(
        keys(&value),
        BTreeSet::from([
            "author",
            "description",
            "name",
            "types",
            "userConfig",
            "version"
        ])
    );
    assert_eq!(value["types"], "./types/index.d.ts");
    assert_eq!(
        keys(&value["userConfig"]),
        BTreeSet::from(["language", "thoth_bin", "thoth_home"])
    );
    assert_eq!(value["userConfig"]["language"]["default"], "en");
    assert_eq!(value["name"], "thoth");
    assert_eq!(value["name"], read_json(MARKETPLACE)["plugins"][0]["name"]);
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(!value["description"].as_str().unwrap().is_empty());
    assert_eq!(value["author"], json!({"name":"Thoth contributors"}));
    for omitted in ["homepage", "repository", "license"] {
        assert!(value.get(omitted).is_none());
    }
}

#[test]
fn ac3_mcp_configuration_is_exact_and_portable() {
    assert_eq!(
        read_json(MCP),
        json!({"mcpServers":{"thoth":{"command":"thoth","args":["mcp","--max-wait","110s","--default-repo","${CLAUDE_PROJECT_DIR}"]}}})
    );
}

fn frontmatter(markdown: &str) -> Map<String, Value> {
    let markdown = &markdown.replace("\r\n", "\n");
    assert!(markdown.starts_with("---\n"));
    let end = markdown[4..].find("\n---\n").unwrap() + 4;
    markdown[4..end]
        .lines()
        .map(|line| {
            let (key, value) = line.split_once(':').unwrap();
            (key.to_owned(), Value::String(value.trim().to_owned()))
        })
        .collect()
}

#[test]
fn ac4_skill_keeps_the_workflow_steps_and_details() {
    let skill = fs::read_to_string(SKILL).unwrap();
    for required in [
        "thoth resume RUN --json",
        "`thoth_resume`",
        "unopened_citation",
        ".agent/review-input/",
        "verify RUN --at-base",
        "verify@base:",
        "judge",
        "--model sonnet",
        "haiku",
        "thoth report --open",
        "failed`, `interrupted`, `unverified` and `reject`",
        "resume` refuses a failed run",
        "folded into the worker's rework prompt",
        "timeout_seconds",
        "resume in a later turn",
        "do not run the check on the task's `base_commit` by hand",
        "temporary worktree at `base_commit`",
        "new evidence codes the reviewer can cite",
        "leaves `thoth status`",
        "`runs[].worktree`",
        // RJ-3: the commander chooses and says why on every launch
        "--reason \"...\"",
        "task_history",
        "unmatched",
        "agy models",
    ] {
        assert!(skill.contains(required), "missing {required}");
    }
}

/// RJ-3: Thoth no longer picks, hands over or recommends, and reviews never
/// wait for a reset; the skill must not teach any of it.
#[test]
fn ac5_skill_teaches_nothing_thoth_no_longer_does() {
    let skill = fs::read_to_string(SKILL).unwrap();
    for gone in [
        "--profile auto",
        "--model auto",
        "--effort auto",
        "recommended_launch",
        "auto_reasons",
        "handover",
        "gemini-3.1-pro",
        "pass_rate",
        "wait for reset and tell the human",
        "The reviewer must be from a different provider",
        "low-risk task",
    ] {
        assert!(!skill.contains(gone), "still says {gone}");
    }
}

#[test]
fn ac5_parsers_accept_crlf_checkouts() {
    let crlf = "---\r\nname: x\r\ndescription: y\r\n---\r\n\r\n```json\r\n{\"a\": 1}\r\n```\r\n";
    assert_eq!(frontmatter(crlf).get("name"), Some(&json!("x")));
    assert_eq!(json_fences(crlf), 1);
}

#[test]
fn ac4_skill_starts_with_required_frontmatter() {
    let skill = fs::read_to_string(SKILL).unwrap();
    let fields = frontmatter(&skill);
    assert_eq!(fields.get("name"), Some(&json!("thoth-dispatch")));
    let description = fields["description"].as_str().unwrap();
    for situation in [
        "another subscription account",
        "running",
        "awaiting review",
        "interrupted",
        "unverified",
    ] {
        assert!(description.contains(situation), "missing {situation}");
    }
}

fn portability_violations(text: &str) -> Vec<String> {
    // The public repository's own name is not a private path.
    let lower = text.to_ascii_lowercase().replace("johnny-lo/thoth", "");
    let mut found = Vec::new();
    for forbidden in [
        "/users/",
        "c--users-",
        "johnny",
        ".agentctl",
        "docs/trial",
        "desktop/thoth",
        "repos/thoth",
    ] {
        if lower.contains(forbidden) {
            found.push(forbidden.to_owned());
        }
    }
    let bytes = text.as_bytes();
    if bytes
        .windows(3)
        .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && matches!(w[2], b'/' | b'\\'))
    {
        found.push("drive-letter absolute path".to_owned());
    }
    found
}

#[test]
fn ac5_public_plugin_files_contain_no_private_or_absolute_paths() {
    for path in [
        SKILL,
        MARKETPLACE,
        MANIFEST,
        MCP,
        "plugins/thoth/hooks/hooks.json",
        "plugins/thoth/hooks/register.tsx",
        "plugins/thoth/hooks/model.ts",
        "plugins/thoth/hooks/i18n.ts",
        "plugins/thoth/types/index.d.ts",
        "plugins/thoth/tests/panel.test.ts",
    ] {
        let violations = portability_violations(&fs::read_to_string(path).unwrap());
        assert!(violations.is_empty(), "{path}: {violations:?}");
    }
}

#[test]
fn ac5_public_skill_documents_antigravity_worker() {
    let skill = fs::read_to_string(SKILL).unwrap();
    assert!(skill.contains("Antigravity worker"));
}

#[test]
fn ac5_portability_checker_rejects_an_intentionally_bad_path() {
    let bad = "bad path is C:/Users/someone/.agentctl/profiles";
    assert!(!portability_violations(bad).is_empty());
}

fn referenced_mcp_tools(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut tools = BTreeSet::new();
    let mut offset = 0;
    while let Some(found) = text[offset..].find("thoth_") {
        let start = offset + found;
        let mut end = start + 6;
        while end < bytes.len() && (bytes[end].is_ascii_lowercase() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start + 6 {
            tools.insert(text[start..end].to_owned());
        }
        offset = end.max(start + 6);
    }
    tools
}

fn unknown_mcp_tools(text: &str, available: &BTreeSet<String>) -> Vec<String> {
    referenced_mcp_tools(text)
        .difference(available)
        .cloned()
        .collect()
}

fn actual_mcp_tools() -> BTreeSet<String> {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .arg("mcp")
        .env("THOTH_HOME", home.path())
        .env_remove("THOTH_RUN_ID")
        .env_remove("THOTH_INBOX_DIR")
        .env_remove("THOTH_EXE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}})
    )
    .unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn ac5_skill_references_only_tools_reported_by_mcp() {
    let unknown = unknown_mcp_tools(&fs::read_to_string(SKILL).unwrap(), &actual_mcp_tools());
    assert!(unknown.is_empty(), "unknown tools: {unknown:?}");
}

#[test]
fn ac5_tool_checker_rejects_an_intentionally_unknown_tool() {
    assert_eq!(
        unknown_mcp_tools("call `thoth_no_such_tool`", &actual_mcp_tools()),
        ["thoth_no_such_tool"]
    );
}

fn json_fences(text: &str) -> usize {
    let text = text.replace("\r\n", "\n");
    let mut rest = text.as_str();
    let mut count = 0;
    while let Some(start) = rest.find("```json\n") {
        let body = &rest[start + 8..];
        let end = body.find("\n```").unwrap();
        serde_json::from_str::<Value>(&body[..end]).unwrap();
        count += 1;
        rest = &body[end + 4..];
    }
    count
}

#[test]
fn ac5_every_json_fence_in_skill_parses() {
    let skill = fs::read_to_string(SKILL).unwrap();
    assert!(json_fences(&skill) >= 2);
}

#[test]
fn ac6_docs_show_remote_local_and_installed_plugin_interfaces() {
    let docs = fs::read_to_string("docs/mcp.md").unwrap();
    for required in [
        "## Claude Code plugin",
        "claude plugin marketplace add johnny-lo/Thoth",
        "claude plugin install thoth@thoth",
        "claude plugin marketplace add /absolute/path/to/thoth",
        "must already be on `PATH`",
        "skill remains available",
        "MCP connection failure",
        "/thoth:thoth-dispatch",
        "plugin:thoth:thoth",
        "mcp__plugin_thoth_thoth__thoth_status",
    ] {
        assert!(docs.contains(required), "missing {required}");
    }
}

#[test]
fn ac6_adapter_contract_points_to_the_plugin_skill() {
    let docs = fs::read_to_string("docs/adapter-usage.md").unwrap();
    let first_paragraph = docs
        .lines()
        .find(|line| line.starts_with("This page"))
        .unwrap();
    assert!(first_paragraph.contains("plugins/thoth/skills/thoth-dispatch/SKILL.md"));
    assert!(!first_paragraph.contains(".claude/skills/thoth-dispatch/SKILL.md"));
}

#[test]
fn plugin_files_exist_without_forbidden_payload_directories() {
    for path in [
        MARKETPLACE,
        MANIFEST,
        MCP,
        SKILL,
        "plugins/thoth/hooks/hooks.json",
        "plugins/thoth/hooks/register.tsx",
        "plugins/thoth/hooks/model.ts",
        "plugins/thoth/hooks/i18n.ts",
        "plugins/thoth/types/index.d.ts",
        "plugins/thoth/tests/panel.test.ts",
    ] {
        assert!(Path::new(path).is_file(), "{path}");
    }
    assert_eq!(
        read_json("plugins/thoth/hooks/hooks.json"),
        json!({"modules":["./register.tsx"]})
    );
    for absent in [
        "plugins/thoth/bin",
        "plugins/thoth/commands",
        "plugins/thoth/agents",
    ] {
        assert!(!Path::new(absent).exists());
    }
}

const INTEGRATIONS: &str = "docs/integrations.md";

fn angle_placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        if let Some(end) = after.find('>') {
            let inner = &after[..end];
            if !inner.is_empty()
                && inner.chars().next().unwrap().is_ascii_alphabetic()
                && inner
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' '))
            {
                found.push(format!("<{inner}>"));
            }
        }
        rest = after;
    }
    found
}

#[test]
fn ac5a_integrations_doc_has_every_required_command_and_setting() {
    let doc = fs::read_to_string(INTEGRATIONS).unwrap();
    for required in [
        "claude plugin marketplace add johnny-lo/Thoth",
        "claude plugin install thoth@thoth",
        "codex mcp add thoth -- thoth mcp",
        "[mcp_servers.thoth]",
        "command = \"thoth\"",
        "args = [\"mcp\"]",
        "tool_timeout_sec = 120",
        "env_vars = [\"THOTH_HOME\"]",
        "{\"mcpServers\":{\"thoth\":{\"command\":\"thoth\",\"args\":[\"mcp\"]}}}",
        "thoth guide",
        "plugins/thoth/skills/thoth-dispatch",
        "thoth_guide",
        "30 seconds",
        "## Checked against",
        "Claude Code",
        "2.1.285",
        "Codex CLI",
        "0.154.0",
        "2026-09-30",
        "not verified on a real install",
        "~/.codex/skills",
        "~/.agents/skills",
        "not tested",
    ] {
        assert!(doc.contains(required), "missing {required}");
    }
}

#[test]
fn ac5b_every_json_fence_in_integrations_doc_parses() {
    let doc = fs::read_to_string(INTEGRATIONS).unwrap();
    assert!(json_fences(&doc) >= 1);
}

#[test]
fn ac5c_integrations_doc_references_only_real_tools() {
    let doc = fs::read_to_string(INTEGRATIONS).unwrap();
    let unknown = unknown_mcp_tools(&doc, &actual_mcp_tools());
    assert!(unknown.is_empty(), "unknown tools: {unknown:?}");
    assert!(actual_mcp_tools().contains("thoth_guide"));
}

#[test]
fn ac5d_integrations_doc_has_no_private_names_or_paths() {
    let violations = portability_violations(&fs::read_to_string(INTEGRATIONS).unwrap());
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn ac5e_integrations_doc_has_no_angle_bracket_placeholders() {
    let found = angle_placeholders(&fs::read_to_string(INTEGRATIONS).unwrap());
    assert!(found.is_empty(), "placeholders: {found:?}");
}

#[test]
fn ac5e_placeholder_checker_rejects_an_intentionally_bad_text() {
    assert_eq!(
        angle_placeholders("claude plugin marketplace add <OWNER>/thoth and <the repo>"),
        ["<OWNER>", "<the repo>"]
    );
    assert!(angle_placeholders("a < b and c > d, x=1").is_empty());
}

#[test]
fn portability_allows_the_public_repo_name_but_not_private_paths() {
    assert!(portability_violations("see johnny-lo/Thoth").is_empty());
    assert!(portability_violations("C:/Users/johnny/x").contains(&"johnny".to_owned()));
    assert!(portability_violations("cd /c/Users/someone/x").contains(&"/users/".to_owned()));
    assert!(portability_violations("C--Users-someone-repos-x").contains(&"c--users-".to_owned()));
}
