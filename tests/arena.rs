//! RJ-2: the Agent Arena source (AC1) and its place in `thoth models` (AC9).
//! The Hugging Face Hub is a `file://` tree the test writes itself: the
//! dataset index at `api/datasets/lmarena-ai/leaderboard-dataset` and one
//! parquet file per board under `datasets/.../resolve/SHA/BOARD/`. The
//! parquet files are produced with the same crate Thoth reads them with
//! (SNAPPY, dictionary encoding), never copied from the real dataset.
//! Nothing here reaches the network or a real harness.

mod common;

use common::Fixture;
use parquet::basic::Compression;
use parquet::column::writer::ColumnWriter;
use parquet::data_type::ByteArray;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const DATASET: &str = "lmarena-ai/leaderboard-dataset";
const BOARD_FILE: &str = "latest-00000-of-00001.parquet";
const ASL_KEY: &str = "asl-fake-key-1";

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn file_url(path: &Path) -> String {
    let shown = path.to_string_lossy().replace('\\', "/");
    format!("file:///{}", shown.trim_start_matches('/'))
}

fn ago(hours: i64) -> String {
    (OffsetDateTime::now_utc() - Duration::hours(hours))
        .format(&Rfc3339)
        .unwrap()
}

/// One board row: (model_name, organization, score, rank).
type RowSpec<'a> = (&'a str, &'a str, f64, i64);

/// Writes a board as the dataset encodes it, plus a `license` column Thoth
/// must ignore.
fn write_board(path: &Path, rows: &[RowSpec]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let schema = Arc::new(
        parse_message_type(
            "message board {
                optional binary model_name (UTF8);
                optional binary organization (UTF8);
                optional binary license (UTF8);
                optional binary leaderboard_publish_date (UTF8);
                optional double score;
                optional double score_ci_lower;
                optional double score_ci_upper;
                optional double observation_count;
                optional int64 rank;
            }",
        )
        .unwrap(),
    );
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_dictionary_enabled(true)
            .build(),
    );
    let file = fs::File::create(path).unwrap();
    let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
    let mut group = writer.next_row_group().unwrap();
    let defined = vec![1i16; rows.len()];
    let strings: [Vec<&str>; 4] = [
        rows.iter().map(|r| r.0).collect(),
        rows.iter().map(|r| r.1).collect(),
        rows.iter().map(|_| "proprietary").collect(),
        rows.iter().map(|_| "2026-10-16").collect(),
    ];
    for values in &strings {
        let mut column = group.next_column().unwrap().unwrap();
        match column.untyped() {
            ColumnWriter::ByteArrayColumnWriter(w) => {
                let bytes: Vec<ByteArray> = values.iter().map(|s| ByteArray::from(*s)).collect();
                w.write_batch(&bytes, Some(&defined), None).unwrap();
            }
            _ => panic!("string column expected"),
        }
        column.close().unwrap();
    }
    let doubles: [Vec<f64>; 4] = [
        rows.iter().map(|r| r.2).collect(),
        rows.iter().map(|r| r.2 - 10.0).collect(),
        rows.iter().map(|r| r.2 + 10.0).collect(),
        rows.iter().map(|_| 100.0).collect(),
    ];
    for values in &doubles {
        let mut column = group.next_column().unwrap().unwrap();
        match column.untyped() {
            ColumnWriter::DoubleColumnWriter(w) => {
                w.write_batch(values, Some(&defined), None).unwrap();
            }
            _ => panic!("double column expected"),
        }
        column.close().unwrap();
    }
    let ranks: Vec<i64> = rows.iter().map(|r| r.3).collect();
    let mut column = group.next_column().unwrap().unwrap();
    match column.untyped() {
        ColumnWriter::Int64ColumnWriter(w) => {
            w.write_batch(&ranks, Some(&defined), None).unwrap();
        }
        _ => panic!("int64 column expected"),
    }
    column.close().unwrap();
    group.close().unwrap();
    writer.close().unwrap();
}

/// A fake Hugging Face Hub serving one version (`sha`) of the dataset with
/// the main board and the given subsets.
struct Hub {
    dir: TempDir,
    url: String,
}

impl Hub {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = file_url(dir.path());
        Self { dir, url }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.path().join("api").join("datasets").join("lmarena-ai").join("leaderboard-dataset")
    }

    fn board_path(&self, sha: &str, board: &str) -> PathBuf {
        self.dir
            .path()
            .join("datasets")
            .join("lmarena-ai")
            .join("leaderboard-dataset")
            .join("resolve")
            .join(sha)
            .join(board)
            .join(BOARD_FILE)
    }

    /// Writes the index for `sha` naming `agent` and `subsets` (plus a text
    /// board and a `full` file Thoth must ignore) and the parquet files.
    fn publish(&self, sha: &str, subsets: &[&str]) {
        let mut siblings = vec![
            json!({"rfilename": "README.md"}),
            json!({"rfilename": format!("agent/{BOARD_FILE}")}),
            json!({"rfilename": "agent/full-00000-of-00001.parquet"}),
            json!({"rfilename": format!("text/{BOARD_FILE}")}),
        ];
        for subset in subsets {
            siblings.push(json!({"rfilename": format!("{subset}/{BOARD_FILE}")}));
        }
        fs::create_dir_all(self.index_path().parent().unwrap()).unwrap();
        fs::write(
            self.index_path(),
            json!({"id": DATASET, "sha": sha, "siblings": siblings}).to_string(),
        )
        .unwrap();
        write_board(
            &self.board_path(sha, "agent"),
            &[
                ("GPT 6.1 Sol (Max)", "openai", 1500.5, 1),
                ("Claude Fable 5.1 (high)", "anthropic", 1490.25, 2),
                ("Mistral Big", "mistral", 1402.0, 3),
            ],
        );
        for subset in subsets {
            write_board(
                &self.board_path(sha, subset),
                &[("Claude Fable 5.1 (high)", "anthropic", 1300.0, 1), ("Only Here", "openai", 1200.0, 2)],
            );
        }
    }
}

fn arena_path(f: &Fixture) -> PathBuf {
    f.home_path().join("model-scores-arena.json")
}

fn read_cache(f: &Fixture) -> Value {
    serde_json::from_str(&fs::read_to_string(arena_path(f)).unwrap()).unwrap()
}

fn models_json(f: &Fixture, hub: &Hub, refresh: bool) -> (Value, String, i32) {
    let mut c = f.cmd();
    c.env("THOTH_ARENA_URL", &hub.url);
    c.arg("models");
    if refresh {
        c.arg("--refresh");
    }
    c.arg("--json");
    let out = c.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("stdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&out.stderr)));
    (v, String::from_utf8_lossy(&out.stderr).into_owned(), out.status.code().unwrap_or(-1))
}

fn switch_on(f: &Fixture) {
    f.cmd().args(["config", "set", "scores-auto-refresh", "on"]).assert().success();
}

// --- AC1 (a), AC9 ------------------------------------------------------------------------

#[test]
fn a_the_first_fetch_writes_the_cache_with_every_board_and_models_shows_it() {
    let f = setup();
    let hub = Hub::new();
    hub.publish("sha-one", &["agent_bash_reliability", "agent_a"]);

    // before: no cache, the stale line names no key
    let (v, _, code) = models_json(&f, &hub, false);
    assert_eq!(code, 0);
    let arena = &v["external"]["agent_arena"];
    assert_eq!(arena["cache"]["present"], false, "{v}");
    assert_eq!(arena["cache"]["stale"], true, "{v}");
    assert!(arena["cache"]["sha"].is_null(), "{v}");
    assert_eq!(arena["boards"], json!([]), "{v}");
    assert_eq!(arena["source"]["name"], "Agent Arena");
    assert_eq!(arena["source"]["url"], "https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset");
    assert_eq!(arena["source"]["license"], "CC BY 4.0");
    let plain = String::from_utf8(f.cmd().arg("models").assert().success().get_output().stdout.clone()).unwrap();
    let stale: Vec<&str> = plain.lines().filter(|l| l.starts_with("stale: no Agent Arena scores cached")).collect();
    assert_eq!(stale.len(), 1, "{plain}");
    assert!(stale[0].contains("thoth models --refresh") && stale[0].contains("scores-auto-refresh on"), "{plain}");
    assert!(!stale[0].contains("API_KEY"), "{plain}");

    // --refresh without any key: exit 0, Agent Arena fetched
    let (v, stderr, code) = models_json(&f, &hub, true);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("AISTUPIDLEVEL_API_KEY") && stderr.contains("skipped"), "{stderr}");
    assert!(!stderr.contains("Agent Arena refresh failed"), "{stderr}");
    let arena = &v["external"]["agent_arena"];
    assert_eq!(arena["cache"]["present"], true, "{v}");
    assert_eq!(arena["cache"]["refreshed"], true, "{v}");
    assert_eq!(arena["cache"]["stale"], false, "{v}");
    assert_eq!(arena["cache"]["age_hours"], 0, "{v}");
    assert_eq!(arena["cache"]["sha"], "sha-one", "{v}");
    assert_eq!(arena["boards"], json!(["agent", "agent_a", "agent_bash_reliability"]), "{v}");
    assert!(arena.get("rows").is_none() && arena["boards"][0].is_string(), "no rows in models: {v}");
    assert_eq!(arena["cache"]["path"].as_str().unwrap().replace('\\', "/"), arena_path(&f).to_string_lossy().replace('\\', "/"));

    let cache = read_cache(&f);
    assert_eq!(cache["schema"], "agent-arena-v1");
    assert_eq!(cache["sha"], "sha-one");
    assert_eq!(cache["source"]["name"], "Agent Arena");
    assert_eq!(cache["source"]["url"], "https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset");
    assert_eq!(cache["source"]["license"], "CC BY 4.0");
    assert_eq!(cache["source"]["license_url"], "https://creativecommons.org/licenses/by/4.0/");
    let boards = cache["boards"].as_object().unwrap();
    let mut names: Vec<&str> = boards.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["agent", "agent_a", "agent_bash_reliability"]);
    assert_eq!(cache["boards"]["agent"]["publish_date"], "2026-10-16");
    let rows = cache["boards"]["agent"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "every row is stored; filtering happens in recommend");
    assert_eq!(rows[2]["organization"], "mistral");
    assert_eq!(rows[0]["model_name"], "GPT 6.1 Sol (Max)");
    assert_eq!(rows[0]["score"], 1500.5);
    assert_eq!(rows[0]["rank"], 1);
    assert_eq!(rows[0]["observation_count"], 100.0);
    assert!(rows[0].get("license").is_none(), "{cache}");
    assert_eq!(cache["boards"]["agent_a"]["rows"].as_array().unwrap().len(), 2);
    for name in fs::read_dir(f.home_path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()) {
        assert!(!name.ends_with(".tmp"), "leftover {name}");
    }

    let plain = String::from_utf8(f.cmd().arg("models").assert().success().get_output().stdout.clone()).unwrap();
    assert!(
        plain.lines().any(|l| l == "scores: Agent Arena https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset (CC BY 4.0)"),
        "{plain}"
    );
    assert!(!plain.lines().any(|l| l.starts_with("stale: no Agent Arena")), "{plain}");

    // a second --refresh with the same sha: no download (the files are gone),
    // the cache is touched
    fs::remove_dir_all(hub.dir.path().join("datasets")).unwrap();
    let mut cache = read_cache(&f);
    cache["fetched_at"] = json!(ago(30));
    fs::write(arena_path(&f), cache.to_string()).unwrap();
    let (v, stderr, code) = models_json(&f, &hub, true);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("Agent Arena refresh failed"), "{stderr}");
    assert_eq!(v["external"]["agent_arena"]["cache"]["refreshed"], false, "same sha is not a refresh: {v}");
    assert_eq!(v["external"]["agent_arena"]["cache"]["age_hours"], 0, "{v}");
    let touched = read_cache(&f);
    assert_eq!(touched["sha"], "sha-one");
    assert_eq!(touched["boards"]["agent"]["rows"].as_array().unwrap().len(), 3);
}

// --- AC1 (b): the automatic refresh and an unchanged sha -----------------------------

#[test]
fn b_the_automatic_refresh_touches_an_unchanged_sha_and_downloads_a_new_one() {
    let f = setup();
    let hub = Hub::new();
    hub.publish("sha-one", &["agent_a"]);
    switch_on(&f);
    let (v, stderr, code) = models_json(&f, &hub, false);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(v["auto_refresh"]["attempted"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["refreshed"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["failed"], json!([]), "{v}");
    assert_eq!(v["external"]["agent_arena"]["cache"]["refreshed"], true, "{v}");
    assert_eq!(stderr, "");
    let plain = String::from_utf8(f.cmd().env("THOTH_ARENA_URL", &hub.url).arg("models").assert().success().get_output().stdout.clone()).unwrap();
    assert!(!plain.contains("auto-refresh:"), "fresh now, nothing to refresh: {plain}");

    // a day later, the same sha: the parquet files are gone, yet it succeeds
    fs::remove_dir_all(hub.dir.path().join("datasets")).unwrap();
    let mut cache = read_cache(&f);
    let old_at = ago(25);
    cache["fetched_at"] = json!(old_at);
    fs::write(arena_path(&f), cache.to_string()).unwrap();
    let (v, stderr, _) = models_json(&f, &hub, false);
    assert_eq!(v["auto_refresh"]["attempted"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["refreshed"], json!([]), "{v}");
    assert_eq!(v["auto_refresh"]["failed"], json!([]), "{v}");
    assert_eq!(stderr, "");
    let touched = read_cache(&f);
    assert!(touched["fetched_at"].as_str().unwrap() > old_at.as_str(), "{touched}");
    assert_eq!(touched["sha"], "sha-one");
    assert_eq!(v["external"]["agent_arena"]["cache"]["age_hours"], 0, "{v}");

    // not due: nothing attempted
    let (v, _, _) = models_json(&f, &hub, false);
    assert_eq!(v["auto_refresh"]["attempted"], json!([]), "{v}");

    // a new sha a day later: downloaded whole, boards follow the new index
    hub.publish("sha-two", &["agent_b", "agent_a"]);
    let mut cache = read_cache(&f);
    cache["fetched_at"] = json!(ago(25));
    fs::write(arena_path(&f), cache.to_string()).unwrap();
    let (v, _, _) = models_json(&f, &hub, false);
    assert_eq!(v["auto_refresh"]["refreshed"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["external"]["agent_arena"]["cache"]["sha"], "sha-two", "{v}");
    assert_eq!(v["external"]["agent_arena"]["boards"], json!(["agent", "agent_a", "agent_b"]), "{v}");
}

// --- AC1 (c): failures leave the old cache alone -------------------------------------

#[test]
fn c_a_broken_or_missing_board_keeps_the_old_cache_and_only_warns() {
    let f = setup();
    let hub = Hub::new();
    hub.publish("sha-one", &["agent_a"]);
    let (_, _, code) = models_json(&f, &hub, true);
    assert_eq!(code, 0);
    let good = fs::read(arena_path(&f)).unwrap();

    // a new version whose subset board is garbage
    hub.publish("sha-two", &["agent_a"]);
    fs::write(hub.board_path("sha-two", "agent_a"), b"not a parquet file at all").unwrap();
    let mut cache = read_cache(&f);
    cache["fetched_at"] = json!(ago(25));
    fs::write(arena_path(&f), cache.to_string()).unwrap();
    let aged = fs::read(arena_path(&f)).unwrap();
    switch_on(&f);
    let (v, stderr, code) = models_json(&f, &hub, false);
    assert_eq!(code, 0);
    assert_eq!(v["auto_refresh"]["attempted"], json!(["Agent Arena"]), "{v}");
    assert_eq!(v["auto_refresh"]["refreshed"], json!([]), "{v}");
    let failed = v["auto_refresh"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{v}");
    assert_eq!(failed[0]["source"], "Agent Arena");
    assert!(failed[0]["error"].as_str().unwrap().contains("agent_a"), "{v}");
    assert_eq!(stderr.lines().filter(|l| l.contains("automatic score refresh failed")).count(), 1, "{stderr}");
    assert_eq!(fs::read(arena_path(&f)).unwrap(), aged, "not a byte of the old cache changed");
    assert_eq!(v["external"]["agent_arena"]["cache"]["sha"], "sha-one", "{v}");
    assert_eq!(v["external"]["agent_arena"]["cache"]["present"], true, "{v}");
    for name in fs::read_dir(f.home_path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()) {
        assert!(!name.ends_with(".tmp"), "leftover {name}");
        assert_ne!(name, ".model-scores.refresh.lock");
    }

    // the same through --refresh: exit 0, one warning
    let (v, stderr, code) = models_json(&f, &hub, true);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("warning: Agent Arena refresh failed"), "{stderr}");
    assert_eq!(fs::read(arena_path(&f)).unwrap(), aged);
    assert_eq!(v["external"]["agent_arena"]["cache"]["refreshed"], false, "{v}");

    // a missing board file (the index names it, the file is not there)
    fs::remove_file(hub.board_path("sha-two", "agent_a")).unwrap();
    let (_, stderr, code) = models_json(&f, &hub, true);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("warning: Agent Arena refresh failed"), "{stderr}");
    assert_eq!(fs::read(arena_path(&f)).unwrap(), aged);

    // a keyed source failing in the same call still exits 1
    let out = f
        .cmd()
        .env("THOTH_ARENA_URL", &hub.url)
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", "file:///definitely/missing")
        .args(["models", "--refresh"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("AI Stupid Level refresh failed"), "{stderr}");
    assert!(stderr.contains("warning: Agent Arena refresh failed"), "{stderr}");
    assert!(!stderr.contains(ASL_KEY));
    assert_eq!(fs::read(arena_path(&f)).unwrap(), aged);
    let _ = good;
}

// --- AC1 (d): no fetch inside a worker or with the switch off ----------------------------

#[test]
fn d_a_worker_or_the_switch_off_never_fetches_agent_arena() {
    let f = setup();
    let hub = Hub::new();
    hub.publish("sha-one", &[]);
    let (v, stderr, _) = models_json(&f, &hub, false);
    assert_eq!(v["auto_refresh"]["skipped"], "off", "{v}");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]), "{v}");
    assert!(!arena_path(&f).exists());
    assert_eq!(stderr, "");

    switch_on(&f);
    let mut c = f.cmd();
    c.env("THOTH_ARENA_URL", &hub.url).env("THOTH_RUN_ID", "01RUN").args(["models", "--json"]);
    let out = c.assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["auto_refresh"]["skipped"], "worker", "{v}");
    assert_eq!(v["auto_refresh"]["attempted"], json!([]), "{v}");
    assert!(!arena_path(&f).exists());

    // and `report` shares the refresh: with the switch on it fetches
    f.cmd().env("THOTH_ARENA_URL", &hub.url).args(["report", "--json"]).assert().success();
    assert!(arena_path(&f).exists());
}

// --- AC1 (1): the setting ----------------------------------------------------------------

#[test]
fn e_arena_url_is_a_text_setting_after_aa_url() {
    let f = common::fixture();
    let out = f.cmd().env_remove("THOTH_ARENA_URL").args(["config", "list", "--json"]).assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 16, "{v}");
    let at = settings.iter().position(|s| s["key"] == "arena-url").unwrap();
    assert_eq!(settings[at - 1]["key"], "aa-url", "{v}");
    assert_eq!(settings[at]["kind"], "text");
    assert_eq!(settings[at]["env"], "THOTH_ARENA_URL");
    assert_eq!(settings[at]["default"], "https://huggingface.co");
    assert_eq!(settings[at]["value"], "https://huggingface.co");
    assert_eq!(settings[at]["source"], "default");
    assert_eq!(settings[at]["settable"], true);
    f.cmd().args(["config", "set", "arena-url", "https://mirror.example/hub/", "--json"]).assert().success();
    let config: Value = serde_json::from_str(&fs::read_to_string(f.home_path().join("config.json")).unwrap()).unwrap();
    assert_eq!(config["arena_url"], "https://mirror.example/hub/");
    let out = f.cmd().env_remove("THOTH_ARENA_URL").args(["config", "get", "arena-url", "--json"]).assert().success().get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["value"], "https://mirror.example/hub/");
    f.cmd().args(["config", "set", "arena-url", " ", "--json"]).assert().failure();
}

// --- AC1 (2): which fetch follows redirects, through the real fetch entry -----------------

const AA_KEY: &str = "aa-fake-key-1";

/// A `file://` URL of a path under the package root (the tests' working
/// directory), as tests/models.rs builds its fixture URLs.
fn fixture_url(relative: &str) -> String {
    let p = fs::canonicalize(relative).unwrap();
    let shown = p.to_string_lossy();
    let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown).replace('\\', "/");
    format!("file:///{}", shown.trim_start_matches('/'))
}

/// Review of attempt 2: checked through `thoth models --refresh` itself, not
/// on `curl_args`. A `curl` stand-in (src/bin/fake-curl.rs) alone on PATH
/// records every argument list the real fetch code starts it with and
/// serves the file:// trees, so the whole refresh (AI Stupid Level, both
/// Artificial Analysis pages, the Agent Arena index and both boards)
/// succeeds. Only the three Agent Arena requests follow redirects (`-L`),
/// and they carry no header; the three keyed requests read their header from
/// stdin (`-H @-`), never follow a redirect, and never show a key in an
/// argument.
#[test]
fn f_only_the_agent_arena_requests_of_the_real_fetch_follow_redirects() {
    let f = setup();
    let hub = Hub::new();
    hub.publish("sha-one", &["agent_a"]);
    // The stand-in is `curl` in a directory of its own. Linux finds it
    // through PATH; Windows looks in the running program's own directory
    // (and the system directories, where a real curl.exe lives) before PATH,
    // so thoth runs from a copy placed next to the stand-in.
    let bin = tempfile::tempdir().unwrap();
    let exe = std::env::consts::EXE_SUFFIX;
    fs::copy(env!("CARGO_BIN_EXE_fake-curl"), bin.path().join(format!("curl{exe}"))).unwrap();
    let thoth = bin.path().join(format!("thoth{exe}"));
    fs::copy(env!("CARGO_BIN_EXE_thoth"), &thoth).unwrap();
    let log = f.home_path().join("fake-curl.jsonl");
    let asl_url = fixture_url("tests/fixtures/aistupidlevel/v1-models.json");
    let aa_base = format!("{}/page-", fixture_url("tests/fixtures/artificialanalysis"));

    let mut c = std::process::Command::new(&thoth);
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME"] {
        c.env_remove(var);
    }
    let out = c
        .current_dir(f.repo.path())
        .env("THOTH_HOME", f.home.path())
        .env("PATH", bin.path())
        .env("FAKE_CURL_LOG", &log)
        .env("THOTH_ARENA_URL", &hub.url)
        .env("AISTUPIDLEVEL_API_KEY", ASL_KEY)
        .env("THOTH_SCORES_URL", &asl_url)
        .env("ARTIFICIALANALYSIS_API_KEY", AA_KEY)
        .env("THOTH_AA_URL", &aa_base)
        .args(["models", "--refresh", "--json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["cache"]["refreshed"], true, "AI Stupid Level through the stand-in: {v}");
    for source in ["artificial_analysis", "agent_arena"] {
        assert_eq!(v["external"][source]["cache"]["refreshed"], true, "{source} through the stand-in: {v}");
    }
    assert_eq!(v["external"]["agent_arena"]["boards"], json!(["agent", "agent_a"]), "{v}");
    assert_eq!(read_cache(&f)["sha"], "sha-one");
    assert!(!stderr.contains("refresh failed"), "{stderr}");

    let calls: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let args = |c: &Value| -> Vec<String> {
        c["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
    };
    let url_of = |c: &Value| args(c).last().cloned().unwrap();
    let has = |c: &Value, flag: &str| args(c).iter().any(|a| a == flag);
    for c in &calls {
        assert!(has(c, "-sS") && has(c, "--fail") && has(c, "--max-time") && has(c, "-o"), "{c}");
        let text = c.to_string();
        assert!(!text.contains(ASL_KEY) && !text.contains(AA_KEY), "a key in an argument: {c}");
    }

    let arena: Vec<&Value> = calls.iter().filter(|c| url_of(c).starts_with(&hub.url)).collect();
    assert_eq!(
        arena.iter().map(|c| url_of(c)).collect::<Vec<_>>(),
        [
            format!("{}/api/datasets/{DATASET}", hub.url),
            format!("{}/datasets/{DATASET}/resolve/sha-one/agent/{BOARD_FILE}", hub.url),
            format!("{}/datasets/{DATASET}/resolve/sha-one/agent_a/{BOARD_FILE}", hub.url),
        ],
        "the index, then each board of that sha: {calls:?}"
    );
    for c in &arena {
        assert!(has(c, "-L"), "a public request follows redirects: {c}");
        assert!(!has(c, "-H") && !has(c, "@-"), "and sends no header: {c}");
        assert_eq!(c["stdin_bytes"], 0, "{c}");
    }

    let keyed: Vec<&Value> = calls.iter().filter(|c| !url_of(c).starts_with(&hub.url)).collect();
    assert_eq!(
        keyed.iter().map(|c| url_of(c)).collect::<Vec<_>>(),
        [asl_url.clone(), format!("{aa_base}1"), format!("{aa_base}2")],
        "{calls:?}"
    );
    for c in &keyed {
        assert!(!has(c, "-L") && !has(c, "--location"), "a keyed request never follows a redirect: {c}");
        let a = args(c);
        let at = a.iter().position(|x| x == "-H").unwrap_or_else(|| panic!("{c}"));
        assert_eq!(a[at + 1], "@-", "the header comes through stdin: {c}");
        assert!(c["stdin_bytes"].as_u64().unwrap() > 0, "{c}");
    }
    assert_eq!(calls.len(), 6, "{calls:?}");
}
