//! RJ-2: what `thoth recommend` sends besides the account facts: the three
//! external tables filtered to the organizations with an account here (AC4;
//! CROSS-1: only with `--tables`), Thoth's own usage with runs left (AC5;
//! CROSS-1: under each account's model in `candidates[].models[].usage`),
//! the task's reject history (AC6), each account's models (AC7; CROSS-1: the
//! cross table's `models`), the human output (AC10) and the MCP tool (AC11).
//! Always the fakes and a temporary THOTH_HOME; the caches are written by the
//! tests; nothing reaches a network or a real harness.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const CODEX_MODEL: &str = "gpt-5.6-luna";

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
}

fn recommend(f: &Fixture, role: &str, task: Option<&str>) -> Value {
    let mut c = f.cmd();
    c.args(["recommend", "--role", role, "--json"]);
    if let Some(t) = task {
        c.args(["--task", t]);
    }
    json_out(&mut c)
}

/// CROSS-1: `recommend --tables --json`, which also sends the three full
/// filtered tables.
fn recommend_tables(f: &Fixture, role: &str, task: Option<&str>) -> Value {
    let mut c = f.cmd();
    c.args(["recommend", "--role", role, "--tables", "--json"]);
    if let Some(t) = task {
        c.args(["--task", t]);
    }
    json_out(&mut c)
}

fn recommend_plain(f: &Fixture, role: &str, task: Option<&str>) -> String {
    let mut c = f.cmd();
    c.args(["recommend", "--role", role]);
    if let Some(t) = task {
        c.args(["--task", t]);
    }
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// CROSS-1: the human output's table line of `profile`'s `model`, split into
/// its cells (columns are at least two spaces apart, cells have single
/// spaces).
fn table_cells(plain: &str, profile: &str, model: &str) -> Vec<String> {
    let block = plain
        .split("\n\n")
        .find(|b| b.starts_with(&format!("{profile}  ")) || b.starts_with(&format!("{profile} (")))
        .unwrap_or_else(|| panic!("no block for {profile}: {plain}"));
    let line = block
        .lines()
        .find(|l| l.starts_with(&format!("  {model} ")))
        .unwrap_or_else(|| panic!("no line for {model} in {block}"));
    line.split("  ").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect()
}

fn candidate<'a>(v: &'a Value, profile: &str) -> &'a Value {
    v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == profile)
        .unwrap_or_else(|| panic!("no candidate {profile}: {v}"))
}

/// CROSS-1: the usage rows of `profile`'s `model` row in the cross table.
fn model_usage(v: &Value, profile: &str, model: &str) -> Vec<Value> {
    candidate(v, profile)["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == model)
        .unwrap_or_else(|| panic!("no model {model} for {profile}: {v}"))["usage"]
        .as_array()
        .unwrap()
        .clone()
}

/// Every usage row in the output: under the models and under
/// `usage_other_models`.
fn all_usage(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for c in v["candidates"].as_array().unwrap() {
        for m in c["models"].as_array().unwrap().iter().chain(c["usage_other_models"].as_array().unwrap()) {
            out.extend(m["usage"].as_array().unwrap().iter().cloned());
        }
    }
    out
}

/// Whether `key` is a key anywhere in `v`.
fn has_key(v: &Value, key: &str) -> bool {
    match v {
        Value::Object(m) => m.contains_key(key) || m.values().any(|x| has_key(x, key)),
        Value::Array(a) => a.iter().any(|x| has_key(x, key)),
        _ => false,
    }
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort_unstable();
    keys
}

fn add_codex(f: &Fixture, id: &str, with_cache: bool) -> PathBuf {
    let cfg = f.add_codex_profile(id);
    if with_cache {
        fs::write(
            cfg.join("models_cache.json"),
            json!({"models":[
                {"slug":CODEX_MODEL,"display_name":"Luna","visibility":"list","priority":1},
                {"slug":"gpt-5.6-hidden","display_name":"Hidden","visibility":"hide","priority":2},
                {"slug":"gpt-5.6-sol","display_name":"Sol","visibility":"list","priority":3}
            ]})
            .to_string(),
        )
        .unwrap();
    }
    cfg
}

fn add_agy(f: &Fixture, id: &str) {
    let cfg = f.home_path().join("profiles").join(id);
    fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", id, "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
}

/// The three caches, each with rows of organizations with and without an
/// account here and with columns Thoth must not send on.
fn write_caches(f: &Fixture) {
    let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    fs::write(
        f.home_path().join("model-scores-arena.json"),
        json!({"schema":"agent-arena-v1","fetched_at":now,"sha":"sha-t","source":{"name":"Agent Arena","url":"https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset","license":"CC BY 4.0","license_url":"https://creativecommons.org/licenses/by/4.0/"},
            "boards":{
                "agent":{"publish_date":"2026-10-16","rows":[
                    {"model_name":"Mistral Big","organization":"mistral","score":1600.0,"score_ci_lower":1590.0,"score_ci_upper":1610.0,"observation_count":50.0,"rank":1,"license":"x","category":"y","session_count":3.0},
                    {"model_name":"GPT 6.1 Sol (Max)","organization":"openai","score":1500.5,"score_ci_lower":1490.0,"score_ci_upper":1510.0,"observation_count":100.0,"rank":3},
                    {"model_name":"Claude Fable 5.1 (high)","organization":"anthropic","score":1520.0,"score_ci_lower":1510.0,"score_ci_upper":1530.0,"observation_count":80.0,"rank":2},
                    {"model_name":"Gemini 3.8 Pro","organization":"google","score":1400.0,"score_ci_lower":1390.0,"score_ci_upper":1410.0,"observation_count":70.0,"rank":4},
                    {"model_name":"Llama 9","organization":"meta","score":1300.0,"score_ci_lower":null,"score_ci_upper":null,"observation_count":10.0,"rank":5}
                ]},
                "agent_bash_reliability":{"publish_date":"2026-10-16","rows":[
                    {"model_name":"Claude Fable 5.1 (high)","organization":"anthropic","score":1100.0,"score_ci_lower":null,"score_ci_upper":null,"observation_count":9.0,"rank":1},
                    {"model_name":"Subset Only","organization":"openai","score":1000.0,"score_ci_lower":null,"score_ci_upper":null,"observation_count":9.0,"rank":2}
                ]}
            }})
        .to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        json!({"schema":"artificialanalysis-v2-free","fetched_at":now,"source":{"name":"Artificial Analysis","url":"https://artificialanalysis.ai"},"pages":1,"data":[
            {"name":"Claude Fable 5.1 (max)","model_creator":{"slug":"anthropic","name":"Anthropic"},"evaluations":{"artificial_analysis_intelligence_index":70.0,"artificial_analysis_coding_index":60.0,"artificial_analysis_agentic_index":55.0},"pricing":{"price_1m_input_tokens":3.0,"price_1m_output_tokens":15.0}},
            {"name":"GPT 6.1 Sol (xhigh)","model_creator":{"slug":"openai","name":"OpenAI"},"evaluations":{"artificial_analysis_intelligence_index":71.0,"artificial_analysis_coding_index":61.0,"artificial_analysis_agentic_index":null},"pricing":{"price_1m_input_tokens":2.0}},
            {"name":"Gemini 3.8 Pro","model_creator":{"slug":"google","name":"Google"},"evaluations":{"artificial_analysis_intelligence_index":65.0}},
            {"name":"Mistral Big","model_creator":{"slug":"mistral","name":"Mistral AI"},"evaluations":{"artificial_analysis_intelligence_index":50.0}},
            {"name":"Llama 9","model_creator":{"slug":"meta","name":"Meta"},"evaluations":{"artificial_analysis_intelligence_index":40.0}}
        ]})
        .to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        json!({"schema":"aistupidlevel-v1","fetched_at":now,"url":"fixture","source":{"name":"AI Stupid Level","url":"https://aistupidlevel.info","license":"test license"},"models":{"success":true,"data":[
            {"id":"1","name":"claude-sonnet-5","provider":"anthropic","currentScore":88,"trend":"up","status":"good","confidenceLower":86.0,"confidenceUpper":90.0,"lastUpdated":"2026-10-16T07:00:00Z"},
            {"id":"2","name":"gpt-6","provider":"openai","currentScore":80,"trend":"down","status":"good","confidenceLower":78.0,"confidenceUpper":82.0,"lastUpdated":"2026-10-16T07:01:00Z"},
            {"id":"3","name":"gemini-3-8-pro","provider":"google","currentScore":70,"trend":"stable","status":"warning","confidenceLower":68.0,"confidenceUpper":72.0,"lastUpdated":"2026-10-16T07:02:00Z"},
            {"id":"4","name":"mistral-big","provider":"mistral","currentScore":60,"trend":"up","status":"good","confidenceLower":58.0,"confidenceUpper":62.0,"lastUpdated":"2026-10-16T07:03:00Z"}
        ]}})
        .to_string(),
    )
    .unwrap();
}

fn names(rows: &Value, key: &str) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r[key].as_str().unwrap().to_string())
        .collect()
}

// --- CROSS-1 AC1 -------------------------------------------------------------------------

/// The top level: `usage` is gone (its rows sit under the models), `unmatched`
/// is new, and `tables` is null without --tables. The human output is the
/// same text either way.
#[test]
fn the_top_level_keys_and_tables_only_with_the_flag() {
    let f = init();
    f.add_profile("claude-a");
    add_codex(&f, "codex-a", true);
    write_caches(&f);
    let (task, _) = f.create_task(Fixture::SPEC);
    let keys = [
        "candidates", "live_quota_refresh", "model", "role", "score_refresh", "tables", "task_history", "task_id",
        "unmatched",
    ];
    for task in [None, Some(task.as_str())] {
        let without = recommend(&f, "implementer", task);
        assert_eq!(sorted_keys(&without), keys, "{without}");
        assert!(without["tables"].is_null(), "{without}");
        assert!(without["unmatched"].is_array(), "{without}");
        let with = recommend_tables(&f, "implementer", task);
        assert_eq!(sorted_keys(&with), keys, "{with}");
        assert_eq!(sorted_keys(&with["tables"]), ["agent_arena", "ai_stupid_level", "artificial_analysis"]);
        for key in ["candidates", "unmatched", "task_history", "role", "task_id", "model"] {
            assert_eq!(without[key], with[key], "{key}");
        }
        let plain = recommend_plain(&f, "implementer", task);
        let mut c = f.cmd();
        c.args(["recommend", "--role", "implementer", "--tables"]);
        if let Some(t) = task {
            c.args(["--task", t]);
        }
        let plain_tables = String::from_utf8(c.assert().success().get_output().stdout.clone()).unwrap();
        assert_eq!(plain, plain_tables);
        assert!(plain.lines().last().unwrap() == "full filtered tables: add --tables --json; nothing above picks for you", "{plain}");
        assert!(!plain.contains("rows are in --json") && !plain.contains("usage: implementer"), "{plain}");
    }
}

// --- AC4 ---------------------------------------------------------------------------------

#[test]
fn tables_keep_only_the_organizations_with_an_account_and_the_listed_columns() {
    let f = init();
    f.add_profile("claude-a");
    add_codex(&f, "codex-a", true);
    write_caches(&f);
    // CROSS-1: the full tables only with --tables
    assert!(recommend(&f, "implementer", None)["tables"].is_null());
    let v = recommend_tables(&f, "implementer", None);
    let tables = &v["tables"];
    assert_eq!(sorted_keys(tables), ["agent_arena", "ai_stupid_level", "artificial_analysis"]);
    for key in ["agent_arena", "ai_stupid_level", "artificial_analysis"] {
        let t = &tables[key];
        assert_eq!(t["present"], true, "{t}");
        assert_eq!(t["stale"], false, "{t}");
        assert_eq!(t["age_hours"], 0, "{t}");
        assert!(t["fetched_at"].is_string(), "{t}");
        assert!(t["filtered"].as_str().unwrap().contains("Filtered by Thoth"), "{t}");
        assert!(t["source"]["name"].is_string() && t["source"]["url"].is_string(), "{t}");
        assert!(t["license"].is_object(), "{t}");
        assert!(!t.to_string().contains("profile_id"), "no account in a table: {t}");
    }
    let arena = &tables["agent_arena"];
    assert_eq!(arena["license"]["text"], "CC BY 4.0");
    assert_eq!(arena["license"]["url"], "https://creativecommons.org/licenses/by/4.0/");
    assert_eq!(arena["sha"], "sha-t");
    assert_eq!(arena["publish_date"], "2026-10-16");
    assert!(arena["filtered"].as_str().unwrap().contains("CC BY 4.0"), "{arena}");
    assert!(arena["filtered"].as_str().unwrap().contains("not comparable"), "{arena}");
    assert_eq!(
        sorted_keys(arena),
        ["age_hours", "fetched_at", "filtered", "license", "present", "publish_date", "rows", "sha", "source", "stale"]
    );
    assert_eq!(names(&arena["rows"], "model_name"), ["Claude Fable 5.1 (high)", "GPT 6.1 Sol (Max)"], "{arena}");
    let fable = &arena["rows"][0];
    assert_eq!(
        sorted_keys(fable),
        ["effort", "model_name", "observation_count", "organization", "rank", "score", "score_ci_lower", "score_ci_upper", "signals"]
    );
    assert_eq!(fable["effort"], "high");
    assert_eq!(fable["rank"], 2);
    assert_eq!(fable["score"], 1520.0);
    assert_eq!(fable["signals"], json!({"agent_bash_reliability": {"score": 1100.0, "rank": 1}}));
    assert_eq!(arena["rows"][1]["effort"], "max");
    assert_eq!(arena["rows"][1]["signals"], json!({}));

    let aa = &tables["artificial_analysis"];
    assert!(aa["license"]["text"].as_str().unwrap().contains("must not be forwarded"), "{aa}");
    assert_eq!(names(&aa["rows"], "name"), ["Claude Fable 5.1 (max)", "GPT 6.1 Sol (xhigh)"], "{aa}");
    assert_eq!(sorted_keys(&aa["rows"][0]), ["agentic_index", "coding_index", "effort", "intelligence_index", "name"]);
    assert_eq!(aa["rows"][0]["effort"], "max");
    assert_eq!(aa["rows"][0]["intelligence_index"], 70.0);
    assert!(aa["rows"][1]["agentic_index"].is_null());

    let asl = &tables["ai_stupid_level"];
    assert_eq!(asl["license"]["text"], "test license");
    assert_eq!(names(&asl["rows"], "name"), ["claude-sonnet-5", "gpt-6"], "{asl}");
    assert_eq!(sorted_keys(&asl["rows"][0]), ["degraded", "name", "observed_at"]);
    assert_eq!(asl["rows"][0]["degraded"], false);
    assert_eq!(asl["rows"][1]["degraded"], true, "trend down");
    assert_eq!(asl["rows"][0]["observed_at"], "2026-10-16T07:00:00Z");
}

#[test]
fn tables_without_caches_are_absent_and_empty() {
    let f = init();
    f.add_profile("claude-a");
    let v = recommend_tables(&f, "implementer", None);
    for key in ["agent_arena", "ai_stupid_level", "artificial_analysis"] {
        let t = &v["tables"][key];
        assert_eq!(t["present"], false, "{t}");
        assert_eq!(t["rows"], json!([]), "{t}");
        assert!(t["fetched_at"].is_null(), "{t}");
        assert!(t["age_hours"].is_null(), "{t}");
        assert_eq!(t["stale"], true, "{t}");
    }
    assert!(v["tables"]["agent_arena"]["sha"].is_null());
    assert!(v["tables"]["agent_arena"]["publish_date"].is_null());
    assert!(v["tables"]["ai_stupid_level"]["license"]["text"].is_null());
    let plain = recommend_plain(&f, "implementer", None);
    for name in ["Agent Arena", "Artificial Analysis", "AI Stupid Level"] {
        assert!(plain.lines().any(|l| l == format!("{name}: not cached")), "{plain}");
    }
}

/// The organizations an agy account brings come from its stored model list
/// (MODELS-1: what its last login check read from `agy models`; the fake's
/// built-in list names claude-, gpt- and gemini- models). Before any login
/// check there is no list: only the account's own provider counts.
#[test]
fn an_agy_account_alone_brings_anthropic_and_openai_rows() {
    let f = init();
    add_agy(&f, "agy-a");
    write_caches(&f);
    let v = recommend_tables(&f, "implementer", None);
    assert_eq!(names(&v["tables"]["agent_arena"]["rows"], "organization"), ["google"], "no list stored yet: {v}");
    assert_eq!(v["candidates"][0]["models"], json!([]), "no list stored yet: {v}");
    assert!(v["candidates"][0].get("available_models").is_none(), "{v}");

    f.cmd().args(["profile", "check", "agy-a", "--json"]).assert().success();
    let v = recommend_tables(&f, "implementer", None);
    assert_eq!(
        names(&v["tables"]["agent_arena"]["rows"], "organization"),
        ["anthropic", "openai", "google"],
        "{v}"
    );
    assert_eq!(names(&v["tables"]["artificial_analysis"]["rows"], "name"), ["Claude Fable 5.1 (max)", "GPT 6.1 Sol (xhigh)", "Gemini 3.8 Pro"]);
    assert_eq!(names(&v["tables"]["ai_stupid_level"]["rows"], "name"), ["claude-sonnet-5", "gpt-6", "gemini-3-8-pro"]);
    // AC7: the stored agy list is its models; CROSS-1: each model's names are
    // the slug and the display name `agy models` printed
    let agy = &v["candidates"][0];
    assert_eq!(agy["profile_id"], "agy-a");
    let models = agy["models"].as_array().unwrap();
    assert!(
        models.iter().any(|m| m["model"] == "gemini-3.8-flash-high"
            && m["names"] == json!(["gemini-3.8-flash-high", "Fake display name for gemini-3.8-flash-high"])),
        "{agy}"
    );
    assert!(models.iter().any(|m| m["model"] == "claude-sonnet-4-6"), "{agy}");
    assert_eq!(agy["models_source"], "agy_models", "{agy}");
}

/// CROSS-1 AC2 (b): an agy account's `models` is the list its last login
/// check stored, one row each; none stored is `[]` with the note saying so.
#[test]
fn an_agy_account_has_a_model_row_per_stored_model() {
    let f = init();
    add_agy(&f, "agy-a");
    let v = recommend(&f, "implementer", None);
    let agy = candidate(&v, "agy-a");
    assert_eq!(agy["models"], json!([]), "{agy}");
    assert!(agy["models_note"].as_str().unwrap().contains("no model list stored yet"), "{agy}");
    assert!(agy["models_seen_at"].is_null(), "{agy}");

    f.cmd().env("FAKE_AGY_MODELS", "a-model,b-model").args(["profile", "check", "agy-a", "--json"]).assert().success();
    let v = recommend(&f, "implementer", None);
    let agy = candidate(&v, "agy-a");
    let models: Vec<&str> = agy["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
    assert_eq!(models, ["a-model", "b-model"], "{agy}");
    assert_eq!(agy["models_source"], "agy_models", "{agy}");
    assert!(agy["models_seen_at"].is_string(), "{agy}");
    let listed = json_out(f.cmd().args(["models", "--json"]));
    let profile = listed["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == "agy-a").unwrap();
    assert_eq!(agy["models_seen_at"], profile["models_seen_at"], "{agy}");
    assert_eq!(agy["models_note"], profile["note"], "{agy}");
}

/// CROSS-1 AC2 (c): a claude-code model a run proved unavailable leaves
/// `models` for `models_excluded`, with the run that proved it, until
/// `profile forget-model`.
#[test]
fn a_model_a_run_proved_unavailable_is_excluded_until_forgotten() {
    let f = init();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "credits_required")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "fable", "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "failed", "{}", f.explain_run(&run));

    let v = recommend(&f, "implementer", None);
    let b = candidate(&v, "claude-b");
    let models: Vec<&str> = b["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
    assert_eq!(models, ["haiku", "sonnet", "opus"], "{b}");
    let excluded = b["models_excluded"].as_array().unwrap();
    assert_eq!(excluded.len(), 1, "{b}");
    assert_eq!(sorted_keys(&excluded[0]), ["model", "reason", "run_id", "seen_at"]);
    assert_eq!(excluded[0]["model"], "fable", "{b}");
    assert_eq!(excluded[0]["run_id"], run.as_str(), "{b}");
    assert!(excluded[0]["seen_at"].is_string() && excluded[0]["reason"].is_string(), "{b}");
    // the failed fable run's usage row is still sent, once, under the other models
    let other = b["usage_other_models"].as_array().unwrap();
    assert!(other.iter().all(|o| o["model"] == "fable"), "{b}");
    let plain = recommend_plain(&f, "implementer", None);
    assert!(plain.lines().any(|l| l.starts_with("  unavailable: fable since ")), "{plain}");

    f.cmd().args(["profile", "forget-model", "claude-b", "fable", "--json"]).assert().success();
    let v = recommend(&f, "implementer", None);
    let b = candidate(&v, "claude-b");
    let models: Vec<&str> = b["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
    assert_eq!(models, ["haiku", "sonnet", "opus", "fable"], "{b}");
    assert_eq!(b["models_excluded"], json!([]), "{b}");
    assert_eq!(b["usage_other_models"], json!([]), "{b}");
}

// --- AC7 ---------------------------------------------------------------------------------

#[test]
fn candidates_list_the_models_they_can_run() {
    let f = init();
    f.add_profile("claude-a");
    add_codex(&f, "codex-a", true);
    add_codex(&f, "codex-b", false);
    let v = recommend(&f, "implementer", None);
    let model_names = |id: &str| -> Vec<String> {
        candidate(&v, id)["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap().to_string()).collect()
    };
    // CROSS-1 AC2 (a): `thoth models`' slugs in its order; the hidden one left out
    assert_eq!(model_names("codex-a"), [CODEX_MODEL, "gpt-5.6-sol"]);
    let listed = json_out(f.cmd().args(["models", "--json"]));
    let listed_slugs = |id: &str| -> Vec<String> {
        let p = listed["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == id).unwrap();
        p["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(model_names("codex-a"), listed_slugs("codex-a"));
    let codex = candidate(&v, "codex-a");
    assert_eq!(codex["models_source"], "codex_models_cache", "{codex}");
    assert!(codex["models_seen_at"].is_string(), "{codex}");
    assert!(codex["models_note"].is_null(), "{codex}");
    assert_eq!(codex["models"][0]["names"], json!([CODEX_MODEL, "Luna"]), "{codex}");
    assert_eq!(model_names("codex-b"), Vec::<String>::new());
    let codex_b = candidate(&v, "codex-b");
    assert!(codex_b["models_note"].as_str().unwrap().contains("missing or unreadable"), "{codex_b}");
    assert!(codex_b["models_seen_at"].is_null(), "{codex_b}");
    assert_eq!(model_names("claude-a"), ["haiku", "sonnet", "opus", "fable"]);
    assert_eq!(model_names("claude-a"), listed_slugs("claude-a"));
    for c in v["candidates"].as_array().unwrap() {
        // AC2 (d), and RJ-1's no-`held` still holds
        assert!(c.get("available_models").is_none(), "{c}");
        assert!(c.get("held").is_none(), "{c}");
        assert_eq!(c["shared"], false, "{c}");
        assert_eq!(c["windows"], json!([]), "no reading: {c}");
        assert_eq!(c["models_excluded"], json!([]), "{c}");
        assert_eq!(c["usage_other_models"], json!([]), "{c}");
        for m in c["models"].as_array().unwrap() {
            assert_eq!(sorted_keys(m), MODEL_KEYS, "{m}");
            assert_eq!(m["usage"], json!([]), "no runs: {m}");
        }
        // claude aliases that never ran have no name yet: three null cells
        if c["harness"] == "claude-code" {
            for m in c["models"].as_array().unwrap() {
                assert_eq!(m["names"], json!([]), "{m}");
                for key in ["agent_arena", "artificial_analysis", "ai_stupid_level"] {
                    assert!(m[key].is_null(), "{m}");
                }
            }
        }
    }
    // no cache at all: nothing is listed as unmatched
    assert_eq!(v["unmatched"], json!([]), "{v}");
}

// --- AC5 ---------------------------------------------------------------------------------

/// CROSS-1: the keys of a model row.
const MODEL_KEYS: [&str; 6] = ["agent_arena", "ai_stupid_level", "artificial_analysis", "model", "names", "usage"];

/// CROSS-1 AC4: a usage row under a model, without the account and model it
/// sits under and the account's plan and shared mark.
const USAGE_KEYS: [&str; 14] = [
    "completed", "effort", "estimate_reliable", "machines", "quota_points_avg", "quota_points_weekly_avg",
    "quota_runs", "quota_runs_unreliable", "role", "runs", "runs_left", "runs_left_window", "task_type",
    "tokens_avg",
];

fn review_doc(f: &Fixture, name: &str, verdict: &str) -> String {
    let criterion = if verdict == "PASS" {
        json!({"id": "AC1", "verdict": "met", "evidence": ["E001"]})
    } else {
        json!({"id": "AC1", "verdict": "unmet", "evidence": ["E001"], "note": "not shown"})
    };
    let path = f.home_path().join(name);
    fs::write(&path, json!({"verdict": verdict, "findings": {"criteria": [criterion], "extra_findings": []}}).to_string()).unwrap();
    path.display().to_string()
}

fn dispatch_codex(f: &Fixture, task: &str) -> String {
    let mut c = f.cmd();
    // codex's `ok` runs three commands, so E001 exists for `review record`
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_CODEX_ROLLOUT", "1")
        .args(["dispatch", task, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run = v["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(task, "180s").1, "in_review", "{}", f.explain_run(&run));
    run
}

#[test]
fn usage_counts_this_computers_runs_with_runs_left_and_never_a_verdict() {
    let f = init();
    let cfg = add_codex(&f, "codex-a", true);
    f.add_profile("claude-a");
    f.cmd().args(["config", "set", "machine-name", "alpha"]).assert().success();
    f.cmd().args(["profile", "set-shared", "codex-a", "on"]).assert().success();
    let (task1, _) = f.create_task(Fixture::SPEC);
    let run1 = dispatch_codex(&f, &task1);
    let (task2, _) = f.create_task(Fixture::SPEC);
    let run2 = dispatch_codex(&f, &task2);

    let v = recommend(&f, "implementer", None);
    // CROSS-1 AC4: the one row sits under codex-a's model, and only there
    assert_eq!(all_usage(&v).len(), 1, "{v}");
    let rows = model_usage(&v, "codex-a", CODEX_MODEL);
    assert_eq!(rows.len(), 1, "{v}");
    let row = &rows[0];
    assert_eq!(sorted_keys(row), USAGE_KEYS, "{row}");
    assert_eq!(row["role"], "implementer");
    assert_eq!(row["effort"], "medium");
    assert!(row["task_type"].is_null(), "{row}");
    assert_eq!(candidate(&v, "codex-a")["shared"], true);
    assert_eq!(row["machines"], json!(["alpha"]));
    assert_eq!(row["runs"], 2);
    assert_eq!(row["completed"], 2);
    assert!(row["tokens_avg"].as_f64().unwrap() > 0.0, "{row}");
    // a shared account: every run is unreliable, the estimate still has a
    // number and says it is not the account's own
    assert_eq!(row["quota_runs_unreliable"], 2, "{row}");
    assert_eq!(row["quota_runs"], 2, "{row}");
    assert_eq!(row["quota_points_avg"], 3.0, "{row}");
    assert_eq!(row["quota_points_weekly_avg"], 0.0, "{row}");
    assert_eq!(row["estimate_reliable"], false);
    // the fake rollout's windows have reset (count as 0%): (95 - 0) / 3
    assert_eq!(row["runs_left"], 31, "{row}");
    assert_eq!(row["runs_left_window"], "five_hour", "{row}");
    let plain = recommend_plain(&f, "implementer", None);
    let cells = table_cells(&plain, "codex-a", CODEX_MODEL);
    assert_eq!(cells[6], "31 five_hour", "{cells:?}");
    assert!(cells[5].ends_with(" unreliable"), "{cells:?}");
    assert!(plain.lines().last().unwrap().contains("--tables"), "{plain}");
    // the implementer runs are not the reviewer role's (that role's rows are
    // checked in `usage_keeps_reviewer_and_spec_reviewer_runs_in_rows_of_their_own`)
    assert_eq!(all_usage(&recommend(&f, "reviewer", None)), Vec::<Value>::new());

    // verdicts change nothing in usage
    let pass = review_doc(&f, "pass.json", "PASS");
    f.cmd().args(["review", "record", &task1, "-f", &pass, "--json"]).assert().success();
    let reject = review_doc(&f, "reject.json", "REJECT");
    f.cmd().args(["review", "record", &task2, "-f", &reject, "--json"]).assert().success();
    let v = recommend(&f, "implementer", None);
    let rows = model_usage(&v, "codex-a", CODEX_MODEL);
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(sorted_keys(&rows[0]), USAGE_KEYS, "{v}");
    for key in ["pass", "reject", "pass_rate", "reviewed", "verdict"] {
        assert!(rows[0].get(key).is_none(), "{key} must not be sent: {v}");
    }
    for key in ["pass", "reject", "pass_rate"] {
        assert!(!has_key(&v, key), "no {key} anywhere: {v}");
    }
    let usage = json!(all_usage(&v)).to_string();
    assert!(!usage.contains("PASS") && !usage.contains("REJECT"), "{v}");
    let _ = (run1, run2);

    // a shared-folder line of another computer, same account and group:
    // counted in runs and machines, not in the quota averages. The same
    // remote run written twice in one file and again in another file is
    // still one run (review of attempt 1: each run once).
    let dir = f.home_path().join("records");
    fs::create_dir_all(&dir).unwrap();
    let line = json!({
        "v": 1, "machine": "other", "repo_id": "deadbeef0000", "repo": "Thoth",
        "run_id": "REMOTE-1", "task_id": "T-REMOTE", "task_title": "remote", "attempt": 1,
        "role": "implementer", "profile": "codex-a", "provider": "openai", "harness": "codex",
        "model": CODEX_MODEL, "effort_requested": "medium", "effort": "medium",
        "task_type": null, "difficulty": null, "risk": "low",
        "status": "completed", "interrupted_reason": null, "exit_code": 0,
        "started_at": "2026-01-02T00:00:00Z", "ended_at": "2026-01-02T00:10:00Z",
        "verdict": "PASS",
        "tokens": {"input": 5, "cache_read": null, "cache_write": null, "output": 6, "reasoning": null, "total": 11},
        "quota": {"five_hour_points": 50.0, "seven_day_points": 9.0, "reliable": true, "unreliable_reasons": [], "source": "codex_rollout"},
        "recommended": null, "deviates": false, "exempt": null, "reason": null,
    });
    fs::write(dir.join("other.jsonl"), format!("{line}\n{line}\n")).unwrap();
    fs::write(dir.join("third.jsonl"), format!("{line}\n")).unwrap();
    f.cmd().args(["config", "set", "records-dir"]).arg(&dir).assert().success();
    let v = recommend(&f, "implementer", None);
    let rows = model_usage(&v, "codex-a", CODEX_MODEL);
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["runs"], 3, "two local runs plus REMOTE-1 once: {v}");
    assert_eq!(rows[0]["completed"], 3, "{v}");
    assert_eq!(rows[0]["machines"], json!(["alpha", "other"]), "{v}");
    assert_eq!(rows[0]["quota_runs"], 2, "{v}");
    assert_eq!(rows[0]["quota_points_avg"], 3.0, "{v}");
    assert_eq!(rows[0]["runs_left"], 31, "{v}");
    assert_eq!(rows[0]["estimate_reliable"], false, "{v}");

    // CROSS-1 AC4 (3): a model gone from the cache keeps its usage, once,
    // under `usage_other_models`
    fs::write(
        cfg.join("models_cache.json"),
        json!({"models":[{"slug":"gpt-5.6-sol","display_name":"Sol","visibility":"list","priority":3}]}).to_string(),
    )
    .unwrap();
    let v = recommend(&f, "implementer", None);
    let codex = candidate(&v, "codex-a");
    assert!(!codex["models"].as_array().unwrap().iter().any(|m| m["model"] == CODEX_MODEL), "{codex}");
    let other = codex["usage_other_models"].as_array().unwrap();
    assert_eq!(other.len(), 1, "{codex}");
    assert_eq!(other[0]["model"], CODEX_MODEL, "{codex}");
    assert_eq!(other[0]["usage"].as_array().unwrap().len(), 1, "{codex}");
    assert_eq!(other[0]["usage"][0]["runs"], 3, "{codex}");
    assert_eq!(sorted_keys(&other[0]["usage"][0]), USAGE_KEYS, "{codex}");
    assert_eq!(all_usage(&v).len(), 1, "{v}");
    let plain = recommend_plain(&f, "implementer", None);
    assert!(plain.lines().any(|l| l == format!("  other: {CODEX_MODEL} 3 run(s) (not in the list now)")), "{plain}");
}

/// Review of attempt 3 (AC5 (1)): a usage row's `role` is the run's own
/// role, so `--role reviewer` keeps a spec review and a code review of the
/// same account, model, effort and task type in two rows, not one row of the
/// command's role. All three runs are made through the CLI with fake-claude:
/// `spec review start`, `dispatch`, `review start`.
#[test]
fn usage_keeps_reviewer_and_spec_reviewer_runs_in_rows_of_their_own() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_approve").env("FAKE_TASK_ID", &task).args([
        "spec", "review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "high", "--json",
    ]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "spec review start")).unwrap();
    let spec_run = r["run_id"].as_str().unwrap().to_string();
    let mut wait = f.cmd();
    wait.args(["wait", "--run", &spec_run, "--timeout", "180s", "--json"]);
    let w: Value = serde_json::from_slice(&f.run_ok(wait, "wait --run")).unwrap();
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&spec_run));
    assert_eq!(w["role"], "spec_reviewer", "{w}");

    // `hooks` captures commands, so the reviewer has evidence to cite
    let d = f.dispatch(&task, "claude-b", "hooks", &["--model", "sonnet", "--effort", "medium"]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&run));
    let r = f.review_start(&task, "claude-a", "review_pass", &["--model", "opus", "--effort", "high"]);
    let reviewer = r["run_id"].as_str().unwrap().to_string();
    let mut wait = f.cmd();
    wait.args(["wait", "--run", &reviewer, "--timeout", "180s", "--json"]);
    let w: Value = serde_json::from_slice(&f.run_ok(wait, "wait --run")).unwrap();
    assert_eq!(w["verdict"], "PASS", "{}", f.explain_task(&task));
    assert_eq!(w["role"], "reviewer", "{w}");

    let v = recommend(&f, "reviewer", None);
    assert_eq!(all_usage(&v).len(), 2, "the implementer run left out: {v}");
    let rows = model_usage(&v, "claude-a", "opus");
    let roles: Vec<&str> = rows.iter().map(|r| r["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["reviewer", "spec_reviewer"], "a row per run role: {v}");
    for row in &rows {
        assert_eq!(sorted_keys(row), USAGE_KEYS, "{row}");
        assert_eq!(row["effort"], "high", "{row}");
        assert!(row["task_type"].is_null(), "{row}");
        assert_eq!(row["runs"], 1, "{row}");
        assert_eq!(row["completed"], 1, "{row}");
    }
    // CROSS-1: the table shows one of the two rows and says there is another
    let plain = recommend_plain(&f, "reviewer", None);
    let cells = table_cells(&plain, "claude-a", "opus");
    assert!(cells[6].ends_with(" (+1 rows)"), "{cells:?}");

    // the implementer role has the implementer run alone
    let v = recommend(&f, "implementer", None);
    assert_eq!(all_usage(&v).len(), 1, "{v}");
    let rows = model_usage(&v, "claude-b", "sonnet");
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["role"], "implementer", "{v}");
    assert_eq!(rows[0]["runs"], 1, "{v}");
}

fn rfc3339_hours_from_now(hours: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn epoch_hours_from_now(hours: i64) -> i64 {
    (time::OffsetDateTime::now_utc() + time::Duration::hours(hours)).unix_timestamp()
}

/// A completed implementer run of `profile` on `model` under a task of its
/// own, with one `run_quota` event of `detail`.
fn insert_run_with_quota(f: &Fixture, run_id: &str, profile: &str, model: &str, detail: &Value) {
    let (task, _) = f.create_task(Fixture::SPEC);
    let conn = f.state_conn();
    conn.execute(
        "INSERT INTO runs (run_id,task_id,attempt,role,provider,account,model,spec,status,config_dir,
                           allowed_tools,worktree,run_dir,base_commit,cancel_requested,timeout_seconds,
                           started_at,ended_at,input_tokens,output_tokens)
         VALUES (?1,?2,1,'implementer','anthropic',?3,?4,'{\"launch\":{\"effort\":\"medium\"}}','completed','','','','','abc',0,0,
                 '2026-01-01T00:00:00Z','2026-01-01T00:10:00Z',10,20)",
        params![run_id, task, profile, model],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES ('2026-01-01T00:10:00Z', ?1, 'run_quota', ?2)",
        params![run_id, detail.to_string()],
    )
    .unwrap();
}

/// The account's current windows: one `rate_limit_warning` on `run_id`.
fn insert_windows(f: &Fixture, run_id: &str, five_hour: f64, seven_day: f64) {
    let detail = json!({"windows":[
        {"name":"five_hour","utilization":five_hour,"resets_at":epoch_hours_from_now(2)},
        {"name":"seven_day","utilization":seven_day,"resets_at":epoch_hours_from_now(48)}
    ]});
    f.state_conn()
        .execute(
            "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, 'rate_limit_warning', ?3)",
            params![rfc3339_hours_from_now(-1), run_id, detail.to_string()],
        )
        .unwrap();
}

/// The one usage row of `profile`'s `model`.
fn usage_row(v: &Value, profile: &str, model: &str) -> Value {
    let rows = model_usage(v, profile, model);
    assert_eq!(rows.len(), 1, "one usage row for {profile} {model}: {rows:?}");
    rows[0].clone()
}

/// Review of attempt 1 (AC10): an exhausted account whose runs give no
/// average has `runs_left` 0 but no window to name; the line keeps the
/// known 0 and says the window is unknown, instead of `runs left 0` alone
/// or `runs left unknown`.
#[test]
fn an_exhausted_account_without_an_average_prints_runs_left_zero_with_the_window_unknown() {
    let f = init();
    f.add_profile("claude-a");
    insert_run_with_quota(&f, "R-NOREAD", "claude-a", "sonnet", &json!({"points": null, "reliable": true, "seven_day": {"points": null}}));
    insert_windows(&f, "R-NOREAD", 0.97, 0.10);
    let v = recommend(&f, "implementer", None);
    assert_eq!(candidate(&v, "claude-a")["quota_state"], "exhausted", "{v}");
    let row = usage_row(&v, "claude-a", "sonnet");
    assert!(row["quota_points_avg"].is_null() && row["quota_points_weekly_avg"].is_null(), "{row}");
    assert_eq!(row["quota_runs"], 0, "{row}");
    assert_eq!(row["runs_left"], 0, "{row}");
    assert!(row["runs_left_window"].is_null(), "{row}");
    // CROSS-1: the known 0 alone, not `no data`
    let plain = recommend_plain(&f, "implementer", None);
    let cells = table_cells(&plain, "claude-a", "sonnet");
    assert_eq!(cells[6], "0", "{cells:?}");
}

/// Review of attempt 1: the averages prefer the reliable runs and fall back
/// to the unreliable ones, checked through `recommend --json` on runs read
/// from the database, not on a hand-built row. Three groups of one account:
/// sonnet has reliable and unreliable runs, opus only an unreliable one with
/// numbers, haiku only an unreliable one without.
#[test]
fn usage_averages_prefer_reliable_runs_and_fall_back_to_unreliable_ones() {
    let f = init();
    f.add_profile("claude-a");
    insert_run_with_quota(&f, "R-S1", "claude-a", "sonnet", &json!({"points": 2.0, "reliable": true, "seven_day": {"points": null}}));
    insert_run_with_quota(&f, "R-S2", "claude-a", "sonnet", &json!({"points": 4.0, "reliable": true, "seven_day": {"points": 1.0}}));
    insert_run_with_quota(
        &f,
        "R-S3",
        "claude-a",
        "sonnet",
        &json!({"points": 100.0, "reliable": false, "unreliable_reasons": ["overlap:R-S1"], "seven_day": {"points": 100.0}}),
    );
    insert_run_with_quota(
        &f,
        "R-O1",
        "claude-a",
        "opus",
        &json!({"points": 100.0, "reliable": false, "unreliable_reasons": ["overlap:R-S1"], "seven_day": {"points": 100.0}}),
    );
    insert_run_with_quota(&f, "R-H1", "claude-a", "haiku", &json!({"points": null, "reliable": false, "unreliable_reasons": ["overlap:R-S1"], "seven_day": {"points": null}}));
    insert_windows(&f, "R-S1", 0.20, 0.10);

    let v = recommend(&f, "implementer", None);
    assert_eq!(all_usage(&v).len(), 3, "{v}");
    let sonnet = usage_row(&v, "claude-a", "sonnet");
    assert_eq!(sonnet["runs"], 3, "{sonnet}");
    assert_eq!(sonnet["quota_runs"], 2, "the reliable runs only: {sonnet}");
    assert_eq!(sonnet["quota_runs_unreliable"], 1, "{sonnet}");
    assert_eq!(sonnet["quota_points_avg"], 3.0, "{sonnet}");
    assert_eq!(sonnet["quota_points_weekly_avg"], 1.0, "the one reliable run with a weekly number: {sonnet}");
    assert_eq!(sonnet["estimate_reliable"], true, "{sonnet}");
    // (95 - 20) / 3 = 25 for five_hour; (95 - 10) / 1 = 85 for seven_day
    assert_eq!(sonnet["runs_left"], 25, "{sonnet}");
    assert_eq!(sonnet["runs_left_window"], "five_hour", "{sonnet}");

    let opus = usage_row(&v, "claude-a", "opus");
    assert_eq!(opus["quota_runs"], 1, "the unreliable run with numbers: {opus}");
    assert_eq!(opus["quota_runs_unreliable"], 1, "{opus}");
    assert_eq!(opus["quota_points_avg"], 100.0, "{opus}");
    assert_eq!(opus["quota_points_weekly_avg"], 100.0, "{opus}");
    assert_eq!(opus["estimate_reliable"], false, "{opus}");
    assert_eq!(opus["runs_left"], 0, "75 / 100 floors to 0: {opus}");
    assert_eq!(opus["runs_left_window"], "five_hour", "{opus}");

    let haiku = usage_row(&v, "claude-a", "haiku");
    assert_eq!(haiku["quota_runs"], 0, "{haiku}");
    assert_eq!(haiku["quota_runs_unreliable"], 1, "{haiku}");
    assert!(haiku["quota_points_avg"].is_null() && haiku["quota_points_weekly_avg"].is_null(), "{haiku}");
    assert_eq!(haiku["estimate_reliable"], false, "{haiku}");
    assert!(haiku["runs_left"].is_null() && haiku["runs_left_window"].is_null(), "{haiku}");

    // a shared account's estimate is never its own, reliable runs or not
    f.cmd().args(["profile", "set-shared", "claude-a", "on"]).assert().success();
    let v = recommend(&f, "implementer", None);
    assert_eq!(candidate(&v, "claude-a")["shared"], true, "{v}");
    let sonnet = usage_row(&v, "claude-a", "sonnet");
    assert_eq!(sonnet["quota_points_avg"], 3.0, "{sonnet}");
    assert_eq!(sonnet["estimate_reliable"], false, "{sonnet}");
    let plain = recommend_plain(&f, "implementer", None);
    let cells = table_cells(&plain, "claude-a", "sonnet");
    assert_eq!(cells[6], "25 five_hour", "{cells:?}");
    assert!(cells[5].ends_with(" unreliable"), "{cells:?}");
    assert!(plain.lines().any(|l| l.starts_with("claude-a (shared)  anthropic / claude-code")), "{plain}");
}

/// Review of attempt 2 (AC5 (2)): the unreliable runs are used only when the
/// group has no reliable run at all. A reliable run whose two windows have no
/// number keeps the group on the reliable side: both averages are null,
/// nothing is estimated from the unreliable run's numbers, `quota_runs` is 0,
/// and the estimate is still the account's own (not shared).
#[test]
fn a_reliable_run_without_numbers_keeps_the_unreliable_numbers_out() {
    let f = init();
    f.add_profile("claude-a");
    insert_run_with_quota(&f, "R-REL", "claude-a", "sonnet", &json!({"points": null, "reliable": true, "seven_day": {"points": null}}));
    insert_run_with_quota(
        &f,
        "R-UNREL",
        "claude-a",
        "sonnet",
        &json!({"points": 100.0, "reliable": false, "unreliable_reasons": ["overlap:R-REL"], "seven_day": {"points": 100.0}}),
    );
    insert_windows(&f, "R-REL", 0.20, 0.10);

    let v = recommend(&f, "implementer", None);
    assert_eq!(all_usage(&v).len(), 1, "{v}");
    let row = usage_row(&v, "claude-a", "sonnet");
    assert_eq!(row["runs"], 2, "{row}");
    assert_eq!(row["quota_runs_unreliable"], 1, "{row}");
    assert_eq!(row["quota_runs"], 0, "a reliable run exists, and none has a number: {row}");
    assert!(row["quota_points_avg"].is_null(), "not the unreliable run's 100: {row}");
    assert!(row["quota_points_weekly_avg"].is_null(), "{row}");
    assert_eq!(row["estimate_reliable"], true, "the reliable side, of an account that is not shared: {row}");
    assert!(row["runs_left"].is_null() && row["runs_left_window"].is_null(), "no average, no count: {row}");
    // CROSS-1: both averages null and no count read `no data`, not unreliable
    let plain = recommend_plain(&f, "implementer", None);
    let cells = table_cells(&plain, "claude-a", "sonnet");
    assert_eq!(cells[5], "no data", "{cells:?}");
    assert_eq!(cells[6], "no data", "{cells:?}");
    assert!(!cells.iter().any(|c| c.contains("unreliable")), "{cells:?}");
}

/// AC5: a run fake-claude actually made on a `profile set-shared` account
/// records a `run_quota` marked unreliable (`shared`); `usage` still gives
/// that row an average and says the estimate is not the account's own.
#[test]
fn a_shared_claude_account_run_by_fake_claude_has_an_average_marked_unreliable() {
    let f = init();
    f.add_profile("claude-b");
    f.cmd().args(["profile", "set-shared", "claude-b", "on"]).assert().success();
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "ok", &["--model", "sonnet", "--effort", "medium"]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&run));

    let v = recommend(&f, "implementer", None);
    assert_eq!(all_usage(&v).len(), 1, "{v}");
    let row = &usage_row(&v, "claude-b", "sonnet");
    assert_eq!(sorted_keys(row), USAGE_KEYS, "{row}");
    assert_eq!(row["effort"], "medium");
    assert_eq!(candidate(&v, "claude-b")["shared"], true, "{v}");
    assert_eq!(row["runs"], 1, "{row}");
    assert_eq!(row["completed"], 1, "{row}");
    assert_eq!(row["quota_runs_unreliable"], 1, "the shared mark makes the run unreliable: {row}");
    assert_eq!(row["quota_runs"], 1, "its numbers are still averaged: {row}");
    // fake-claude's one reading is both ends of the run: 0 points
    assert_eq!(row["quota_points_avg"], 0.0, "{row}");
    assert_eq!(row["quota_points_weekly_avg"], 0.0, "{row}");
    assert_eq!(row["estimate_reliable"], false, "{row}");
    // an average of 0 gives no count
    assert!(row["runs_left"].is_null() && row["runs_left_window"].is_null(), "{row}");
    let plain = recommend_plain(&f, "implementer", None);
    let cells = table_cells(&plain, "claude-b", "sonnet");
    assert_eq!(cells[5], "0.00%/0.00% unreliable", "{cells:?}");
    assert_eq!(cells[6], "no data", "{cells:?}");
}

// --- AC6 ---------------------------------------------------------------------------------

#[test]
fn task_history_counts_attempts_and_rejects_and_names_the_newest_runs() {
    let f = init();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task, _) = f.create_task(Fixture::SPEC);

    let v = recommend(&f, "implementer", Some(&task));
    assert_eq!(v["task_history"], json!({"attempts": 0, "rejects": 0, "last_implementer": null, "last_reviewer": null}), "{v}");
    let plain = recommend_plain(&f, "implementer", Some(&task));
    assert!(plain.lines().next().unwrap() == format!("task {task}: no runs yet"), "{plain}");

    let d = f.dispatch(&task, "claude-b", "hooks", &["--model", "sonnet", "--effort", "medium"]);
    let first = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&first));
    let reject = review_doc(&f, "reject.json", "REJECT");
    f.cmd().args(["review", "record", &task, "-f", &reject, "--json"]).assert().success();
    let d = f.dispatch(&task, "claude-b", "ok", &["--model", "opus", "--effort", "high"]);
    let second = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["attempt"], 2, "{d}");
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&second));

    let v = recommend(&f, "implementer", Some(&task));
    let h = &v["task_history"];
    assert_eq!(sorted_keys(h), ["attempts", "last_implementer", "last_reviewer", "rejects"]);
    assert_eq!(h["attempts"], 2, "{h}");
    assert_eq!(h["rejects"], 1, "{h}");
    let last = &h["last_implementer"];
    assert_eq!(
        sorted_keys(last),
        ["attempt", "effort", "effort_requested", "model", "profile", "provider", "run_id", "status", "verdict"]
    );
    assert_eq!(last["run_id"], second.as_str(), "{h}");
    assert_eq!(last["attempt"], 2);
    assert_eq!(last["profile"], "claude-b");
    assert_eq!(last["provider"], "anthropic");
    assert_eq!(last["model"], "opus");
    assert_eq!(last["effort_requested"], "high");
    assert_eq!(last["effort"], "high");
    assert_eq!(last["status"], "completed");
    assert!(last["verdict"].is_null(), "not reviewed yet: {h}");
    assert!(h["last_reviewer"].is_null(), "{h}");
    assert!(!h.to_string().contains("escalat"), "facts only: {h}");
    let plain = recommend_plain(&f, "implementer", Some(&task));
    let line = plain.lines().next().unwrap();
    assert!(line.starts_with(&format!("task {task}: attempt 2, rejected 1 time(s); last implementer run {second}: claude-b opus effort high (no verdict)")), "{plain}");
    assert!(!plain.contains("last reviewer"), "{plain}");

    // the reviewer role adds the reviewer line; a reviewer run gives it facts
    let plain = recommend_plain(&f, "reviewer", Some(&task));
    assert_eq!(plain.lines().nth(1).unwrap(), "last reviewer run: none", "{plain}");
    let r = f.review_start(&task, "claude-a", "review_reject", &["--model", "sonnet", "--effort", "low"]);
    let reviewer = r["run_id"].as_str().unwrap().to_string();
    let mut wait = f.cmd();
    wait.args(["wait", "--run", &reviewer, "--timeout", "180s"]);
    f.run_ok(wait, "wait --run");
    let v = recommend(&f, "reviewer", Some(&task));
    let h = &v["task_history"];
    assert_eq!(h["rejects"], 2, "{h}");
    assert_eq!(h["last_implementer"]["verdict"], "REJECT", "{h}");
    assert_eq!(h["last_reviewer"]["run_id"], reviewer.as_str(), "{h}");
    assert_eq!(h["last_reviewer"]["model"], "sonnet");
    assert_eq!(h["last_reviewer"]["effort"], "low", "{h}");
    assert_eq!(h["last_reviewer"]["verdict"], "REJECT", "{h}");
    let plain = recommend_plain(&f, "reviewer", Some(&task));
    assert!(plain.lines().nth(1).unwrap().starts_with(&format!("last reviewer run {reviewer}: claude-a sonnet effort low (REJECT)")), "{plain}");

    // a task of another repository is found too; one without --task is null
    assert!(recommend(&f, "implementer", None)["task_history"].is_null());
}

const IMPORTANT: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;

/// Review of attempt 1: `attempts` is the highest attempt of any run of the
/// task, a spec review's included (a spec review carries the attempt the next
/// dispatch will use), so a task whose only run is a spec review is at
/// attempt 1, not 0.
#[test]
fn task_history_attempts_count_a_spec_review_run_too() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(IMPORTANT);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--json"]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "spec review start")).unwrap();
    let spec_run = r["run_id"].as_str().unwrap().to_string();
    let mut wait = f.cmd();
    wait.args(["wait", "--run", &spec_run, "--timeout", "180s", "--json"]);
    let w: Value = serde_json::from_slice(&f.run_ok(wait, "wait --run")).unwrap();
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&spec_run));

    let v = recommend(&f, "implementer", Some(&task));
    let h = &v["task_history"];
    assert_eq!(h["attempts"], 1, "the spec review run is attempt 1: {h}");
    assert_eq!(h["rejects"], 0, "{h}");
    assert!(h["last_implementer"].is_null(), "{h}");
    assert!(h["last_reviewer"].is_null(), "a spec reviewer is not the task's reviewer: {h}");
    let plain = recommend_plain(&f, "implementer", Some(&task));
    let line = plain.lines().next().unwrap();
    assert_eq!(line, format!("task {task}: attempt 1, rejected 0 time(s); last implementer run: none"), "{plain}");

    // an implementer run at the same attempt changes nothing in the count
    let d = f.dispatch(&task, "claude-b", "ok", &["--model", "sonnet", "--effort", "medium"]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["attempt"], 1, "{d}");
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&run));
    let h = recommend(&f, "implementer", Some(&task))["task_history"].clone();
    assert_eq!(h["attempts"], 1, "{h}");
    assert_eq!(h["last_implementer"]["run_id"], run.as_str(), "{h}");
    assert_eq!(h["last_implementer"]["attempt"], 1, "{h}");
}

// --- AC11: MCP ---------------------------------------------------------------------------

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(f.home.path()))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("FAKE_TASK_ID")
            .env_remove("ARTIFICIALANALYSIS_API_KEY")
            .env_remove("AISTUPIDLEVEL_API_KEY")
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
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_mcp_recommend_tool_returns_the_same_document_as_the_cli() {
    let f = init();
    f.add_profile("claude-a");
    add_codex(&f, "codex-a", true);
    write_caches(&f);
    let (task, _) = f.create_task(Fixture::SPEC);
    let cli = recommend(&f, "implementer", Some(&task));

    let mut s = Server::start(&f);
    let list = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "thoth_recommend")
        .unwrap();
    let description = tool["description"].as_str().unwrap();
    const NEEDLES: [&str; 10] = [
        "quota",
        "windows",
        "models it can run now",
        "external tables",
        "own usage",
        "runs are left",
        "no table matched",
        "full filtered tables",
        "reject",
        "does not choose",
    ];
    for needle in NEEDLES {
        assert!(description.contains(needle), "{needle}: {description}");
    }
    // CROSS-1: `tables` joins the parameters
    assert_eq!(sorted_keys(&tool["inputSchema"]["properties"]), ["repo", "role", "tables", "task"]);
    assert_eq!(tool["inputSchema"]["properties"]["tables"], json!({"type": "boolean"}));
    let help = String::from_utf8(f.cmd().args(["recommend", "--help"]).assert().success().get_output().stdout.clone()).unwrap();
    for needle in NEEDLES.iter().chain(&["--tables"]) {
        assert!(help.contains(needle), "{needle}: {help}");
    }

    let response = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"thoth_recommend","arguments":{"role":"implementer","task":task}}}));
    assert_eq!(response["result"]["isError"], false, "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(sorted_keys(content), sorted_keys(&cli), "{content}");
    assert!(content["tables"].is_null(), "{content}");
    for key in ["candidates", "unmatched", "task_history", "score_refresh"] {
        assert_eq!(content[key], cli[key], "{key} differs: {content}");
    }
    assert_eq!(content["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(content["task_id"], task.as_str());

    let response = s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"thoth_recommend","arguments":{"role":"implementer","task":task,"tables":true}}}));
    assert_eq!(response["result"]["isError"], false, "{response}");
    let content = &response["result"]["structuredContent"];
    let with_tables = recommend_tables(&f, "implementer", Some(&task));
    assert!(content["tables"].is_object(), "{content}");
    assert_eq!(content["tables"], with_tables["tables"], "{content}");
    assert_eq!(content["candidates"], with_tables["candidates"], "{content}");
}

/// Nothing in `recommend` writes: the caches, databases and the profile
/// directories are untouched, reading the tables included.
#[test]
fn recommend_with_tables_is_still_read_only() {
    let f = init();
    let cfg = f.add_profile("claude-a");
    write_caches(&f);
    let before = snapshot(&f.home_path());
    recommend(&f, "implementer", None);
    recommend(&f, "reviewer", None);
    // CROSS-1: the full tables are read the same way
    recommend_tables(&f, "implementer", None);
    recommend_tables(&f, "reviewer", None);
    assert_eq!(before, snapshot(&f.home_path()));
    assert!(cfg.is_dir());
}

fn snapshot(root: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, std::time::SystemTime)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            if meta.is_dir() {
                walk(root, &path, out);
            } else if !path.to_string_lossy().ends_with("-wal") && !path.to_string_lossy().ends_with("-shm") {
                out.push((path.strip_prefix(root).unwrap().display().to_string(), meta.len(), meta.modified().unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
