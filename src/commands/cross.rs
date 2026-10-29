//! CROSS-1: `recommend`'s cross table. Each account gets the models it can
//! run now (what `thoth models` lists, minus the ones a run proved
//! unavailable); each model gets the rows of the three filtered tables whose
//! name is one of the model's own names, and Thoth's own usage of that
//! account and model. A name that matches nothing leaves the cell null and is
//! listed in `unmatched`: no prefix, similarity or alias table, no guessing
//! (the user, 2026-10-21), and nothing here picks (RJ-1).

use crate::commands::models::{self, Listed};
use crate::commands::own_usage::Row;
use crate::lang::{self, Lang};
use serde_json::{json, Map, Value};

/// The three tables in the order a model row and `missing` list them, with
/// the key of each table row's name.
pub(crate) const TABLES: [(&str, &str); 3] = [
    ("agent_arena", "model_name"),
    ("artificial_analysis", "name"),
    ("ai_stupid_level", "name"),
];

/// The usage row keys that the cross table already says (the account and the
/// model) or that belong to the account, not the row.
const USAGE_DROPPED: [&str; 5] = ["profile", "model", "plan", "plan_label", "shared"];

/// A name's normalized base and the effort word of its trailing parenthesis,
/// read the way the tables read their rows' `effort`.
fn parts(name: &str) -> (String, Option<String>) {
    let (base, effort) = models::aa_name_parts(name);
    (models::normalized(base), effort)
}

/// Whether one of an account's names for a model is a table row's name: the
/// same normalized base, and the same effort when the own name has one.
pub(crate) fn name_matches(own: &str, row: &str) -> bool {
    let (own_base, own_effort) = parts(own);
    let (row_base, row_effort) = parts(row);
    !own_base.is_empty() && own_base == row_base && (own_effort.is_none() || own_effort == row_effort)
}

/// A model's own names: for claude-code the `score_name` (none until the
/// alias has run once); for codex and antigravity the slug and the display
/// name (one when they are the same or there is no display name).
pub(crate) fn names(harness: &str, model: &str, display_name: &str, score_name: Option<&str>) -> Vec<String> {
    match harness {
        crate::adapter::HARNESS_CLAUDE_CODE => score_name.map(|n| vec![n.to_string()]).unwrap_or_default(),
        crate::adapter::HARNESS_CODEX | crate::adapter::antigravity::HARNESS_NAME => {
            let mut out = vec![model.to_string()];
            if !display_name.is_empty() && display_name != model {
                out.push(display_name.to_string());
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The rows of one table (`--tables`' rows, in their order) that one of
/// `names` matches, each once; Agent Arena's without `organization`. Null when
/// none does.
pub(crate) fn cell(names: &[String], table: &str, name_key: &str, rows: &[Value]) -> Value {
    let hits: Vec<Value> = rows
        .iter()
        .filter(|row| {
            row.get(name_key)
                .and_then(Value::as_str)
                .is_some_and(|row_name| names.iter().any(|own| name_matches(own, row_name)))
        })
        .map(|row| {
            let mut row = row.clone();
            if table == "agent_arena" {
                if let Some(object) = row.as_object_mut() {
                    object.remove("organization");
                }
            }
            row
        })
        .collect();
    if hits.is_empty() {
        Value::Null
    } else {
        Value::Array(hits)
    }
}

/// One usage row as a model row carries it.
fn usage_json(row: &Row) -> Value {
    let mut value = row.json();
    if let Some(object) = value.as_object_mut() {
        for key in USAGE_DROPPED {
            object.remove(key);
        }
    }
    value
}

/// One account's facts for the cross table.
pub(crate) struct Account<'a> {
    pub(crate) profile_id: &'a str,
    pub(crate) harness: &'a str,
    pub(crate) shared: bool,
    pub(crate) windows: Vec<Value>,
    pub(crate) models_source: &'static str,
    pub(crate) models_seen_at: Option<String>,
    pub(crate) models_note: Option<String>,
    pub(crate) listed: Vec<Listed>,
}

/// The keys CROSS-1 adds to a candidate, and the account's `unmatched`
/// entries. `tables` is the `--tables` object (always built: it is the pool
/// the cells are taken from); `usage` is every usage row of the role.
pub(crate) fn account(a: Account, tables: &Value, usage: &[Row]) -> (Map<String, Value>, Vec<Value>) {
    let empty = Vec::new();
    let table_rows = |key: &str| tables.pointer(&format!("/{key}/rows")).and_then(Value::as_array).unwrap_or(&empty);
    let present = |key: &str| tables.pointer(&format!("/{key}/present")).and_then(Value::as_bool) == Some(true);
    let mut models_out = Vec::new();
    let mut excluded = Vec::new();
    let mut unmatched = Vec::new();
    let mut in_list: Vec<&str> = Vec::new();
    for m in &a.listed {
        if let Some(u) = &m.unavailable {
            excluded.push(json!({"model": m.model, "seen_at": u.seen_at, "reason": u.reason, "run_id": u.run_id}));
            continue;
        }
        in_list.push(&m.model);
        let own = names(a.harness, &m.model, &m.display_name, m.score_name.as_deref());
        let mut row = Map::new();
        row.insert("model".into(), json!(m.model));
        row.insert("names".into(), json!(own));
        let mut missing = Vec::new();
        for (table, name_key) in TABLES {
            let value = cell(&own, table, name_key, table_rows(table));
            if value.is_null() && present(table) {
                missing.push(table);
            }
            row.insert(table.into(), value);
        }
        row.insert(
            "usage".into(),
            Value::Array(
                usage
                    .iter()
                    .filter(|r| r.profile == a.profile_id && r.model == m.model)
                    .map(usage_json)
                    .collect(),
            ),
        );
        if !missing.is_empty() {
            unmatched.push(json!({"profile_id": a.profile_id, "model": m.model, "names": own, "missing": missing}));
        }
        models_out.push(Value::Object(row));
    }
    // The account's usage rows of models not in `models` now, grouped by
    // model in the rows' order, so every row is sent exactly once.
    let mut other: Vec<(String, Vec<Value>)> = Vec::new();
    for r in usage.iter().filter(|r| r.profile == a.profile_id && !in_list.contains(&r.model.as_str())) {
        match other.iter_mut().find(|(model, _)| *model == r.model) {
            Some((_, rows)) => rows.push(usage_json(r)),
            None => other.push((r.model.clone(), vec![usage_json(r)])),
        }
    }
    let mut out = Map::new();
    out.insert("shared".into(), json!(a.shared));
    out.insert("windows".into(), Value::Array(a.windows));
    out.insert("models_source".into(), json!(a.models_source));
    out.insert("models_seen_at".into(), json!(a.models_seen_at));
    out.insert("models_note".into(), json!(a.models_note));
    out.insert("models".into(), Value::Array(models_out));
    out.insert("models_excluded".into(), Value::Array(excluded));
    out.insert(
        "usage_other_models".into(),
        Value::Array(other.into_iter().map(|(model, usage)| json!({"model": model, "usage": usage})).collect()),
    );
    (out, unmatched)
}

// --- the human output -----------------------------------------------------------------------

/// Display width: an ASCII character is one column, any other two.
fn width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// Rows of cells (the first is the header) as lines: two spaces of indent,
/// every column as wide as its widest cell, two spaces between columns, the
/// last column not padded.
pub(crate) fn layout(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| rows.iter().filter_map(|r| r.get(i)).map(|c| width(c)).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::from("  ");
            for (i, c) in row.iter().enumerate() {
                line.push_str(c);
                if i + 1 < row.len() {
                    line.push_str(&" ".repeat(widths[i] - width(c) + 2));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// A JSON number as its value reads: `70.0` is `70`; null or not a number is
/// `-`.
fn number(v: &Value) -> String {
    if let Some(i) = v.as_i64() {
        i.to_string()
    } else if let Some(x) = v.as_f64() {
        x.to_string()
    } else {
        "-".to_string()
    }
}

/// ` (EFFORT)` of a cell's first row and ` +N` for the other rows.
fn effort_and_more(first: &Value, rows: &[Value], with_effort: bool) -> String {
    let mut s = String::new();
    if with_effort {
        if let Some(e) = first.get("effort").and_then(Value::as_str) {
            s.push_str(&format!(" ({e})"));
        }
    }
    if rows.len() > 1 {
        s.push_str(&format!(" +{}", rows.len() - 1));
    }
    s
}

fn score_cell(table: &str, cell: &Value, no_data: &str) -> String {
    let Some(rows) = cell.as_array().filter(|r| !r.is_empty()) else {
        return no_data.to_string();
    };
    let first = &rows[0];
    match table {
        "artificial_analysis" => format!(
            "{}/{}/{}{}",
            number(&first["intelligence_index"]),
            number(&first["coding_index"]),
            number(&first["agentic_index"]),
            effort_and_more(first, rows, true)
        ),
        "agent_arena" => format!(
            "#{} {}{}",
            number(&first["rank"]),
            number(&first["score"]),
            effort_and_more(first, rows, true)
        ),
        _ => format!(
            "{}{}",
            if first["degraded"] == true { "degraded" } else { "ok" },
            effort_and_more(first, rows, false)
        ),
    }
}

/// The three usage columns from the row with the most runs (the first of a
/// tie).
fn usage_cells(usage: &[Value], no_data: &str) -> [String; 3] {
    let Some(row) = usage
        .iter()
        .reduce(|best, r| if r["runs"].as_i64() > best["runs"].as_i64() { r } else { best })
    else {
        return [no_data.to_string(), no_data.to_string(), no_data.to_string()];
    };
    let tokens = row["tokens_avg"]
        .as_f64()
        .map_or(no_data.to_string(), |t| format!("{}", t.round() as i64));
    let percent = |v: &Value| v.as_f64().map(|x| format!("{x:.2}%"));
    let quota = match (percent(&row["quota_points_avg"]), percent(&row["quota_points_weekly_avg"])) {
        (None, None) => no_data.to_string(),
        (five, seven) => format!(
            "{}/{}{}",
            five.unwrap_or_else(|| "-".into()),
            seven.unwrap_or_else(|| "-".into()),
            if row["estimate_reliable"] == false { " unreliable" } else { "" }
        ),
    };
    let mut left = match (row["runs_left"].as_i64(), row["runs_left_window"].as_str()) {
        (Some(n), Some(w)) => format!("{n} {w}"),
        (Some(n), None) => n.to_string(),
        _ => no_data.to_string(),
    };
    if usage.len() > 1 {
        left.push_str(&format!(" (+{} rows)", usage.len() - 1));
    }
    [tokens, quota, left]
}

/// One window as the first line lists it.
fn window_text(w: &Value) -> String {
    let name = w["name"].as_str().unwrap_or("");
    let label = match w["group"].as_str() {
        Some(g) => format!("{name} ({g})"),
        None => name.to_string(),
    };
    if w["reset_since_observed"] == true {
        return format!("{label} reset, counts as 0%");
    }
    let resets = w["resets_at"].as_str().map_or("resets unknown".to_string(), |t| format!("resets {t}"));
    format!("{label} {}% used, {resets}", number(&w["used_percent"]))
}

/// The name a candidate's block starts with: the plan bracket, with
/// `shared` added when the account is marked shared.
fn account_name(c: &Value) -> String {
    let id = c["profile_id"].as_str().unwrap_or("");
    let label = c["plan_label"].as_str();
    let bracket = match (label, c["shared"] == true) {
        (Some(l), true) => Some(format!("{l}, shared")),
        (None, true) => Some("shared".to_string()),
        (l, false) => l.map(str::to_string),
    };
    crate::plan_label::named(id, bracket.as_deref(), Lang::En)
}

/// One candidate's block of the human output, from its `--json` object and
/// the top-level `unmatched`.
pub(crate) fn human_block(c: &Value, unmatched: &[Value], lang: Lang) -> Vec<String> {
    let no_data = lang::text(lang, "recommend.no_data");
    let empty = Vec::new();
    let list = |key: &str| c[key].as_array().unwrap_or(&empty);
    let windows = list("windows");
    let windows_text = if windows.is_empty() {
        "no quota reading".to_string()
    } else {
        windows.iter().map(window_text).collect::<Vec<_>>().join("; ")
    };
    let eligible = c["eligible"] == true;
    let mut lines = vec![format!(
        "{}  {} / {}  {} {}  {}",
        account_name(c),
        c["provider"].as_str().unwrap_or(""),
        c["harness"].as_str().unwrap_or(""),
        if eligible { "eligible" } else { "ineligible" },
        c["quota_state"].as_str().unwrap_or(""),
        windows_text
    )];
    if !eligible {
        let reasons: Vec<&str> = list("reasons").iter().filter_map(Value::as_str).collect();
        lines.push(format!("  reasons: {}", reasons.join("; ")));
    }
    let models_rows = list("models");
    if models_rows.is_empty() {
        lines.push("  no models listed".to_string());
    } else {
        let mut rows = vec![[
            "model",
            "AA I/C/A",
            "Arena #rank score",
            "ASL",
            "tokens avg",
            "quota/run 5h/7d",
            "runs left",
        ]
        .map(str::to_string)
        .to_vec()];
        for m in models_rows {
            let [tokens, quota, left] = usage_cells(m["usage"].as_array().unwrap_or(&empty), no_data);
            rows.push(vec![
                m["model"].as_str().unwrap_or("").to_string(),
                score_cell("artificial_analysis", &m["artificial_analysis"], no_data),
                score_cell("agent_arena", &m["agent_arena"], no_data),
                score_cell("ai_stupid_level", &m["ai_stupid_level"], no_data),
                tokens,
                quota,
                left,
            ]);
        }
        lines.extend(layout(&rows));
    }
    for x in list("models_excluded") {
        lines.push(format!(
            "  unavailable: {} since {}: {}",
            x["model"].as_str().unwrap_or(""),
            x["seen_at"].as_str().unwrap_or("unknown"),
            x["reason"].as_str().unwrap_or("")
        ));
    }
    for u in unmatched.iter().filter(|u| u["profile_id"] == c["profile_id"]) {
        let own: Vec<&str> = u["names"].as_array().unwrap_or(&empty).iter().filter_map(Value::as_str).collect();
        let missing: Vec<&str> = u["missing"].as_array().unwrap_or(&empty).iter().filter_map(Value::as_str).collect();
        lines.push(format!(
            "  no match: {} ({}): {}",
            u["model"].as_str().unwrap_or(""),
            if own.is_empty() { "no name yet; run the alias once".to_string() } else { own.join(", ") },
            missing.join(", ")
        ));
    }
    for o in list("usage_other_models") {
        let runs: i64 = o["usage"].as_array().unwrap_or(&empty).iter().filter_map(|r| r["runs"].as_i64()).sum();
        lines.push(format!("  other: {} {runs} run(s) (not in the list now)", o["model"].as_str().unwrap_or("")));
    }
    if let Some(at) = c["models_seen_at"].as_str() {
        lines.push(format!("  models read at {at}"));
    }
    if let Some(note) = c["models_note"].as_str() {
        lines.push(format!("  note: {note}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_match_by_base_and_by_effort_only_when_the_own_name_has_one() {
        for (own, row) in [
            ("gpt-6.1-sol", "GPT 6.1 Sol (Max)"),
            ("gpt-6.1-sol", "GPT 6.1 Sol (High)"),
            ("Gemini 3.8 Flash (High)", "Gemini 3.8 Flash (High)"),
            ("claude-sonnet-5", "claude-sonnet-5"),
            ("claude-sonnet-5", "Claude Sonnet 5"),
            ("claude-sonnet-5", "Claude Sonnet 5 (max)"),
            ("claude-sonnet-5", "Claude Sonnet 5 (Thinking)"),
            ("Claude Fable 5.1", "claude-fable-5-1"),
            ("GPT-6.1 Sol", "GPT 6.1 Sol (Max)"),
        ] {
            assert!(name_matches(own, row), "{own} should match {row}");
        }
        for (own, row) in [
            ("claude-sonnet-5", "claude-sonnet-5-5"),
            ("Gemini 3.8 Flash (High)", "Gemini 3.8 Flash (Low)"),
            ("Gemini 3.8 Flash (High)", "gemini-3.8-flash"),
            ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
            ("GPT 6.1 Sol", "gpt-6.1"),
            ("", ""),
        ] {
            assert!(!name_matches(own, row), "{own} must not match {row}");
        }
    }

    #[test]
    fn the_effort_is_the_word_in_the_trailing_parenthesis() {
        assert_eq!(parts("GPT 6.1 Sol (Max)"), ("gpt-6-1-sol".to_string(), Some("max".to_string())));
        assert_eq!(parts("Claude Sonnet 5 (Thinking)"), ("claude-sonnet-5".to_string(), None));
        assert_eq!(parts("Gemini (preview) 3 (xhigh effort)"), ("gemini-preview-3".to_string(), Some("xhigh".to_string())));
        assert_eq!(parts("gemini-3.8-flash"), ("gemini-3-8-flash".to_string(), None));
        // a parenthesis that is not at the end stays in the base
        assert_eq!(parts("Sol (Max) beta"), ("sol-max-beta".to_string(), None));
    }

    #[test]
    fn a_cell_lists_each_matching_row_once_in_table_order() {
        let arena = vec![
            json!({"model_name": "GPT 6.1 Sol (High)", "organization": "openai", "rank": 5}),
            json!({"model_name": "Claude Fable 5.1", "organization": "anthropic", "rank": 1}),
            json!({"model_name": "GPT 6.1 Sol (Max)", "organization": "openai", "rank": 3}),
        ];
        let both = strings(&["gpt-6.1-sol", "GPT-6.1 Sol"]);
        let cell = cell(&both, "agent_arena", "model_name", &arena);
        assert_eq!(
            cell,
            json!([{"model_name": "GPT 6.1 Sol (High)", "rank": 5}, {"model_name": "GPT 6.1 Sol (Max)", "rank": 3}]),
            "two names match the same rows, each row once, organization left out"
        );
        let aa = vec![
            json!({"name": "Gemini 3.8 Flash (Low)", "organization": "kept"}),
            json!({"name": "Gemini 3.8 Flash (High)"}),
        ];
        assert_eq!(
            super::cell(&strings(&["Gemini 3.8 Flash (High)"]), "artificial_analysis", "name", &aa),
            json!([{"name": "Gemini 3.8 Flash (High)"}])
        );
        assert!(super::cell(&strings(&["nothing-like-it"]), "artificial_analysis", "name", &aa).is_null());
    }

    #[test]
    fn a_model_without_a_name_has_three_null_cells() {
        assert!(names("claude-code", "sonnet", "sonnet", None).is_empty());
        assert_eq!(names("claude-code", "sonnet", "sonnet", Some("claude-sonnet-5")), ["claude-sonnet-5"]);
        assert_eq!(names("codex", "gpt-6.1-sol", "GPT-6.1 Sol", None), ["gpt-6.1-sol", "GPT-6.1 Sol"]);
        assert_eq!(names("antigravity", "gemini-3.8-flash-high", "gemini-3.8-flash-high", None), ["gemini-3.8-flash-high"]);
        assert_eq!(names("codex", "gpt-6.1-sol", "", None), ["gpt-6.1-sol"]);
        let rows = vec![json!({"model_name": "anything", "name": "anything"})];
        for (table, key) in TABLES {
            assert!(cell(&[], table, key, &rows).is_null(), "{table}");
        }
    }

    #[test]
    fn the_table_lines_are_padded_by_display_width() {
        let rows: Vec<Vec<String>> = [
            ["model", "AA I/C/A", "Arena #rank score", "ASL", "tokens avg", "quota/run 5h/7d", "runs left"],
            ["haiku", "無資料", "無資料", "無資料", "無資料", "無資料", "無資料"],
            ["sonnet", "62/55/48 +1", "#4 1288", "ok", "183000", "6.10%/1.20% unreliable", "8 five_hour (+1 rows)"],
            ["opus", "70/60/55 (max)", "#2 1520 (high)", "degraded", "52000", "-/0.40%", "3"],
        ]
        .iter()
        .map(|r| strings(r))
        .collect();
        assert_eq!(
            layout(&rows),
            [
                "  model   AA I/C/A        Arena #rank score  ASL       tokens avg  quota/run 5h/7d         runs left",
                "  haiku   無資料          無資料             無資料    無資料      無資料                  無資料",
                "  sonnet  62/55/48 +1     #4 1288            ok        183000      6.10%/1.20% unreliable  8 five_hour (+1 rows)",
                "  opus    70/60/55 (max)  #2 1520 (high)     degraded  52000       -/0.40%                 3",
            ]
        );
    }

    #[test]
    fn the_usage_columns_come_from_the_row_with_the_most_runs() {
        let rows = vec![
            json!({"runs": 1, "tokens_avg": 10.4, "quota_points_avg": 1.0, "quota_points_weekly_avg": null, "estimate_reliable": true, "runs_left": 4, "runs_left_window": "five_hour"}),
            json!({"runs": 3, "tokens_avg": 182999.5, "quota_points_avg": 6.1, "quota_points_weekly_avg": 1.2, "estimate_reliable": false, "runs_left": 8, "runs_left_window": "five_hour"}),
            json!({"runs": 3, "tokens_avg": 1.0, "quota_points_avg": null, "quota_points_weekly_avg": null, "estimate_reliable": true, "runs_left": null, "runs_left_window": null}),
        ];
        assert_eq!(usage_cells(&rows, "no data"), ["183000", "6.10%/1.20% unreliable", "8 five_hour (+2 rows)"]);
        assert_eq!(usage_cells(&rows[..1], "no data"), ["10", "1.00%/-", "4 five_hour"]);
        assert_eq!(usage_cells(&rows[2..], "no data"), ["1", "no data", "no data"]);
        assert_eq!(usage_cells(&[], "無資料"), ["無資料", "無資料", "無資料"]);
        let zero = vec![json!({"runs": 1, "tokens_avg": null, "quota_points_avg": 0.0, "quota_points_weekly_avg": 0.0, "estimate_reliable": true, "runs_left": 0, "runs_left_window": null})];
        assert_eq!(usage_cells(&zero, "no data"), ["no data", "0.00%/0.00%", "0"]);
    }

    #[test]
    fn the_first_line_lists_the_windows() {
        let c = json!({"profile_id": "claude-a", "plan_label": "Max", "shared": true, "provider": "anthropic", "harness": "claude-code",
            "eligible": false, "quota_state": "exhausted", "reasons": ["a", "b"],
            "windows": [
                {"name": "five_hour", "group": null, "used_percent": 96.0, "resets_at": "2026-10-21T12:00:00Z", "reset_since_observed": false},
                {"name": "seven_day", "group": "G", "used_percent": 12.5, "resets_at": null, "reset_since_observed": false},
                {"name": "weekly", "group": null, "used_percent": 80.0, "resets_at": "2026-10-01T00:00:00Z", "reset_since_observed": true}
            ],
            "models": [], "models_excluded": [], "usage_other_models": [], "models_seen_at": null, "models_note": null});
        let lines = human_block(&c, &[], Lang::En);
        assert_eq!(
            lines,
            [
                "claude-a (Max, shared)  anthropic / claude-code  ineligible exhausted  five_hour 96% used, resets 2026-10-21T12:00:00Z; seven_day (G) 12.5% used, resets unknown; weekly reset, counts as 0%",
                "  reasons: a; b",
                "  no models listed",
            ]
        );
        let c = json!({"profile_id": "codex-a", "plan_label": null, "shared": false, "provider": "openai", "harness": "codex",
            "eligible": true, "quota_state": "unknown", "windows": [], "models": []});
        assert_eq!(human_block(&c, &[], Lang::En)[0], "codex-a  openai / codex  eligible unknown  no quota reading");
        let c = json!({"profile_id": "codex-b", "plan_label": null, "shared": true, "provider": "openai", "harness": "codex",
            "eligible": true, "quota_state": "unknown", "windows": [], "models": []});
        assert!(human_block(&c, &[], Lang::En)[0].starts_with("codex-b (shared)  openai / codex"));
    }
}
