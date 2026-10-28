//! The dispatch decision table (DT-7, T12/T13): one row per model, profile,
//! task type and effort, from `models::thoth_breakdown` of every (profile, model), and
//! every launch's choice and outcome, from the run lines of this computer and
//! the shared records folder. Read only; `thoth models --json` carries it as
//! `decision_table`, and the report page and the panel read it from there.
//! The JSON carries no pass or reject counts and no verdicts: the commander
//! judges models by the external tables, not by this record; the report page
//! shows them to the human from the Rust values.

use crate::commands::models::{self, ThothBreakdown};
use crate::config::Paths;
use crate::records::{LineWithPrevious, Shared};
use anyhow::Result;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The window `summary.choices_recent` counts.
pub(crate) const RECENT_DAYS: i64 = 7;

pub(crate) const TASK_TYPE_ORDER: [&str; 5] = ["code", "research", "review", "docs", "mechanical"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub model: String,
    pub task_type: Option<String>,
    pub effort: Option<String>,
    pub profile: String,
    pub machines: Vec<String>,
    pub implementer_runs: i64,
    pub reviewed: i64,
    pub pass: i64,
    pub reject: i64,
    pub tokens: i64,
    pub quota_points: Option<f64>,
    pub quota_runs: i64,
    pub quota_points_weekly: Option<f64>,
    pub quota_runs_weekly: i64,
    pub quota_runs_unreliable: i64,
}

impl Row {
    /// PASS over PASS + REJECT; `reviewed` also counts UNVERIFIED, so it is
    /// not the denominator (as in DT-5's `evidence_of`).
    pub(crate) fn pass_rate(&self) -> Option<f64> {
        let decided = self.pass + self.reject;
        (decided != 0).then(|| self.pass as f64 / decided as f64)
    }

    pub(crate) fn tokens_avg(&self) -> Option<f64> {
        (self.tokens != 0 && self.implementer_runs != 0)
            .then(|| self.tokens as f64 / self.implementer_runs as f64)
    }

    /// Five-hour points per reliable run.
    pub(crate) fn quota_avg(&self) -> Option<f64> {
        if self.quota_runs == 0 {
            return None;
        }
        self.quota_points.map(|p| p / self.quota_runs as f64)
    }

    /// Weekly points per reliable run.
    pub(crate) fn quota_avg_weekly(&self) -> Option<f64> {
        if self.quota_runs_weekly == 0 {
            return None;
        }
        self.quota_points_weekly.map(|p| p / self.quota_runs_weekly as f64)
    }

    fn json(&self) -> Value {
        json!({
            "model": self.model,
            "task_type": self.task_type,
            "effort": self.effort,
            "profile": self.profile,
            "machines": self.machines,
            "implementer_runs": self.implementer_runs,
            "reviewed": self.reviewed,
            "tokens": self.tokens,
            "tokens_avg": self.tokens_avg(),
            "quota_points": self.quota_points,
            "quota_runs": self.quota_runs,
            "quota_points_avg": self.quota_avg(),
            "quota_points_weekly": self.quota_points_weekly,
            "quota_runs_weekly": self.quota_runs_weekly,
            "quota_points_weekly_avg": self.quota_avg_weekly(),
            "quota_runs_unreliable": self.quota_runs_unreliable,
        })
    }
}

fn add_points(sum: Option<f64>, more: Option<f64>) -> Option<f64> {
    match (sum, more) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn insert_sorted(list: &mut Vec<String>, value: &str) {
    if let Err(i) = list.binary_search_by(|x| x.as_str().cmp(value)) {
        list.insert(i, value.to_string());
    }
}

/// Listed values in their order, then unlisted ones by name, then None.
/// `recommend`'s usage rows (RJ-2) sort with it too.
pub(crate) fn rank<'a>(value: &'a Option<String>, order: &[&str]) -> (u8, usize, &'a str) {
    match value.as_deref() {
        Some(s) => match order.iter().position(|x| *x == s) {
            Some(i) => (0, i, ""),
            None => (1, 0, s),
        },
        None => (2, 0, ""),
    }
}

/// One row per (model, profile, task type, effort): difficulty is merged;
/// groups without implementer runs give no row.
pub(crate) fn rows_from(breakdowns: &[(String, String, Vec<ThothBreakdown>)]) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for (profile, model, groups) in breakdowns {
        for g in groups {
            if g.implementer_runs == 0 {
                continue;
            }
            let index = rows.iter().position(|r| {
                &r.model == model
                    && &r.profile == profile
                    && r.task_type == g.task_type
                    && r.effort == g.effort
            });
            let row = match index {
                Some(i) => &mut rows[i],
                None => {
                    rows.push(Row {
                        model: model.clone(),
                        task_type: g.task_type.clone(),
                        effort: g.effort.clone(),
                        profile: profile.clone(),
                        machines: Vec::new(),
                        implementer_runs: 0,
                        reviewed: 0,
                        pass: 0,
                        reject: 0,
                        tokens: 0,
                        quota_points: None,
                        quota_runs: 0,
                        quota_points_weekly: None,
                        quota_runs_weekly: 0,
                        quota_runs_unreliable: 0,
                    });
                    rows.last_mut().expect("just pushed")
                }
            };
            row.implementer_runs += g.implementer_runs;
            row.reviewed += g.reviewed;
            row.pass += g.pass;
            row.reject += g.reject;
            // Runs from before DT-1 have only input and output tokens.
            row.tokens += g.total_tokens.max(g.input_tokens + g.output_tokens);
            row.quota_points = add_points(row.quota_points, g.quota_points);
            row.quota_runs += g.quota_runs;
            row.quota_points_weekly = add_points(row.quota_points_weekly, g.quota_points_weekly);
            row.quota_runs_weekly += g.quota_runs_weekly;
            row.quota_runs_unreliable += g.quota_runs_unreliable;
            for machine in &g.machines {
                insert_sorted(&mut row.machines, machine);
            }
        }
    }
    rows.sort_by(|a, b| {
        a.model
            .cmp(&b.model)
            .then_with(|| a.profile.cmp(&b.profile))
            .then_with(|| rank(&a.task_type, &TASK_TYPE_ORDER).cmp(&rank(&b.task_type, &TASK_TYPE_ORDER)))
            .then_with(|| {
                rank(&a.effort, crate::adapter::effort::EFFORT_LEVELS)
                    .cmp(&rank(&b.effort, crate::adapter::effort::EFFORT_LEVELS))
            })
    });
    rows
}

/// One launch: what was chosen, why, and what became of it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Choice {
    pub run_id: String,
    pub machine: String,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub attempt: Option<i64>,
    pub role: String,
    pub started_at: Option<String>,
    pub profile: String,
    pub model: String,
    pub effort_requested: Option<String>,
    pub effort: Option<String>,
    pub task_type: Option<String>,
    pub difficulty: Option<String>,
    pub risk: Option<String>,
    pub previous_run: Option<String>,
    pub reason: Option<String>,
    pub status: String,
    /// For the report page only; not in the JSON.
    pub verdict: Option<String>,
}

impl Choice {
    fn json(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "machine": self.machine,
            "task_id": self.task_id,
            "task_title": self.task_title,
            "attempt": self.attempt,
            "role": self.role,
            "started_at": self.started_at,
            "profile": self.profile,
            "model": self.model,
            "effort_requested": self.effort_requested,
            "effort": self.effort,
            "task_type": self.task_type,
            "difficulty": self.difficulty,
            "risk": self.risk,
            "previous_run": self.previous_run,
            "reason": self.reason,
            "status": self.status,
        })
    }
}

fn parsed(started_at: &Option<String>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(started_at.as_deref()?, &Rfc3339).ok()
}

/// Every run, each role, newest first; an unknown start time sorts last.
/// The `previous_run` beside a line is read from this computer's
/// `launch_choice`; the other computers' lines do not carry it.
pub(crate) fn choices_from(lines: &[LineWithPrevious]) -> Vec<Choice> {
    let mut out: Vec<Choice> = lines
        .iter()
        .map(|(l, previous_run)| Choice {
            run_id: l.run_id.clone(),
            machine: l.machine.clone(),
            task_id: l.task_id.clone(),
            task_title: l.task_title.clone(),
            attempt: l.attempt,
            role: l.role.clone(),
            started_at: l.started_at.clone(),
            profile: l.profile.clone(),
            model: l.model.clone(),
            effort_requested: l.effort_requested.clone(),
            effort: l.effort.clone(),
            task_type: l.task_type.clone(),
            difficulty: l.difficulty.clone(),
            risk: l.risk.clone(),
            previous_run: previous_run.clone(),
            reason: l.reason.clone(),
            status: l.status.clone(),
            verdict: l.verdict.clone(),
        })
        .collect();
    out.sort_by(|a, b| {
        let by_time = match (parsed(&a.started_at), parsed(&b.started_at)) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_time.then_with(|| b.run_id.cmp(&a.run_id))
    });
    out
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Summary {
    pub runs: i64,
    pub machines: usize,
    pub rows: usize,
    pub choices: usize,
    pub choices_recent: usize,
    pub choices_recent_without_reason: usize,
}

pub(crate) fn summary_of(rows: &[Row], choices: &[Choice], now: OffsetDateTime) -> Summary {
    let since = now - Duration::days(RECENT_DAYS);
    let machines: BTreeSet<&str> = rows
        .iter()
        .flat_map(|r| r.machines.iter().map(String::as_str))
        .collect();
    let recent: Vec<&Choice> = choices
        .iter()
        .filter(|c| parsed(&c.started_at).is_some_and(|t| t >= since))
        .collect();
    Summary {
        runs: rows.iter().map(|r| r.implementer_runs).sum(),
        machines: machines.len(),
        rows: rows.len(),
        choices: choices.len(),
        choices_recent: recent.len(),
        choices_recent_without_reason: recent.iter().filter(|c| c.reason.is_none()).count(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Table {
    pub rows: Vec<Row>,
    pub choices: Vec<Choice>,
    pub summary: Summary,
}

impl Table {
    pub(crate) fn json(&self) -> Value {
        json!({
            "rows": self.rows.iter().map(Row::json).collect::<Vec<_>>(),
            "choices": self.choices.iter().map(Choice::json).collect::<Vec<_>>(),
            "summary": {
                "runs": self.summary.runs,
                "machines": self.summary.machines,
                "rows": self.summary.rows,
                "choices": self.summary.choices,
                "choices_recent": self.summary.choices_recent,
                "choices_recent_without_reason": self.summary.choices_recent_without_reason,
                "recent_days": RECENT_DAYS,
            },
        })
    }
}

/// The whole table: this computer's databases plus the shared lines whose
/// run is not here, each run once.
pub(crate) fn build(
    paths: &Paths,
    repos: &[String],
    shared: &Shared,
    now: OffsetDateTime,
) -> Result<Table> {
    let breakdowns = models::all_breakdowns(paths, repos, shared)?;
    let mut lines = crate::records::collect_with_previous_run(paths)?;
    let mut seen: HashSet<String> = lines.iter().map(|(l, _)| l.run_id.clone()).collect();
    for line in &shared.lines {
        if seen.insert(line.run_id.clone()) {
            lines.push((line.clone(), None));
        }
    }
    let rows = rows_from(&breakdowns);
    let choices = choices_from(&lines);
    let summary = summary_of(&rows, &choices, now);
    Ok(Table { rows, choices, summary })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{Line, Tokens};

    fn group(
        effort: Option<&str>,
        task_type: Option<&str>,
        difficulty: Option<&str>,
        runs: i64,
        pass: i64,
        reject: i64,
        machines: &[&str],
    ) -> ThothBreakdown {
        ThothBreakdown {
            effort: effort.map(str::to_string),
            task_type: task_type.map(str::to_string),
            difficulty: difficulty.map(str::to_string),
            implementer_runs: runs,
            completed: 0,
            reviewed: 0,
            pass,
            reject,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
            quota_points: None,
            quota_runs: 0,
            quota_points_weekly: None,
            quota_runs_weekly: 0,
            quota_runs_unreliable: 0,
            machines: machines.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn u1_input() -> Vec<(String, String, Vec<ThothBreakdown>)> {
        vec![
            (
                "claude-a".into(),
                "sonnet".into(),
                vec![
                    group(Some("high"), Some("code"), Some("high"), 2, 1, 1, &["alpha"]),
                    group(Some("high"), Some("code"), Some("normal"), 1, 1, 0, &["alpha"]),
                ],
            ),
            (
                "claude-b".into(),
                "sonnet".into(),
                vec![
                    group(Some("medium"), Some("code"), Some("high"), 1, 0, 0, &["beta"]),
                    group(Some("high"), Some("code"), Some("high"), 1, 1, 0, &["beta"]),
                ],
            ),
            (
                "claude-a".into(),
                "haiku".into(),
                vec![group(None, Some("docs"), Some("low"), 1, 0, 0, &["alpha"])],
            ),
        ]
    }

    #[test]
    fn rows_merge_difficulty_but_not_profile_or_effort() {
        let rows = rows_from(&u1_input());
        assert_eq!(rows.len(), 4);
        let keys: Vec<(&str, &str, Option<&str>, Option<&str>)> = rows
            .iter()
            .map(|r| (r.model.as_str(), r.profile.as_str(), r.task_type.as_deref(), r.effort.as_deref()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("haiku", "claude-a", Some("docs"), None),
                ("sonnet", "claude-a", Some("code"), Some("high")),
                ("sonnet", "claude-b", Some("code"), Some("medium")),
                ("sonnet", "claude-b", Some("code"), Some("high")),
            ]
        );
        let a_high = &rows[1];
        assert_eq!(a_high.implementer_runs, 3);
        assert_eq!(a_high.pass, 2);
        assert_eq!(a_high.reject, 1);
        assert_eq!(a_high.pass_rate(), Some(2.0 / 3.0));
        assert_eq!(a_high.machines, vec!["alpha".to_string()]);
        let b_high = &rows[3];
        assert_eq!(b_high.implementer_runs, 1);
        assert_eq!(b_high.pass, 1);
        assert_eq!(b_high.reject, 0);
        assert_eq!(b_high.pass_rate(), Some(1.0));
        assert_eq!(b_high.machines, vec!["beta".to_string()]);
        assert_eq!(rows[2].pass_rate(), None);
        assert_eq!(rows[0].pass_rate(), None);
    }

    #[test]
    fn tokens_take_the_larger_of_total_and_input_plus_output_per_group() {
        let input = vec![(
            "claude-a".to_string(),
            "sonnet".to_string(),
            vec![
                ThothBreakdown {
                    total_tokens: 1000,
                    input_tokens: 10,
                    output_tokens: 20,
                    ..group(Some("high"), Some("code"), Some("high"), 1, 0, 0, &["alpha"])
                },
                ThothBreakdown {
                    total_tokens: 0,
                    input_tokens: 300,
                    output_tokens: 400,
                    ..group(Some("high"), Some("code"), Some("normal"), 1, 0, 0, &["alpha"])
                },
            ],
        )];
        let rows = rows_from(&input);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens, 1700);
        assert_eq!(rows[0].tokens_avg(), Some(850.0));
        let zero = vec![(
            "claude-a".to_string(),
            "sonnet".to_string(),
            vec![group(Some("high"), Some("code"), Some("high"), 1, 0, 0, &["alpha"])],
        )];
        assert_eq!(rows_from(&zero)[0].tokens_avg(), None);
    }

    #[test]
    fn quota_averages_use_only_reliable_runs() {
        let input = vec![(
            "claude-a".to_string(),
            "sonnet".to_string(),
            vec![ThothBreakdown {
                quota_points: Some(96.0),
                quota_runs: 2,
                quota_runs_unreliable: 1,
                quota_points_weekly: None,
                ..group(Some("high"), Some("code"), Some("high"), 3, 0, 0, &["alpha"])
            }],
        )];
        let rows = rows_from(&input);
        assert_eq!(rows[0].quota_avg(), Some(48.0));
        assert_eq!(rows[0].quota_avg_weekly(), None);
        assert_eq!(rows[0].quota_runs_unreliable, 1);
    }

    fn line(run_id: &str, started_at: Option<&str>) -> Line {
        Line {
            v: 2,
            machine: "alpha".into(),
            repo_id: "repo".into(),
            repo: Some("Thoth".into()),
            run_id: run_id.into(),
            task_id: Some(format!("T-{run_id}")),
            task_title: Some(format!("title {run_id}")),
            attempt: Some(1),
            role: "implementer".into(),
            profile: "claude-a".into(),
            provider: "anthropic".into(),
            harness: Some("claude-code".into()),
            model: "sonnet".into(),
            effort_requested: Some("high".into()),
            effort: Some("high".into()),
            task_type: Some("code".into()),
            difficulty: Some("high".into()),
            risk: Some("normal".into()),
            status: "completed".into(),
            interrupted_reason: None,
            exit_code: Some(0),
            started_at: started_at.map(str::to_string),
            ended_at: started_at.map(str::to_string),
            verdict: Some("PASS".into()),
            tokens: Tokens::default(),
            quota: None,
            reason: None,
        }
    }

    #[test]
    fn choices_list_every_line_newest_first_with_its_fields() {
        let a = Line {
            reason: Some("codex is busy".into()),
            role: "reviewer".into(),
            attempt: Some(2),
            effort_requested: Some("xhigh".into()),
            effort: Some("high".into()),
            task_type: Some("docs".into()),
            difficulty: Some("low".into()),
            risk: Some("important".into()),
            verdict: Some("REJECT".into()),
            status: "failed".into(),
            ..line("A", Some("2026-01-03T00:00:00Z"))
        };
        let b = line("B", Some("2026-01-05T00:00:00Z"));
        let c = line("C", None);
        let d = line("D", Some("2026-01-05T00:00:00Z"));
        let out = choices_from(&[
            (a, Some("R-PREV".into())),
            (b, None),
            (c, None),
            (d, None),
        ]);
        let ids: Vec<&str> = out.iter().map(|c| c.run_id.as_str()).collect();
        assert_eq!(ids, vec!["D", "B", "A", "C"]);
        let a = &out[2];
        assert_eq!(a.machine, "alpha");
        assert_eq!(a.task_id.as_deref(), Some("T-A"));
        assert_eq!(a.task_title.as_deref(), Some("title A"));
        assert_eq!(a.attempt, Some(2));
        assert_eq!(a.role, "reviewer");
        assert_eq!(a.started_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(a.profile, "claude-a");
        assert_eq!(a.model, "sonnet");
        assert_eq!(a.effort_requested.as_deref(), Some("xhigh"));
        assert_eq!(a.effort.as_deref(), Some("high"));
        assert_eq!(a.task_type.as_deref(), Some("docs"));
        assert_eq!(a.difficulty.as_deref(), Some("low"));
        assert_eq!(a.risk.as_deref(), Some("important"));
        assert_eq!(a.previous_run.as_deref(), Some("R-PREV"));
        assert_eq!(a.reason.as_deref(), Some("codex is busy"));
        assert_eq!(a.status, "failed");
        assert_eq!(a.verdict.as_deref(), Some("REJECT"));
        assert_eq!(out[1].previous_run, None);
        assert_eq!(out[1].reason, None);
        let keys: Vec<String> = a.json().as_object().unwrap().keys().cloned().collect();
        let mut expected = vec![
            "run_id", "machine", "task_id", "task_title", "attempt", "role", "started_at",
            "profile", "model", "effort_requested", "effort", "task_type", "difficulty", "risk",
            "previous_run", "reason", "status",
        ];
        expected.sort();
        let mut keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, expected);
    }

    fn choice(run_id: &str, started_at: Option<String>, reason: Option<&str>) -> Choice {
        choices_from(&[(
            Line {
                started_at,
                reason: reason.map(str::to_string),
                ..line(run_id, None)
            },
            None,
        )])
        .remove(0)
    }

    #[test]
    fn summary_counts_runs_machines_and_recent_choices_without_a_reason() {
        let rows = rows_from(&u1_input());
        let now = OffsetDateTime::now_utc();
        let yesterday = (now - Duration::days(1)).format(&Rfc3339).unwrap();
        let choices = vec![
            choice("R-1", Some(yesterday.clone()), None),
            choice("R-2", Some(yesterday), Some("tests are many")),
            choice("R-3", Some("2026-01-01T00:00:00Z".into()), None),
            choice("R-4", None, None),
        ];
        let summary = summary_of(&rows, &choices, now);
        assert_eq!(summary.runs, 6);
        assert_eq!(summary.machines, 2);
        assert_eq!(summary.rows, 4);
        assert_eq!(summary.choices, 4);
        assert_eq!(summary.choices_recent, 2);
        assert_eq!(summary.choices_recent_without_reason, 1);
    }

    #[test]
    fn the_json_has_no_pass_or_reject_counts_and_no_verdicts() {
        let rows = rows_from(&u1_input());
        let choices = vec![choice("R-1", None, None)];
        let summary = summary_of(&rows, &choices, OffsetDateTime::now_utc());
        let v = Table { rows, choices, summary }.json();
        for row in v["rows"].as_array().unwrap() {
            for key in ["pass", "reject", "pass_rate"] {
                assert!(row.get(key).is_none(), "{key} in {row}");
            }
        }
        assert!(v["choices"][0].get("verdict").is_none());
        assert!(v["summary"].get("deviations").is_none());
    }
}
