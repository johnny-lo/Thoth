//! Agent Arena (RJ-2): the Hugging Face dataset `lmarena-ai/leaderboard-dataset`
//! (CC BY 4.0), fetched without a key through the same curl child process as
//! the other score sources and kept whole in `$THOTH_HOME/model-scores-arena.json`.
//! The filtering happens when `recommend` sends the table, not here.

use crate::commands::models::{self, CacheFile, MANUAL_FETCH_MAX_TIME_SECONDS};
use crate::config::Paths;
use anyhow::{anyhow, bail, Context, Result};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub(crate) const NAME: &str = "Agent Arena";
pub(crate) const DEFAULT_ARENA_URL: &str = "https://huggingface.co";
pub(crate) const SCHEMA: &str = "agent-arena-v1";
const DATASET: &str = "lmarena-ai/leaderboard-dataset";
/// Fixed, like `AA_SOURCE_URL`: the citation does not follow `arena-url`.
pub(crate) const SOURCE_URL: &str = "https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset";
pub(crate) const LICENSE: &str = "CC BY 4.0";
pub(crate) const LICENSE_URL: &str = "https://creativecommons.org/licenses/by/4.0/";
pub(crate) const MAIN_BOARD: &str = "agent";
const BOARD_FILE: &str = "latest-00000-of-00001.parquet";
/// The whole automatic refresh (index plus every board) shares this limit.
pub(crate) const AUTO_BUDGET_SECONDS: f64 = 20.0;
const REQUIRED_COLUMNS: [&str; 4] = ["model_name", "organization", "score", "rank"];
const KEPT_COLUMNS: [&str; 7] = [
    "model_name",
    "organization",
    "score",
    "score_ci_lower",
    "score_ci_upper",
    "observation_count",
    "rank",
];
const PUBLISH_DATE_COLUMN: &str = "leaderboard_publish_date";

pub(crate) fn cache_path(paths: &Paths) -> PathBuf {
    paths.home.join("model-scores-arena.json")
}

pub(crate) fn read_cache(paths: &Paths) -> CacheFile {
    models::read_cache_file(cache_path(paths), SCHEMA)
}

/// The board names of a cache, sorted.
pub(crate) fn board_names(cache: &CacheFile) -> Vec<String> {
    cache
        .value
        .as_ref()
        .and_then(|v| v.get("boards"))
        .and_then(Value::as_object)
        .map(|boards| boards.keys().cloned().collect())
        .unwrap_or_default()
}

pub(crate) fn cached_sha(cache: &CacheFile) -> Option<String> {
    cache
        .value
        .as_ref()
        .and_then(|v| v.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// --- the time budget ---------------------------------------------------------

/// How long the whole refresh may take: unlimited for `models --refresh`
/// (each curl gets `MANUAL_FETCH_MAX_TIME_SECONDS`), a deadline for the
/// automatic refresh.
pub(crate) struct Budget {
    deadline: Option<Instant>,
}

impl Budget {
    pub(crate) fn unlimited() -> Self {
        Self { deadline: None }
    }

    pub(crate) fn seconds(total: f64) -> Self {
        Self { deadline: Some(Instant::now() + Duration::from_secs_f64(total)) }
    }

    /// curl's `--max-time` for the next request; `None` when less than a
    /// second is left, so nothing more is fetched.
    fn max_time(&self) -> Option<u64> {
        match self.deadline {
            None => Some(MANUAL_FETCH_MAX_TIME_SECONDS),
            Some(deadline) => max_time_for(deadline.saturating_duration_since(Instant::now())),
        }
    }
}

/// The remaining budget as whole seconds, rounded up; `None` under a second.
pub(crate) fn max_time_for(remaining: Duration) -> Option<u64> {
    if remaining < Duration::from_secs(1) {
        return None;
    }
    Some(remaining.as_secs_f64().ceil() as u64)
}

fn out_of_time() -> anyhow::Error {
    anyhow!(
        "the {}-second limit of the automatic refresh ran out; the boards not fetched yet are left for the next call",
        AUTO_BUDGET_SECONDS as u64
    )
}

// --- the dataset index -------------------------------------------------------

/// `agent` plus every `agent_*` folder that has a `latest` parquet file,
/// sorted by name.
pub(crate) fn board_configs(siblings: &[Value]) -> Vec<String> {
    let mut configs = vec![MAIN_BOARD.to_string()];
    let mut subsets: Vec<String> = siblings
        .iter()
        .filter_map(|s| s.get("rfilename").and_then(Value::as_str))
        .filter_map(|name| {
            let (dir, file) = name.split_once('/')?;
            (file == BOARD_FILE && dir.starts_with("agent_")).then(|| dir.to_string())
        })
        .collect();
    subsets.sort();
    subsets.dedup();
    configs.extend(subsets);
    configs
}

/// Fetches the boards one after another; a board that cannot be fetched or
/// read stops the loop, and so does a budget with less than a second left.
pub(crate) fn download_boards(
    configs: &[String],
    mut next_max_time: impl FnMut() -> Option<u64>,
    mut fetch_one: impl FnMut(&str, u64) -> Result<Value>,
) -> Result<Map<String, Value>> {
    let mut boards = Map::new();
    for config in configs {
        let max_time = next_max_time().ok_or_else(out_of_time)?;
        let board = fetch_one(config, max_time)?;
        boards.insert(config.clone(), board);
    }
    Ok(boards)
}

// --- parquet -----------------------------------------------------------------

fn field_to_json(field: &Field) -> Value {
    match field {
        Field::Null => Value::Null,
        Field::Bool(b) => json!(b),
        Field::Byte(n) => json!(n),
        Field::Short(n) => json!(n),
        Field::Int(n) => json!(n),
        Field::Long(n) => json!(n),
        Field::UByte(n) => json!(n),
        Field::UShort(n) => json!(n),
        Field::UInt(n) => json!(n),
        Field::ULong(n) => json!(n),
        Field::Float(f) => json!(f),
        Field::Double(f) => json!(f),
        Field::Str(s) => json!(s),
        // Other types (dates, decimals, nested values) are not columns this
        // board uses; a kept column of such a type is recorded as null.
        _ => Value::Null,
    }
}

/// The rows of one board file as `{column: value}` objects limited to the
/// columns Thoth keeps. The four required columns must be in the file's
/// schema (a file with no row is checked the same way) and set in every
/// row; the others are null when the file lacks them.
pub(crate) fn read_board_rows(path: &Path) -> Result<Vec<Map<String, Value>>> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = SerializedFileReader::new(file).map_err(|e| anyhow!("not a readable parquet file: {e}"))?;
    let columns: Vec<&str> = reader
        .metadata()
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .map(|c| c.name())
        .collect();
    for column in REQUIRED_COLUMNS {
        if !columns.contains(&column) {
            bail!(
                "the file has no {column} column; the required columns are {}",
                REQUIRED_COLUMNS.join(", ")
            );
        }
    }
    let mut rows = Vec::new();
    for (index, row) in reader
        .get_row_iter(None)
        .map_err(|e| anyhow!("reading parquet rows: {e}"))?
        .enumerate()
    {
        let row = row.map_err(|e| anyhow!("reading parquet row {index}: {e}"))?;
        let mut object = Map::new();
        for (name, field) in row.get_column_iter() {
            if KEPT_COLUMNS.contains(&name.as_str()) || name == PUBLISH_DATE_COLUMN {
                object.insert(name.clone(), field_to_json(field));
            }
        }
        for column in REQUIRED_COLUMNS {
            if object.get(column).is_none_or(Value::is_null) {
                bail!("row {index} has no {column}; the required columns are {}", REQUIRED_COLUMNS.join(", "));
            }
        }
        for column in KEPT_COLUMNS {
            object.entry(column).or_insert(Value::Null);
        }
        rows.push(object);
    }
    Ok(rows)
}

/// One board as stored in the cache: its publish date and its rows.
fn board_json(rows: Vec<Map<String, Value>>) -> Value {
    let publish_date = rows
        .first()
        .and_then(|r| r.get(PUBLISH_DATE_COLUMN))
        .cloned()
        .unwrap_or(Value::Null);
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut row| {
            row.remove(PUBLISH_DATE_COLUMN);
            Value::Object(row)
        })
        .collect();
    json!({"publish_date": publish_date, "rows": rows})
}

// --- the refresh -------------------------------------------------------------

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

/// Refreshes the cache. `Ok(true)`: a new version of the dataset was
/// downloaded and the cache replaced; `Ok(false)`: the cached `sha` is still
/// current, only `fetched_at` moved; `Err`: nothing was changed.
pub(crate) fn refresh(paths: &Paths, budget: &Budget) -> Result<bool> {
    let base = crate::commands::settings::effective("arena-url");
    let base = base.trim_end_matches('/').to_string();
    let api_url = format!("{base}/api/datasets/{DATASET}");
    let nonce = format!(
        "{}.{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let api_tmp = paths.home.join(format!(".model-scores-arena.api.{nonce}.tmp"));
    let cache_tmp = paths.home.join(format!(".model-scores-arena.{nonce}.tmp"));
    let board_tmp = |config: &str| paths.home.join(format!(".model-scores-arena.{config}.{nonce}.tmp"));
    let target = cache_path(paths);
    let result = (|| -> Result<bool> {
        let max_time = budget.max_time().ok_or_else(out_of_time)?;
        models::fetch_public(&api_url, &api_tmp, max_time)?;
        let index: Value = serde_json::from_slice(&fs::read(&api_tmp)?)
            .with_context(|| format!("invalid JSON from {api_url}"))?;
        let sha = index
            .get("sha")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("invalid dataset index from {api_url}: expected a sha string"))?
            .to_string();
        let siblings = index
            .get("siblings")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("invalid dataset index from {api_url}: expected a siblings array"))?;
        let configs = board_configs(siblings);
        let cached = read_cache(paths);
        if cached.present && cached_sha(&cached).as_deref() == Some(sha.as_str()) {
            let mut value = cached.value.expect("present caches have a value");
            value["fetched_at"] = json!(now_rfc3339()?);
            fs::write(&cache_tmp, serde_json::to_vec_pretty(&value)?)?;
            models::replace_cache(&cache_tmp, &target)?;
            return Ok(false);
        }
        let boards = download_boards(&configs, || budget.max_time(), |config, max_time| {
            let url = format!("{base}/datasets/{DATASET}/resolve/{sha}/{config}/{BOARD_FILE}");
            let tmp = board_tmp(config);
            let fetched = (|| -> Result<Value> {
                models::fetch_public(&url, &tmp, max_time)?;
                let rows = read_board_rows(&tmp).with_context(|| format!("board {config} from {url}"))?;
                Ok(board_json(rows))
            })();
            let _ = fs::remove_file(&tmp);
            fetched
        })?;
        let body = json!({
            "schema": SCHEMA,
            "fetched_at": now_rfc3339()?,
            "sha": sha,
            "source": {"name": NAME, "url": SOURCE_URL, "license": LICENSE, "license_url": LICENSE_URL},
            "boards": boards,
        });
        fs::write(&cache_tmp, serde_json::to_vec_pretty(&body)?)?;
        models::replace_cache(&cache_tmp, &target)?;
        Ok(true)
    })();
    for path in [&api_tmp, &cache_tmp] {
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::basic::Compression;
    use parquet::column::writer::ColumnWriter;
    use parquet::data_type::ByteArray;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    /// Writes a board the way the dataset is encoded (SNAPPY, dictionary
    /// encoding on); `with_rank` false leaves the rank column out.
    fn write_board(path: &Path, with_rank: bool) {
        write_board_rows(path, with_rank, 3);
    }

    /// `write_board` with the first `count` of the three rows (0 for a file
    /// that has the schema but no row).
    fn write_board_rows(path: &Path, with_rank: bool, count: usize) {
        let rank_column = if with_rank { "optional int64 rank;" } else { "" };
        let message = format!(
            "message board {{
                optional binary model_name (UTF8);
                optional binary organization (UTF8);
                optional binary license (UTF8);
                optional binary leaderboard_publish_date (UTF8);
                optional double score;
                optional double score_ci_lower;
                optional double observation_count;
                {rank_column}
            }}"
        );
        let schema = Arc::new(parse_message_type(&message).unwrap());
        let props = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .set_dictionary_enabled(true)
                .build(),
        );
        let file = fs::File::create(path).unwrap();
        let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
        let mut group = writer.next_row_group().unwrap();
        let strings: [&[&str]; 4] = [
            &["GPT 6.1 Sol (Max)", "Claude Fable 5.1 (high)", "Gemini 3.8 Pro"],
            &["openai", "anthropic", "google"],
            &["proprietary", "proprietary", "proprietary"],
            &["2026-10-16", "2026-10-16", "2026-10-16"],
        ];
        let defined = &[1i16, 1, 1][..count];
        for values in strings {
            let mut column = group.next_column().unwrap().unwrap();
            if let ColumnWriter::ByteArrayColumnWriter(w) = column.untyped() {
                let bytes: Vec<ByteArray> = values[..count].iter().map(|s| ByteArray::from(*s)).collect();
                w.write_batch(&bytes, Some(defined), None).unwrap();
            } else {
                panic!("string column expected");
            }
            column.close().unwrap();
        }
        for values in [[1500.5, 1490.25, 1402.0], [1480.0, 1470.0, 1390.0], [1200.0, 900.0, 300.0]] {
            let mut column = group.next_column().unwrap().unwrap();
            if let ColumnWriter::DoubleColumnWriter(w) = column.untyped() {
                w.write_batch(&values[..count], Some(defined), None).unwrap();
            } else {
                panic!("double column expected");
            }
            column.close().unwrap();
        }
        if with_rank {
            let mut column = group.next_column().unwrap().unwrap();
            if let ColumnWriter::Int64ColumnWriter(w) = column.untyped() {
                w.write_batch(&[1i64, 2, 3][..count], Some(defined), None).unwrap();
            } else {
                panic!("int64 column expected");
            }
            column.close().unwrap();
        }
        group.close().unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn a_generated_board_reads_back_with_kept_columns_and_nulls_for_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.parquet");
        write_board(&path, true);
        let rows = read_board_rows(&path).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["model_name"], "GPT 6.1 Sol (Max)");
        assert_eq!(rows[0]["organization"], "openai");
        assert_eq!(rows[0]["score"], 1500.5);
        assert_eq!(rows[0]["score_ci_lower"], 1480.0);
        assert_eq!(rows[0]["observation_count"], 1200.0);
        assert_eq!(rows[0]["rank"], 1);
        assert_eq!(rows[2]["rank"], 3);
        assert!(rows[0]["score_ci_upper"].is_null(), "a kept column the file lacks is null");
        assert!(rows[0].get("license").is_none(), "an extra column is ignored");
        assert_eq!(rows[0][PUBLISH_DATE_COLUMN], "2026-10-16");
        let board = board_json(rows);
        assert_eq!(board["publish_date"], "2026-10-16");
        assert!(board["rows"][0].get(PUBLISH_DATE_COLUMN).is_none());
        let mut keys: Vec<&str> = board["rows"][1].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut want = KEPT_COLUMNS.to_vec();
        want.sort_unstable();
        assert_eq!(keys, want);
    }

    #[test]
    fn a_board_without_rank_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-rank.parquet");
        write_board(&path, false);
        let error = read_board_rows(&path).unwrap_err().to_string();
        assert!(error.contains("rank"), "{error}");
    }

    /// Review of attempt 1: the required columns are a property of the file's
    /// schema, so a board with no row at all and no `rank` column is refused
    /// too (it must never replace the cache), while a complete schema with no
    /// row reads as an empty board.
    #[test]
    fn a_board_with_no_rows_and_no_rank_column_is_an_error_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-no-rank.parquet");
        write_board_rows(&path, false, 0);
        let error = read_board_rows(&path).unwrap_err().to_string();
        assert!(error.contains("rank"), "{error}");

        let complete = dir.path().join("empty.parquet");
        write_board_rows(&complete, true, 0);
        assert_eq!(read_board_rows(&complete).unwrap().len(), 0);
    }

    #[test]
    fn a_truncated_board_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.parquet");
        write_board(&path, true);
        let bytes = fs::read(&path).unwrap();
        let cut = dir.path().join("cut.parquet");
        fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        assert!(read_board_rows(&cut).is_err());
        fs::write(&cut, &bytes[..7]).unwrap();
        assert!(read_board_rows(&cut).is_err());
        assert!(read_board_rows(&dir.path().join("absent.parquet")).is_err());
    }

    #[test]
    fn the_budget_rounds_up_and_stops_under_a_second() {
        assert_eq!(max_time_for(Duration::from_secs(20)), Some(20));
        assert_eq!(max_time_for(Duration::from_millis(12_300)), Some(13));
        assert_eq!(max_time_for(Duration::from_millis(1_000)), Some(1));
        assert_eq!(max_time_for(Duration::from_millis(400)), None);
        assert_eq!(max_time_for(Duration::ZERO), None);
        assert_eq!(Budget::unlimited().max_time(), Some(MANUAL_FETCH_MAX_TIME_SECONDS));
    }

    #[test]
    fn a_budget_used_up_midway_stops_before_the_next_board() {
        let configs: Vec<String> = ["agent", "agent_a", "agent_b"].iter().map(|s| s.to_string()).collect();
        let mut budgets = vec![Some(20u64), Some(3), None].into_iter();
        let mut fetched = Vec::new();
        let error = download_boards(
            &configs,
            || budgets.next().unwrap(),
            |config, max_time| {
                fetched.push((config.to_string(), max_time));
                Ok(json!({"publish_date": null, "rows": []}))
            },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(fetched, vec![("agent".to_string(), 20), ("agent_a".to_string(), 3)]);
        assert!(error.contains("ran out"), "{error}");

        let mut none = std::iter::repeat(None::<u64>);
        let mut calls = 0;
        let error = download_boards(&configs, || none.next().unwrap(), |_, _| {
            calls += 1;
            Ok(Value::Null)
        })
        .unwrap_err()
        .to_string();
        assert_eq!(calls, 0, "no curl when less than a second is left");
        assert!(error.contains("ran out"), "{error}");
    }

    #[test]
    fn board_configs_are_agent_plus_sorted_agent_subsets() {
        let siblings = vec![
            json!({"rfilename": "text/latest-00000-of-00001.parquet"}),
            json!({"rfilename": "agent_bash_reliability/latest-00000-of-00001.parquet"}),
            json!({"rfilename": "agent/latest-00000-of-00001.parquet"}),
            json!({"rfilename": "agent_a/full-00000-of-00001.parquet"}),
            json!({"rfilename": "agent_a/latest-00000-of-00001.parquet"}),
            json!({"rfilename": "README.md"}),
        ];
        assert_eq!(board_configs(&siblings), ["agent", "agent_a", "agent_bash_reliability"]);
        assert_eq!(board_configs(&[]), ["agent"]);
    }
}
