use serde::{Deserialize, Serialize};
use std::path::Path;

/// The effort levels, from least to most reasoning.
pub const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

pub fn parse_level(text: &str) -> Result<String, String> {
    let level = text.trim().to_ascii_lowercase();
    if EFFORT_LEVELS.contains(&level.as_str()) {
        Ok(level)
    } else {
        Err(format!(
            "invalid effort level '{text}'; expected one of: {}",
            EFFORT_LEVELS.join(", ")
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupportedLevels {
    Levels(Vec<String>),
    NoEffort,
    Unknown,
    NotApplicable,
}

pub fn supported_levels(harness: &str, model: &str, config_dir: &Path) -> SupportedLevels {
    match harness {
        "claude-code" => claude_levels(model),
        "codex" => codex_levels(model, config_dir),
        "antigravity" => antigravity_levels(model),
        _ => SupportedLevels::NotApplicable,
    }
}

/// AGY-1: the levels agy's own `--effort` takes (`agy --help`: `low|medium|
/// high|xhigh|max`; `ultra` is not among them, so the downgrade rule lowers
/// it to `max`).
const AGY_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// AGY-1: the level an agy model name carries itself. `agy models` lists
/// effort variants as names ending in `-low`, `-medium`, `-high`, `-xhigh` or
/// `-max` (`gemini-3.8-flash-high`); such a name is already one effort, and
/// giving agy `--effort` as well fails with `--model X conflicts with
/// --effort=Y`. The suffix is read after trimming, without regard to letter
/// case; the level is returned in lower case. `None` for every other name
/// (`claude-sonnet-4-6`) and for every other harness.
pub fn carried_level(harness: &str, model: &str) -> Option<String> {
    if harness != "antigravity" {
        return None;
    }
    let model = model.trim().to_ascii_lowercase();
    AGY_EFFORT_LEVELS
        .iter()
        .find(|level| model.ends_with(&format!("-{level}")))
        .map(|level| (*level).to_string())
}

/// AGY-1: a name that carries its level supports exactly that one; any other
/// name takes the five levels agy's flag accepts.
fn antigravity_levels(model: &str) -> SupportedLevels {
    match carried_level("antigravity", model) {
        Some(level) => SupportedLevels::Levels(vec![level]),
        None => levels(AGY_EFFORT_LEVELS),
    }
}

fn claude_levels(model: &str) -> SupportedLevels {
    let model = model.trim().to_ascii_lowercase();
    if model == "haiku" || model.starts_with("claude-haiku") {
        SupportedLevels::NoEffort
    } else if matches!(model.as_str(), "sonnet" | "opus" | "fable")
        || [
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
        ]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        levels(&["low", "medium", "high", "xhigh", "max"])
    } else if model.starts_with("claude-opus-4-6") || model.starts_with("claude-sonnet-4-6") {
        levels(&["low", "medium", "high", "max"])
    } else {
        SupportedLevels::Unknown
    }
}

fn levels(names: &[&str]) -> SupportedLevels {
    SupportedLevels::Levels(names.iter().map(|name| (*name).to_string()).collect())
}

fn codex_levels(model: &str, config_dir: &Path) -> SupportedLevels {
    let path = config_dir.join("models_cache.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return SupportedLevels::Unknown;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return SupportedLevels::Unknown;
    };
    let Some(models) = root.get("models").and_then(serde_json::Value::as_array) else {
        return SupportedLevels::Unknown;
    };
    let Some(entry) = models
        .iter()
        .find(|entry| entry.get("slug").and_then(serde_json::Value::as_str) == Some(model))
    else {
        return SupportedLevels::Unknown;
    };
    let Some(raw_levels) = entry
        .get("supported_reasoning_levels")
        .and_then(serde_json::Value::as_array)
    else {
        return SupportedLevels::Unknown;
    };
    let mut found = [false; 6];
    for raw in raw_levels {
        let Some(effort) = raw.get("effort").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(index) = EFFORT_LEVELS.iter().position(|level| *level == effort) {
            found[index] = true;
        }
    }
    let supported: Vec<String> = EFFORT_LEVELS
        .iter()
        .enumerate()
        .filter(|(index, _)| found[*index])
        .map(|(_, level)| (*level).to_string())
        .collect();
    if supported.is_empty() {
        SupportedLevels::Unknown
    } else {
        SupportedLevels::Levels(supported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortResolution {
    pub requested: Option<String>,
    pub sent: Option<String>,
    pub note: Option<String>,
}

pub fn resolve(
    harness: &str,
    model: &str,
    config_dir: &Path,
    requested: Option<&str>,
) -> EffortResolution {
    let requested = requested.map(str::to_string);
    let Some(requested_level) = requested.clone() else {
        // AGY-1: nothing asked for, but an agy name that carries its level
        // is that level; it is recorded as sent (the launch adds no flag,
        // see `antigravity::build_command`) with a note saying where it
        // came from.
        if let Some(level) = carried_level(harness, model) {
            return EffortResolution {
                requested,
                note: Some(format!("effort {level} is carried by the model variant {model}")),
                sent: Some(level),
            };
        }
        return EffortResolution { requested, sent: None, note: None };
    };
    match supported_levels(harness, model, config_dir) {
        SupportedLevels::Levels(supported) => {
            if supported.iter().any(|level| level == &requested_level) {
                EffortResolution { requested, sent: Some(requested_level.clone()), note: None }
            } else {
                let requested_index = EFFORT_LEVELS.iter().position(|level| *level == requested_level);
                let sent = requested_index
                    .and_then(|index| {
                        supported
                            .iter()
                            .rev()
                            .find(|level| EFFORT_LEVELS.iter().position(|candidate| *candidate == level.as_str()).is_some_and(|i| i < index))
                    })
                    .cloned()
                    .unwrap_or_else(|| supported[0].clone());
                EffortResolution {
                    requested,
                    note: Some(format!("{requested_level} is not supported by {model}; using {sent}")),
                    sent: Some(sent),
                }
            }
        }
        SupportedLevels::NoEffort => EffortResolution {
            requested,
            sent: None,
            note: Some(format!("{model} does not support effort; the harness default applies")),
        },
        SupportedLevels::Unknown => EffortResolution {
            requested: requested.clone(),
            sent: requested,
            note: Some(format!("could not confirm {model} supports {requested_level}; passed as given")),
        },
        SupportedLevels::NotApplicable => EffortResolution {
            requested,
            sent: None,
            note: Some(format!("harness {harness} has no effort setting")),
        },
    }
}

pub fn launch_args(harness: &str, sent: Option<&str>) -> Vec<String> {
    let Some(sent) = sent else { return Vec::new() };
    match harness {
        "claude-code" => vec!["--effort".to_string(), sent.to_string()],
        "codex" => vec!["-c".to_string(), format!("model_reasoning_effort=\"{sent}\"")],
        // AGY-1: agy's own flag; `antigravity::build_command` leaves it out
        // when the model name already carries the level.
        "antigravity" => vec!["--effort".to_string(), sent.to_string()],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LaunchSettings {
    pub effort_requested: Option<String>,
    pub effort: Option<String>,
    pub effort_note: Option<String>,
}

pub fn with_launch(spec_json: &str, settings: &LaunchSettings) -> anyhow::Result<String> {
    let mut value: serde_json::Value = serde_json::from_str(spec_json)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("spec must be a JSON object"))?;
    object.insert("launch".to_string(), serde_json::to_value(settings)?);
    Ok(serde_json::to_string(&value)?)
}

pub fn parse_launch(spec_json: &str) -> Option<LaunchSettings> {
    let value: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    let mut settings: LaunchSettings = serde_json::from_value(value.get("launch")?.clone()).ok()?;
    for field in [&mut settings.effort, &mut settings.effort_requested] {
        *field = field.as_deref().and_then(|level| parse_level(level).ok());
    }
    Some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn codex_cache(dir: &Path, slug: &str, efforts: &[&str]) {
        let supported: Vec<_> = efforts.iter().map(|effort| serde_json::json!({"effort": effort})).collect();
        fs::write(dir.join("models_cache.json"), serde_json::json!({"models": [{"slug": slug, "supported_reasoning_levels": supported}]}).to_string()).unwrap();
    }

    #[test]
    fn parse_level_accepts_trimmed_mixed_case_and_rejects_bad_values() {
        assert_eq!(parse_level("  High ").unwrap(), "high");
        assert!(parse_level("extreme").is_err());
        assert!(parse_level("").is_err());
        assert!(parse_level("extreme").unwrap_err().contains("low, medium, high, xhigh, max, ultra"));
    }

    #[test]
    fn supported_levels_covers_claude_rules() {
        assert_eq!(supported_levels("claude-code", " haiku ", Path::new(".")), SupportedLevels::NoEffort);
        assert_eq!(supported_levels("claude-code", "claude-haiku-4", Path::new(".")), SupportedLevels::NoEffort);
        assert_eq!(supported_levels("claude-code", "sonnet", Path::new(".")), levels(&["low", "medium", "high", "xhigh", "max"]));
        assert_eq!(supported_levels("claude-code", "claude-opus-4-6-20260101", Path::new(".")), levels(&["low", "medium", "high", "max"]));
        assert_eq!(supported_levels("claude-code", "claude-unknown", Path::new(".")), SupportedLevels::Unknown);
    }

    #[test]
    fn supported_levels_claude_high_effort_prefixes_each_have_all_levels() {
        for model in [
            "claude-sonnet-5-20260101",
            "claude-opus-5-20260101",
            "claude-fable-5-20260101",
            "claude-opus-4-8-20260101",
            "claude-opus-4-7-20260101",
        ] {
            assert_eq!(supported_levels("claude-code", model, Path::new(".")), levels(&["low", "medium", "high", "xhigh", "max"]));
        }
    }

    #[test]
    fn supported_levels_claude_sonnet_46_has_no_xhigh_or_ultra() {
        assert_eq!(supported_levels("claude-code", "claude-sonnet-4-6-20260101", Path::new(".")), levels(&["low", "medium", "high", "max"]));
    }

    #[test]
    fn supported_levels_claude_plain_opus_and_fable_have_all_levels() {
        for model in ["opus", "fable"] {
            assert_eq!(supported_levels("claude-code", model, Path::new(".")), levels(&["low", "medium", "high", "xhigh", "max"]));
        }
    }

    #[test]
    fn supported_levels_covers_codex_cache_and_other_harness() {
        let dir = tempdir().unwrap();
        codex_cache(dir.path(), "gpt", &["ultra", "medium", "bogus", "low"]);
        assert_eq!(supported_levels("codex", "gpt", dir.path()), levels(&["low", "medium", "ultra"]));
        assert_eq!(supported_levels("codex", "missing", dir.path()), SupportedLevels::Unknown);
        fs::write(dir.path().join("models_cache.json"), "not json").unwrap();
        assert_eq!(supported_levels("codex", "gpt", dir.path()), SupportedLevels::Unknown);
        assert_eq!(supported_levels("other", "gpt", dir.path()), SupportedLevels::NotApplicable);
    }

    #[test]
    fn supported_levels_codex_missing_cache_is_unknown() {
        let dir = tempdir().unwrap();
        assert_eq!(supported_levels("codex", "gpt", dir.path()), SupportedLevels::Unknown);
    }

    #[test]
    fn supported_levels_codex_empty_effort_list_is_unknown() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("models_cache.json"), serde_json::json!({
            "models": [{"slug": "gpt", "supported_reasoning_levels": []}]
        }).to_string()).unwrap();
        assert_eq!(supported_levels("codex", "gpt", dir.path()), SupportedLevels::Unknown);
    }

    #[test]
    fn resolve_applies_downgrade_and_unknown_rules() {
        let dir = tempdir().unwrap();
        assert_eq!(resolve("claude-code", "sonnet", dir.path(), Some("ultra")).sent, Some("max".into()));
        assert_eq!(resolve("claude-code", "claude-opus-4-6", dir.path(), Some("xhigh")).sent, Some("high".into()));
        codex_cache(dir.path(), "gpt", &["low", "medium", "high", "xhigh"]);
        assert_eq!(resolve("codex", "gpt", dir.path(), Some("max")).sent, Some("xhigh".into()));
        codex_cache(dir.path(), "gpt", &["medium", "high"]);
        assert_eq!(resolve("codex", "gpt", dir.path(), Some("low")).sent, Some("medium".into()));
        let haiku = resolve("claude-code", "haiku", dir.path(), Some("high"));
        assert_eq!(haiku.sent, None);
        assert!(haiku.note.is_some());
        let unknown = resolve("claude-code", "new-model", dir.path(), Some("high"));
        assert_eq!(unknown.sent, Some("high".into()));
        assert!(unknown.note.is_some());
    }

    #[test]
    fn launch_args_are_complete() {
        assert_eq!(launch_args("claude-code", Some("high")), vec!["--effort", "high"]);
        assert_eq!(launch_args("codex", Some("high")), vec!["-c", "model_reasoning_effort=\"high\""]);
        assert_eq!(launch_args("codex", None), Vec::<String>::new());
        assert_eq!(launch_args("antigravity", Some("low")), vec!["--effort", "low"]);
        assert_eq!(launch_args("antigravity", None), Vec::<String>::new());
        assert_eq!(launch_args("other", Some("high")), Vec::<String>::new());
    }

    /// AGY-1 (AC4): a name ending in one of agy's five levels carries it.
    #[test]
    fn carried_level_reads_only_an_agy_level_suffix() {
        assert_eq!(carried_level("antigravity", "gemini-3.8-flash-high").as_deref(), Some("high"));
        assert_eq!(carried_level("antigravity", "  Gemini-3.8-Flash-XHIGH "), Some("xhigh".to_string()));
        assert_eq!(carried_level("antigravity", "gpt-oss-120b-medium").as_deref(), Some("medium"));
        assert_eq!(carried_level("antigravity", "gemini-3.1-pro-low").as_deref(), Some("low"));
        assert_eq!(carried_level("antigravity", "x-max").as_deref(), Some("max"));
        for model in ["claude-sonnet-4-6", "claude-opus-4-6-thinking", "flash-ultra", "high", "low-x", ""] {
            assert_eq!(carried_level("antigravity", model), None, "{model}");
        }
        // only agy names carry a level; a Claude or codex name never does
        assert_eq!(carried_level("claude-code", "claude-sonnet-4-6-high"), None);
        assert_eq!(carried_level("codex", "gpt-5.6-luna-low"), None);
    }

    /// AGY-1 (AC4): a carried level is the only supported one; a bare name
    /// takes agy's five.
    #[test]
    fn supported_levels_covers_antigravity_rules() {
        assert_eq!(supported_levels("antigravity", "gemini-3.8-flash-high", Path::new(".")), levels(&["high"]));
        assert_eq!(supported_levels("antigravity", " GEMINI-3.8-FLASH-low ", Path::new(".")), levels(&["low"]));
        assert_eq!(
            supported_levels("antigravity", "claude-sonnet-4-6", Path::new(".")),
            levels(&["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(supported_levels("antigravity", "", Path::new(".")), levels(&["low", "medium", "high", "xhigh", "max"]));
    }

    /// AGY-1 (AC4): the existing downgrade rule against `Levels([one])`, the
    /// carried level when nothing was asked for, and `ultra` lowered to `max`.
    #[test]
    fn resolve_applies_antigravity_rules() {
        let dir = tempdir().unwrap();
        let exact = resolve("antigravity", "gemini-3.8-flash-high", dir.path(), Some("high"));
        assert_eq!(exact, EffortResolution { requested: Some("high".into()), sent: Some("high".into()), note: None });
        let above = resolve("antigravity", "gemini-3.8-flash-high", dir.path(), Some("max"));
        assert_eq!(above.sent.as_deref(), Some("high"));
        assert_eq!(above.note.as_deref(), Some("max is not supported by gemini-3.8-flash-high; using high"));
        let below = resolve("antigravity", "gemini-3.8-flash-high", dir.path(), Some("low"));
        assert_eq!(below.sent.as_deref(), Some("high"));
        assert_eq!(below.note.as_deref(), Some("low is not supported by gemini-3.8-flash-high; using high"));
        let carried = resolve("antigravity", "gemini-3.8-flash-high", dir.path(), None);
        assert_eq!(carried, EffortResolution {
            requested: None,
            sent: Some("high".into()),
            note: Some("effort high is carried by the model variant gemini-3.8-flash-high".into()),
        });
        let bare = resolve("antigravity", "claude-sonnet-4-6", dir.path(), Some("medium"));
        assert_eq!(bare, EffortResolution { requested: Some("medium".into()), sent: Some("medium".into()), note: None });
        let ultra = resolve("antigravity", "claude-sonnet-4-6", dir.path(), Some("ultra"));
        assert_eq!(ultra.sent.as_deref(), Some("max"));
        assert!(ultra.note.as_deref().unwrap().contains("ultra is not supported by claude-sonnet-4-6"), "{ultra:?}");
        // nothing asked for and nothing carried: nothing sent, no note (the
        // launch commands refuse this case before it gets here)
        assert_eq!(resolve("antigravity", "claude-sonnet-4-6", dir.path(), None),
            EffortResolution { requested: None, sent: None, note: None });
        // the other harnesses are untouched by the carried-level branch
        assert_eq!(resolve("claude-code", "claude-sonnet-4-6-high", dir.path(), None),
            EffortResolution { requested: None, sent: None, note: None });
    }

    #[test]
    fn launch_round_trip_preserves_other_spec_keys() {
        let settings = LaunchSettings { effort_requested: Some("high".into()), effort: Some("high".into()), effort_note: Some("note".into()) };
        let out = with_launch(r#"{"goal":"g","acceptance_criteria":["AC1"],"rework":{"attempt":2},"custom_key":"x"}"#, &settings).unwrap();
        assert_eq!(parse_launch(&out), Some(settings));
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["goal"], "g");
        assert_eq!(value["acceptance_criteria"], serde_json::json!(["AC1"]));
        assert_eq!(value["rework"]["attempt"], 2);
        assert_eq!(value["custom_key"], "x");
        assert!(crate::spec::parse_spec(&out).is_ok());
    }

    #[test]
    fn launch_rejects_non_objects_and_bad_launch_values() {
        let settings = LaunchSettings::default();
        assert!(with_launch("[1,2,3]", &settings).is_err());
        assert!(with_launch("not json", &settings).is_err());
        assert_eq!(parse_launch(r#"{"launch":7}"#), None);
        assert_eq!(parse_launch("not json"), None);
    }

    #[test]
    fn parse_launch_clears_invalid_levels_but_keeps_the_rest() {
        let both = parse_launch(r#"{"launch":{"effort":"extreme","effort_requested":"EXTREME"}}"#).unwrap();
        assert_eq!(both.effort, None);
        assert_eq!(both.effort_requested, None);
        let mixed = parse_launch(r#"{"launch":{"effort":"High","effort_requested":""}}"#).unwrap();
        assert_eq!(mixed.effort, Some("high".to_string()));
        assert_eq!(mixed.effort_requested, None);
        // an older run's launch keys (S3-8's `reasons` and the three flags)
        // are still read; only the effort fields are kept
        let kept = parse_launch(
            r#"{"launch":{"effort":"high","profile_auto":true,"model_auto":false,"effort_auto":true,"reasons":["x"]}}"#,
        )
        .unwrap();
        assert_eq!(kept.effort, Some("high".to_string()));
        assert_eq!(kept.effort_requested, None);
    }
}
