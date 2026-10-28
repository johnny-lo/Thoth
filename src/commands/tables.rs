//! The three external tables `recommend` sends, filtered by fact only (RJ-2):
//! the organizations the registered accounts belong to, and the columns the
//! commander can use. Thoth neither maps a row to an account nor ranks; the
//! full tables stay in the caches and at their sources.

use crate::commands::{arena, models};
use crate::config::Paths;
use crate::db::global::{Profile, StoredModels};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const AA_LICENSE_TEXT: &str = "Artificial Analysis data: cite artificialanalysis.ai as the source; it stays on this computer and must not be forwarded.";
const ARENA_FILTERED: &str = "Filtered by Thoth from the Agent Arena main board: only the organizations with an account registered here, sorted by the source's rank, and only the columns listed; see the source for the full table. `signals` are the agent_* subset boards, matched by identical model_name; their scores are not comparable with each other or with the main board. CC BY 4.0 asks that this modification be noted.";
const AA_FILTERED: &str = "Filtered by Thoth from the Artificial Analysis free endpoint: only the model creators with an account registered here, and only the three indexes; see the source for the full table.";
const ASL_FILTERED: &str = "Filtered by Thoth from AI Stupid Level: only the providers with an account registered here, and only whether the model may have degraded lately (status not good, or trend down); see the source for the scores.";

/// The organizations of the registered accounts (any status), normalized:
/// each profile's provider, plus what the prefix of each model name in an
/// antigravity profile's stored list (what its last login check read,
/// MODELS-1; none stored adds nothing) says: `claude-` anthropic, `gpt-`
/// openai, `gemini-` google.
pub(crate) fn organizations(profiles: &[Profile], stored: &BTreeMap<String, StoredModels>) -> BTreeSet<String> {
    let mut orgs = BTreeSet::new();
    for p in profiles {
        orgs.insert(models::normalized(&p.provider));
        if p.harness == "antigravity" {
            let models = stored.get(&p.profile_id).map(|s| s.models.as_slice()).unwrap_or_default();
            for entry in models {
                if let Some(org) = agy_organization(&entry.model) {
                    orgs.insert(models::normalized(org));
                }
            }
        }
    }
    orgs.remove("");
    orgs
}

/// The organization an antigravity model name's prefix names, or none.
pub(crate) fn agy_organization(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude-") {
        Some("anthropic")
    } else if lower.starts_with("gpt-") {
        Some("openai")
    } else if lower.starts_with("gemini-") {
        Some("google")
    } else {
        None
    }
}

fn table_frame(
    source_name: &str,
    source_url: &str,
    license_text: Option<&str>,
    license_url: &str,
    filtered: &str,
    cache: &models::CacheFile,
    rows: Vec<Value>,
) -> Map<String, Value> {
    let (age_hours, stale) = models::staleness(cache.fetched_at.as_deref().filter(|_| cache.present));
    let mut table = Map::new();
    table.insert("source".into(), json!({"name": source_name, "url": source_url}));
    table.insert("license".into(), json!({"text": license_text, "url": license_url}));
    table.insert("filtered".into(), json!(filtered));
    table.insert("present".into(), json!(cache.present));
    table.insert("fetched_at".into(), json!(cache.fetched_at));
    table.insert("age_hours".into(), json!(age_hours));
    table.insert("stale".into(), json!(stale));
    table.insert("rows".into(), Value::Array(rows));
    table
}

fn number_or_null(v: Option<&Value>) -> Value {
    v.filter(|v| v.is_number()).cloned().unwrap_or(Value::Null)
}

fn as_rank(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(f64::MAX)
}

/// Agent Arena: the main board's rows of the given organizations, by rank,
/// with each subset board's score and rank for the identical model name.
pub(crate) fn arena_rows(cache: &Value, orgs: &BTreeSet<String>) -> Vec<Value> {
    let Some(boards) = cache.get("boards").and_then(Value::as_object) else {
        return Vec::new();
    };
    let board_rows = |name: &str| -> &[Value] {
        boards
            .get(name)
            .and_then(|b| b.get("rows"))
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };
    let subsets: Vec<&String> = boards.keys().filter(|k| k.as_str() != arena::MAIN_BOARD).collect();
    let mut rows: Vec<&Value> = board_rows(arena::MAIN_BOARD)
        .iter()
        .filter(|row| {
            row.get("organization")
                .and_then(Value::as_str)
                .is_some_and(|org| orgs.contains(&models::normalized(org)))
        })
        .collect();
    rows.sort_by(|a, b| {
        as_rank(a.get("rank"))
            .partial_cmp(&as_rank(b.get("rank")))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.into_iter()
        .map(|row| {
            let name = row.get("model_name").and_then(Value::as_str).unwrap_or("");
            let (_, effort) = models::aa_name_parts(name);
            let mut signals = Map::new();
            for subset in &subsets {
                if let Some(hit) = board_rows(subset)
                    .iter()
                    .find(|r| r.get("model_name").and_then(Value::as_str) == Some(name))
                {
                    signals.insert(
                        (*subset).clone(),
                        json!({"score": number_or_null(hit.get("score")), "rank": number_or_null(hit.get("rank"))}),
                    );
                }
            }
            json!({
                "model_name": name,
                "organization": row.get("organization").cloned().unwrap_or(Value::Null),
                "effort": effort,
                "score": number_or_null(row.get("score")),
                "score_ci_lower": number_or_null(row.get("score_ci_lower")),
                "score_ci_upper": number_or_null(row.get("score_ci_upper")),
                "rank": number_or_null(row.get("rank")),
                "observation_count": number_or_null(row.get("observation_count")),
                "signals": signals,
            })
        })
        .collect()
}

fn creator_matches(value: Option<&str>, orgs: &BTreeSet<String>) -> bool {
    let Some(value) = value else { return false };
    let normalized = models::normalized(value);
    orgs.iter()
        .any(|org| normalized == *org || normalized.starts_with(&format!("{org}-")))
}

/// Artificial Analysis: the rows whose creator slug or name is one of the
/// organizations (or starts with it and a dash), with the three indexes.
pub(crate) fn aa_rows(cache: &Value, orgs: &BTreeSet<String>) -> Vec<Value> {
    cache
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter(|row| {
                    creator_matches(row.pointer("/model_creator/slug").and_then(Value::as_str), orgs)
                        || creator_matches(row.pointer("/model_creator/name").and_then(Value::as_str), orgs)
                })
                .map(|row| {
                    let name = row.get("name").and_then(Value::as_str).unwrap_or("");
                    let (_, effort) = models::aa_name_parts(name);
                    json!({
                        "name": name,
                        "effort": effort,
                        "intelligence_index": number_or_null(row.pointer("/evaluations/artificial_analysis_intelligence_index")),
                        "coding_index": number_or_null(row.pointer("/evaluations/artificial_analysis_coding_index")),
                        "agentic_index": number_or_null(row.pointer("/evaluations/artificial_analysis_agentic_index")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// AI Stupid Level: only whether a model of the given providers may have
/// degraded lately, as `may_have_degraded` judges it in `thoth models`.
pub(crate) fn asl_rows(cache: &Value, orgs: &BTreeSet<String>) -> Vec<Value> {
    cache
        .pointer("/models/data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter(|row| {
                    row.get("provider")
                        .and_then(Value::as_str)
                        .is_some_and(|p| orgs.contains(&models::normalized(p)))
                })
                .map(|row| {
                    let status = row.get("status").and_then(Value::as_str).unwrap_or("");
                    let trend = row.get("trend").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "name": row.get("name").cloned().unwrap_or(Value::Null),
                        "degraded": status != "good" || trend == "down",
                        "observed_at": row.get("lastUpdated").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `tables` object of `recommend --json`: exactly `artificial_analysis`,
/// `agent_arena` and `ai_stupid_level`, read from the caches once. `stored`
/// is the profiles' stored model lists (`organizations`).
pub(crate) fn build(paths: &Paths, profiles: &[Profile], stored: &BTreeMap<String, StoredModels>) -> Value {
    let orgs = organizations(profiles, stored);
    let empty = Value::Null;

    let arena_cache = arena::read_cache(paths);
    let arena_value = arena_cache.value.as_ref().unwrap_or(&empty);
    let mut agent_arena = table_frame(
        arena::NAME,
        arena::SOURCE_URL,
        Some(arena::LICENSE),
        arena::LICENSE_URL,
        ARENA_FILTERED,
        &arena_cache,
        arena_rows(arena_value, &orgs),
    );
    agent_arena.insert("sha".into(), arena_value.get("sha").cloned().unwrap_or(Value::Null));
    agent_arena.insert(
        "publish_date".into(),
        arena_value
            .pointer(&format!("/boards/{}/publish_date", arena::MAIN_BOARD))
            .cloned()
            .unwrap_or(Value::Null),
    );

    let aa_cache = models::aa_cache(paths);
    let aa_value = aa_cache.value.as_ref().unwrap_or(&empty);
    let artificial_analysis = table_frame(
        "Artificial Analysis",
        models::AA_SOURCE_URL,
        Some(AA_LICENSE_TEXT),
        models::AA_SOURCE_URL,
        AA_FILTERED,
        &aa_cache,
        aa_rows(aa_value, &orgs),
    );

    let asl_cache = models::asl_cache(paths);
    let asl_value = asl_cache.value.as_ref().unwrap_or(&empty);
    let asl_license = asl_value
        .pointer("/source/license")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ai_stupid_level = table_frame(
        "AI Stupid Level",
        models::SOURCE_URL,
        asl_license.as_deref(),
        models::SOURCE_URL,
        ASL_FILTERED,
        &asl_cache,
        asl_rows(asl_value, &orgs),
    );

    let mut tables: BTreeMap<&str, Value> = BTreeMap::new();
    tables.insert("artificial_analysis", Value::Object(artificial_analysis));
    tables.insert("agent_arena", Value::Object(agent_arena));
    tables.insert("ai_stupid_level", Value::Object(ai_stupid_level));
    json!(tables)
}

/// One line per table for the human output: row count and fetch time.
pub(crate) fn human_lines(tables: &Value) -> Vec<String> {
    ["agent_arena", "artificial_analysis", "ai_stupid_level"]
        .iter()
        .filter_map(|key| {
            let table = tables.get(key)?;
            let name = table.pointer("/source/name").and_then(Value::as_str).unwrap_or(key);
            Some(if table.get("present").and_then(Value::as_bool) == Some(true) {
                format!(
                    "{name}: {} row(s) (filtered), fetched {}",
                    table.get("rows").and_then(Value::as_array).map_or(0, Vec::len),
                    table.get("fetched_at").and_then(Value::as_str).unwrap_or("unknown")
                )
            } else {
                format!("{name}: not cached")
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, provider: &str, harness: &str) -> Profile {
        Profile {
            profile_id: id.into(),
            provider: provider.into(),
            harness: harness.into(),
            account_label: None,
            config_dir: String::new(),
            default_models: None,
            status: "active".into(),
        }
    }

    fn stored(profile_id: &str, models: &[&str]) -> BTreeMap<String, StoredModels> {
        let entries = models
            .iter()
            .map(|m| crate::adapter::ModelEntry { model: (*m).to_string(), display_name: (*m).to_string() })
            .collect();
        BTreeMap::from([(profile_id.to_string(), StoredModels { models: entries, seen_at: None })])
    }

    #[test]
    fn organizations_come_from_providers_and_agy_model_prefixes() {
        let none = BTreeMap::new();
        let only_claude = organizations(&[profile("claude-a", "Anthropic", "claude-code")], &none);
        assert_eq!(only_claude.into_iter().collect::<Vec<_>>(), ["anthropic"]);
        // an agy account without a stored list is its provider only
        let agy_unlisted = organizations(&[profile("agy-a", "google", "antigravity")], &none);
        assert_eq!(agy_unlisted.into_iter().collect::<Vec<_>>(), ["google"]);
        let only_agy = organizations(
            &[profile("agy-a", "google", "antigravity")],
            &stored("agy-a", &["gemini-3.8-flash-high", "claude-sonnet-4-6", "gpt-oss-120b-medium", "mystery-1"]),
        );
        assert_eq!(
            only_agy.into_iter().collect::<Vec<_>>(),
            ["anthropic", "google", "openai"],
            "the stored agy list names claude-, gpt- and gemini- models"
        );
        // a claude-code account's stored list (there is none) adds nothing
        let claude_listed = organizations(
            &[profile("claude-a", "anthropic", "claude-code")],
            &stored("claude-a", &["gpt-oss-120b-medium"]),
        );
        assert_eq!(claude_listed.into_iter().collect::<Vec<_>>(), ["anthropic"]);
        assert_eq!(agy_organization("claude-sonnet-4-6"), Some("anthropic"));
        assert_eq!(agy_organization("gpt-oss-120b-medium"), Some("openai"));
        assert_eq!(agy_organization("gemini-3.8-flash-high"), Some("google"));
        assert_eq!(agy_organization("mystery-1"), None);
    }

    #[test]
    fn arena_rows_keep_only_the_organizations_sorted_by_rank_with_signals() {
        let cache = json!({"boards": {
            "agent": {"publish_date": "2026-10-16", "rows": [
                {"model_name": "Mistral Big", "organization": "mistral", "score": 1300.0, "score_ci_lower": 1290.0, "score_ci_upper": 1310.0, "observation_count": 100.0, "rank": 1},
                {"model_name": "GPT 6.1 Sol (Max)", "organization": "openai", "score": 1290.0, "score_ci_lower": 1280.0, "score_ci_upper": 1300.0, "observation_count": 200.0, "rank": 3},
                {"model_name": "Claude Fable 5.1", "organization": "Anthropic", "score": 1295.0, "score_ci_lower": 1285.0, "score_ci_upper": 1305.0, "observation_count": 150.0, "rank": 2},
            ]},
            "agent_bash": {"publish_date": "2026-10-16", "rows": [
                {"model_name": "GPT 6.1 Sol (Max)", "organization": "openai", "score": 1100.0, "score_ci_lower": null, "score_ci_upper": null, "observation_count": 10.0, "rank": 1},
                {"model_name": "Only Here", "organization": "openai", "score": 1000.0, "score_ci_lower": null, "score_ci_upper": null, "observation_count": 10.0, "rank": 2},
            ]},
        }});
        let orgs: BTreeSet<String> = ["anthropic", "openai"].iter().map(|s| s.to_string()).collect();
        let rows = arena_rows(&cache, &orgs);
        let names: Vec<&str> = rows.iter().map(|r| r["model_name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Claude Fable 5.1", "GPT 6.1 Sol (Max)"]);
        assert_eq!(rows[1]["effort"], "max");
        assert!(rows[0]["effort"].is_null());
        assert_eq!(rows[1]["signals"]["agent_bash"], json!({"score": 1100.0, "rank": 1}));
        assert!(rows[0]["signals"].as_object().unwrap().is_empty());
        let mut keys: Vec<&str> = rows[0].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["effort", "model_name", "observation_count", "organization", "rank", "score", "score_ci_lower", "score_ci_upper", "signals"]
        );
    }

    #[test]
    fn aa_rows_match_creator_slug_or_name_and_prefix() {
        let cache = json!({"data": [
            {"name": "Claude Fable 5.1 (max)", "model_creator": {"slug": "anthropic", "name": "Anthropic"}, "evaluations": {"artificial_analysis_intelligence_index": 70, "artificial_analysis_coding_index": 60, "artificial_analysis_agentic_index": null}, "pricing": {"price_1m_input_tokens": 3}},
            {"name": "GPT 6.1 Sol", "model_creator": {"slug": "openai-labs", "name": "OpenAI Labs"}, "evaluations": {}},
            {"name": "Llama 9", "model_creator": {"slug": "meta", "name": "Meta"}, "evaluations": {}},
            {"name": "Mistral Big", "model_creator": {"slug": "mistral", "name": "Mistral AI"}, "evaluations": {}},
        ]});
        let orgs: BTreeSet<String> = ["anthropic", "openai"].iter().map(|s| s.to_string()).collect();
        let rows = aa_rows(&cache, &orgs);
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Claude Fable 5.1 (max)", "GPT 6.1 Sol"]);
        assert_eq!(rows[0]["effort"], "max");
        assert_eq!(rows[0]["intelligence_index"], 70);
        assert!(rows[0].get("price_input_per_1m").is_none());
        let mut keys: Vec<&str> = rows[0].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["agentic_index", "coding_index", "effort", "intelligence_index", "name"]);
    }

    #[test]
    fn asl_rows_say_only_whether_a_model_degraded() {
        let cache = json!({"models": {"data": [
            {"name": "claude-sonnet-5", "provider": "anthropic", "currentScore": 88, "status": "good", "trend": "up", "lastUpdated": "2026-10-16T00:00:00Z"},
            {"name": "gpt-6", "provider": "OpenAI", "currentScore": 80, "status": "good", "trend": "down", "lastUpdated": "2026-10-16T01:00:00Z"},
            {"name": "mix", "provider": "mistral", "currentScore": 10, "status": "warning", "trend": "down", "lastUpdated": "x"},
        ]}});
        let orgs: BTreeSet<String> = ["anthropic", "openai"].iter().map(|s| s.to_string()).collect();
        let rows = asl_rows(&cache, &orgs);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], json!({"name": "claude-sonnet-5", "degraded": false, "observed_at": "2026-10-16T00:00:00Z"}));
        assert_eq!(rows[1]["degraded"], true);
    }
}
