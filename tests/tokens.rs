//! DT-1: every token kind a harness reports lands in the run's own columns.
//! Claude's results are per turn and add up across a resume; codex's and
//! antigravity's are cumulative and replace. Driven end to end through
//! `fake-claude`, `fake-codex` and `fake-agy`; never a real harness.

mod common;

use serde_json::Value;

const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";

type Six = [Option<i64>; 6];

fn json(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim()).unwrap()
}

/// input, cache_read, cache_write, output, reasoning, total of one run.
fn six(f: &common::Fixture, run_id: &str) -> Six {
    f.state_conn()
        .query_row(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                    reasoning_tokens, total_tokens FROM runs WHERE run_id = ?1",
            [run_id],
            |r| {
                Ok([
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ])
            },
        )
        .unwrap()
}

fn assert_six(f: &common::Fixture, run_id: &str, want: Six) {
    let got = six(f, run_id);
    for (i, name) in [
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "reasoning_tokens",
        "total_tokens",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(got[i], want[i], "{name}: got {got:?}, want {want:?}\n{}", f.explain_run(run_id));
    }
}

fn codex_setup() -> (common::Fixture, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    std::fs::write(
        cfg.join("models_cache.json"),
        serde_json::json!({"models": [{
            "slug": CODEX_MODEL,
            "display_name": "Luna",
            "visibility": "list",
            "priority": 1
        }]})
        .to_string(),
    )
    .unwrap();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, task_id)
}

// AC3 (a) ------------------------------------------------------------------

#[test]
fn a_single_claude_result_records_every_token_kind_and_the_total() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "ok", &["--model", "sonnet"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert_six(
        &f,
        &run_id,
        [Some(34), Some(202655), Some(1840), Some(610), Some(226), Some(205139)],
    );
}

// AC3 (b) ------------------------------------------------------------------

#[test]
fn the_two_segments_of_a_resumed_claude_run_add_up() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "quota", &["--model", "sonnet"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted", "{}", f.explain_run(&run_id));

    // the operator decides the window has reset
    f.cmd().args(["profile", "check", "claude-b"]).assert().success();

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &run_id, "--json"]);
    f.run_ok(c, "resume");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));

    let runs: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 1, "resume keeps the run");
    // each segment's result carries 34 / 202655 / 1840 / 610 / 226
    assert_six(
        &f,
        &run_id,
        [Some(68), Some(405310), Some(3680), Some(1220), Some(452), Some(410278)],
    );
}

// AC3 (c) ------------------------------------------------------------------

#[test]
fn a_codex_turn_records_its_kinds_and_a_total_without_cache_write() {
    let (f, task_id) = codex_setup();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    let run_id = json(&f.run_ok(c, "dispatch"))["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert_six(
        &f,
        &run_id,
        [Some(22534), Some(11008), Some(0), Some(157), Some(22), Some(22691)],
    );
}

// AC3 (d) ------------------------------------------------------------------

#[test]
fn a_resumed_codex_runs_cumulative_usage_replaces_instead_of_adding() {
    let (f, task_id) = codex_setup();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "quota")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium", "--json"]);
    let run_id = json(&f.run_ok(c, "dispatch"))["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted", "{}", f.explain_run(&run_id));
    // turn.failed carries no usage
    assert_six(&f, &run_id, [None; 6]);

    f.cmd().args(["profile", "check", "codex-a"]).assert().success();

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &run_id, "--json"]);
    f.run_ok(c, "resume");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert_six(
        &f,
        &run_id,
        [Some(22534), Some(11008), Some(0), Some(213), Some(22), Some(22747)],
    );
}

// AC3 (e) ------------------------------------------------------------------

#[test]
fn an_antigravity_result_records_its_kinds_with_no_cache_write() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.home_path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_AGY_DATA_DIR", &cfg)
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", AGY_MODEL, "--json"]);
    let run_id = json(&f.run_ok(c, "dispatch"))["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{}", f.explain_run(&run_id));
    assert_six(&f, &run_id, [Some(100), Some(0), None, Some(639), Some(0), Some(739)]);
}

// AC3 (f) ------------------------------------------------------------------

#[test]
fn models_breakdown_sums_the_new_token_kinds_and_a_null_counts_as_zero() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let c = f.state_conn();
    c.execute(
        "INSERT INTO tasks (task_id,title,spec,risk,status,base_commit,created_at,updated_at)
         VALUES ('t1','t1','{}','normal','completed','abc','2026-01-01','2026-01-01')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO runs (run_id,task_id,role,account,model,status,spec,input_tokens,
            output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,total_tokens)
         VALUES ('r1','t1','implementer','claude-a','sonnet','completed','{}',10,20,5,1,2,36)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO runs (run_id,task_id,role,account,model,status,spec,input_tokens,
            output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,total_tokens)
         VALUES ('r2','t1','implementer','claude-a','sonnet','completed','{}',11,21,NULL,NULL,NULL,NULL)",
        [],
    )
    .unwrap();
    drop(c);

    let out = f.cmd().args(["models", "--json"]).assert().success().get_output().stdout.clone();
    let v = json(&out);
    let profile = v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "claude-a")
        .unwrap_or_else(|| panic!("no claude-a in {v}"));
    let model = profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "sonnet")
        .unwrap_or_else(|| panic!("no sonnet in {profile}"));
    let groups = model["thoth_breakdown"].as_array().expect("thoth_breakdown");
    assert_eq!(groups.len(), 1, "{model}");
    let g = &groups[0];
    assert!(g["effort"].is_null() && g["task_type"].is_null() && g["difficulty"].is_null(), "{g}");
    // the keys that were there before keep their meaning
    assert_eq!(g["input_tokens"], 21, "{g}");
    assert_eq!(g["output_tokens"], 41, "{g}");
    // and the new ones sit beside them
    assert_eq!(g["cache_read_tokens"], 5, "{g}");
    assert_eq!(g["cache_write_tokens"], 1, "{g}");
    assert_eq!(g["reasoning_tokens"], 2, "{g}");
    assert_eq!(g["total_tokens"], 36, "{g}");
}
