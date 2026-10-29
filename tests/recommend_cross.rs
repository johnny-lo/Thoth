//! CROSS-1: `thoth recommend`'s cross table. Each account's models carry the
//! rows of the three filtered tables whose name matches one of the model's
//! own names (AC3), the account's windows (AC2 (e)), and the human output is
//! one table per account (AC5). Every state is made through the CLI (fake
//! workers, `profile usage --refresh`, cache files the tests write); always a
//! temporary THOTH_HOME, nothing reaches a network or a real harness.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::fs;

const SOL: &str = "gpt-6.1-sol";
const PRO: &str = "gpt-6.1-pro";

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

fn recommend(f: &Fixture) -> Value {
    json_out(f.cmd().args(["recommend", "--role", "implementer", "--json"]))
}

fn recommend_tables(f: &Fixture) -> Value {
    json_out(f.cmd().args(["recommend", "--role", "implementer", "--tables", "--json"]))
}

/// The human output in `lang` (`None`: English).
fn plain(f: &Fixture, lang: Option<&str>) -> String {
    let mut c = f.cmd();
    match lang {
        Some(l) => c.env("THOTH_LANG", l),
        None => c.env_remove("THOTH_LANG"),
    };
    let out = c.args(["recommend", "--role", "implementer"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn candidate<'a>(v: &'a Value, profile: &str) -> &'a Value {
    v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == profile)
        .unwrap_or_else(|| panic!("no candidate {profile}: {v}"))
}

fn model<'a>(v: &'a Value, profile: &str, name: &str) -> &'a Value {
    candidate(v, profile)["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == name)
        .unwrap_or_else(|| panic!("no model {name} for {profile}: {v}"))
}

fn unmatched<'a>(v: &'a Value, profile: &str, name: &str) -> Option<&'a Value> {
    v["unmatched"].as_array().unwrap().iter().find(|u| u["profile_id"] == profile && u["model"] == name)
}

/// A codex account whose cache lists `gpt-6.1-sol` and `gpt-6.1-pro`.
fn add_codex(f: &Fixture, id: &str) {
    let cfg = f.add_codex_profile(id);
    fs::write(
        cfg.join("models_cache.json"),
        json!({"models":[
            {"slug":SOL,"display_name":"GPT-6.1 Sol","visibility":"list","priority":1},
            {"slug":PRO,"display_name":"GPT-6.1 Pro","visibility":"list","priority":2}
        ]})
        .to_string(),
    )
    .unwrap();
}

/// The three caches with the names AC3 checks: two Agent Arena variants of
/// GPT 6.1 Sol, one Artificial Analysis one, and AI Stupid Level rows that a
/// prefix or a base without effort would wrongly match, plus `fake-model`
/// (what fake-claude's init reports, so a claude alias that ran is named so).
fn write_caches(f: &Fixture) {
    let now = time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    fs::write(
        f.home_path().join("model-scores-arena.json"),
        json!({"schema":"agent-arena-v1","fetched_at":now,"sha":"sha-x","source":{"name":"Agent Arena","url":"https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset","license":"CC BY 4.0","license_url":"https://creativecommons.org/licenses/by/4.0/"},
            "boards":{"agent":{"publish_date":"2026-10-20","rows":[
                {"model_name":"GPT 6.1 Sol (High)","organization":"openai","score":1450.0,"score_ci_lower":1440.0,"score_ci_upper":1460.0,"observation_count":90.0,"rank":6},
                {"model_name":"GPT 6.1 Sol (Max)","organization":"openai","score":1500.5,"score_ci_lower":1490.0,"score_ci_upper":1510.0,"observation_count":100.0,"rank":3},
                {"model_name":"Claude Sonnet 5 (Thinking)","organization":"anthropic","score":1288.0,"score_ci_lower":1280.0,"score_ci_upper":1296.0,"observation_count":60.0,"rank":4}
            ]}}})
        .to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores-aa.json"),
        json!({"schema":"artificialanalysis-v2-free","fetched_at":now,"source":{"name":"Artificial Analysis","url":"https://artificialanalysis.ai"},"pages":1,"data":[
            {"name":"GPT 6.1 Sol (xhigh)","model_creator":{"slug":"openai","name":"OpenAI"},"evaluations":{"artificial_analysis_intelligence_index":71.0,"artificial_analysis_coding_index":61.0,"artificial_analysis_agentic_index":null}},
            {"name":"Claude Sonnet 5","model_creator":{"slug":"anthropic","name":"Anthropic"},"evaluations":{"artificial_analysis_intelligence_index":62.0,"artificial_analysis_coding_index":55.0,"artificial_analysis_agentic_index":48.0}}
        ]})
        .to_string(),
    )
    .unwrap();
    fs::write(
        f.home_path().join("model-scores.json"),
        json!({"schema":"aistupidlevel-v1","fetched_at":now,"url":"fixture","source":{"name":"AI Stupid Level","url":"https://aistupidlevel.info","license":"test license"},"models":{"success":true,"data":[
            {"id":"1","name":"gpt-6.1","provider":"openai","currentScore":80,"trend":"up","status":"good","confidenceLower":78.0,"confidenceUpper":82.0,"lastUpdated":"2026-10-20T07:00:00Z"},
            {"id":"2","name":"fake-model","provider":"anthropic","currentScore":70,"trend":"down","status":"good","confidenceLower":68.0,"confidenceUpper":72.0,"lastUpdated":"2026-10-20T07:01:00Z"}
        ]}})
        .to_string(),
    )
    .unwrap();
}

/// AC3: the cells are the `--tables` rows the model's names match; a model
/// no table matches is listed in `unmatched` with the present tables it
/// missed.
#[test]
fn cells_hold_the_table_rows_whose_names_match_and_unmatched_names_the_rest() {
    let f = init();
    add_codex(&f, "codex-a");
    f.add_profile("claude-a");
    write_caches(&f);
    let v = recommend(&f);
    let tables = recommend_tables(&f)["tables"].clone();

    let sol = model(&v, "codex-a", SOL);
    assert_eq!(sol["names"], json!([SOL, "GPT-6.1 Sol"]), "{sol}");
    // both Agent Arena variants, in the table's (rank) order, without organization
    let arena: Vec<&str> = sol["agent_arena"].as_array().unwrap().iter().map(|r| r["model_name"].as_str().unwrap()).collect();
    assert_eq!(arena, ["GPT 6.1 Sol (Max)", "GPT 6.1 Sol (High)"], "{sol}");
    let mut full = tables["agent_arena"]["rows"][0].clone();
    full.as_object_mut().unwrap().remove("organization");
    assert_eq!(sol["agent_arena"][0], full, "a table row without organization");
    assert!(sol["agent_arena"][0].get("organization").is_none());
    assert_eq!(sol["artificial_analysis"], json!([tables["artificial_analysis"]["rows"][0]]), "{sol}");
    assert!(sol["ai_stupid_level"].is_null(), "gpt-6.1 is not gpt-6.1-sol: {sol}");
    let u = unmatched(&v, "codex-a", SOL).unwrap_or_else(|| panic!("{v}"));
    assert_eq!(u, &json!({"profile_id": "codex-a", "model": SOL, "names": [SOL, "GPT-6.1 Sol"], "missing": ["ai_stupid_level"]}));
    let pro = unmatched(&v, "codex-a", PRO).unwrap();
    assert_eq!(pro["missing"], json!(["agent_arena", "artificial_analysis", "ai_stupid_level"]));

    // a claude alias that never ran has no name: three null cells, unmatched
    let sonnet = model(&v, "claude-a", "sonnet");
    assert_eq!(sonnet["names"], json!([]), "{sonnet}");
    for key in ["agent_arena", "artificial_analysis", "ai_stupid_level"] {
        assert!(sonnet[key].is_null(), "{sonnet}");
    }
    assert_eq!(unmatched(&v, "claude-a", "sonnet").unwrap()["missing"], json!(["agent_arena", "artificial_analysis", "ai_stupid_level"]));

    // fake-claude reports `fake-model`; after one run that is sonnet's name
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task, "claude-a", "ok", &["--model", "sonnet", "--effort", "medium"]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&run));
    let listed = json_out(f.cmd().args(["models", "--json"]));
    let score_name = listed["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == "claude-a").unwrap()["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "sonnet")
        .unwrap()["score_name"]
        .clone();
    assert_eq!(score_name, "fake-model");
    let v = recommend(&f);
    let sonnet = model(&v, "claude-a", "sonnet");
    assert_eq!(sonnet["names"], json!([score_name]), "{sonnet}");
    assert_eq!(sonnet["ai_stupid_level"], json!([{"name": "fake-model", "degraded": true, "observed_at": "2026-10-20T07:01:00Z"}]), "{sonnet}");
    let missing = &unmatched(&v, "claude-a", "sonnet").unwrap()["missing"];
    assert_eq!(missing, &json!(["agent_arena", "artificial_analysis"]), "{v}");
    assert_eq!(sonnet["usage"].as_array().unwrap().len(), 1, "{sonnet}");

    // a missing cache gives null cells and is not called missed
    fs::remove_file(f.home_path().join("model-scores-aa.json")).unwrap();
    let v = recommend(&f);
    for c in v["candidates"].as_array().unwrap() {
        for m in c["models"].as_array().unwrap() {
            assert!(m["artificial_analysis"].is_null(), "{m}");
        }
    }
    for u in v["unmatched"].as_array().unwrap() {
        assert!(!u["missing"].as_array().unwrap().iter().any(|t| t == "artificial_analysis"), "{u}");
    }
    assert!(unmatched(&v, "codex-a", SOL).is_some(), "{v}");
}

/// AC5 (b): one table per account, padded by display width, `no data` in
/// the reader's language.
#[test]
fn the_human_output_is_one_table_per_account() {
    let f = init();
    add_codex(&f, "codex-a");
    write_caches(&f);
    let text = plain(&f, None);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.contains(&"  model        AA I/C/A         Arena #rank score   ASL      tokens avg  quota/run 5h/7d  runs left"), "{text}");
    assert!(lines.contains(&"  gpt-6.1-sol  71/61/- (xhigh)  #3 1500.5 (max) +1  no data  no data     no data          no data"), "{text}");
    assert!(lines.contains(&"  no match: gpt-6.1-sol (gpt-6.1-sol, GPT-6.1 Sol): ai_stupid_level"), "{text}");
    assert!(lines.iter().any(|l| l.starts_with("  models read at ")), "{text}");
    assert!(lines.iter().any(|l| l.starts_with("codex-a  openai / codex  eligible ") || l.starts_with("codex-a  openai / codex  ineligible ")), "{text}");
    assert!(lines.iter().any(|l| l.starts_with("Agent Arena: 2 row(s) (filtered), fetched ")), "{text}");
    assert_eq!(lines.last().unwrap(), &"full filtered tables: add --tables --json; nothing above picks for you");
    assert!(text.contains("add --tables"), "{text}");
    assert!(!text.contains("usage: implementer") && !text.contains("rows are in --json"), "{text}");
    assert!(lines.iter().all(|l| l == &l.trim_end()), "no trailing blanks: {text}");

    let zh = plain(&f, Some("zh-TW"));
    let zh_lines: Vec<&str> = zh.lines().collect();
    assert!(zh_lines.contains(&"  model        AA I/C/A         Arena #rank score   ASL     tokens avg  quota/run 5h/7d  runs left"), "{zh}");
    let pro = zh_lines.iter().find(|l| l.starts_with("  gpt-6.1-pro ")).unwrap_or_else(|| panic!("{zh}"));
    assert!(pro.contains("無資料"), "{pro}");
    assert!(zh.contains("no match:") && zh.contains("models read at") && zh.contains("add --tables"), "{zh}");
}

/// AC5 (c) and AC2 (e): a live reading of five_hour at 96% that has not
/// reset shows on the first line, the reasons on the next, and the
/// candidate's `windows` are `profile usage --json`'s.
#[test]
fn the_first_line_shows_the_windows_and_an_ineligible_account_its_reasons() {
    let f = init();
    f.add_profile("claude-b");
    f.cmd().args(["config", "set", "live-quota", "on"]).assert().success();
    let answer = f.home_path().join("claude-answer.jsonl");
    let lines = [
        json!({"type": "system", "subtype": "init"}),
        json!({"type": "assistant", "usage_report": {"rate_limits": {"limits": [
            {"kind": "session", "percent": 96, "resets_at": "2099-01-01T00:00:00Z"}
        ]}}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0, "total_cost_usd": 0,
            "local_command": "usage", "result": "Current session: 96% used"}),
    ];
    fs::write(&answer, lines.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    f.cmd()
        .env("FAKE_CLAUDE_USAGE_FILE", &answer)
        .args(["profile", "usage", "--refresh", "--json"])
        .assert()
        .success();

    let usage = json_out(f.cmd().env("FAKE_CLAUDE_USAGE_FILE", &answer).args(["profile", "usage", "--json"]));
    let v = json_out(f.cmd().env("FAKE_CLAUDE_USAGE_FILE", &answer).args(["recommend", "--role", "implementer", "--json"]));
    let windows = &candidate(&v, "claude-b")["windows"];
    assert_eq!(windows, &usage["profiles"][0]["windows"], "{v}");
    assert_eq!(windows[0]["name"], "five_hour", "{windows}");
    assert_eq!(candidate(&v, "claude-b")["eligible"], false, "{v}");

    let out = f
        .cmd()
        .env("FAKE_CLAUDE_USAGE_FILE", &answer)
        .env_remove("THOTH_LANG")
        .args(["recommend", "--role", "implementer"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|l| l.starts_with("claude-b  anthropic / claude-code  ")).unwrap_or_else(|| panic!("{text}"));
    assert!(lines[first].contains("ineligible exhausted  five_hour 96% used, resets 2099-01-01T00:00:00Z"), "{text}");
    assert!(lines[first + 1].starts_with("  reasons: ") && lines[first + 1].contains("at least 95%"), "{text}");
}
