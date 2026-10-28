//! Thoth's own usage per account, model, effort and task type (RJ-2): what a
//! run costs on average and how many more runs the account's windows allow.
//! Only usage; no verdicts, no pass rate (the user, 2026-10-17: the records
//! keep how much was used, the external tables say how good a model is).

use crate::commands::decision_table::{rank, TASK_TYPE_ORDER};
use crate::commands::usage::{RankedCandidate, Window, EXHAUSTED_PERCENT};
use crate::config::Paths;
use crate::plan_label;
use crate::records::Line;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// One row of `usage`.
pub(crate) struct Row {
    pub(crate) role: String,
    pub(crate) profile: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) task_type: Option<String>,
    pub(crate) plan: Option<String>,
    pub(crate) plan_label: Option<String>,
    pub(crate) shared: bool,
    pub(crate) machines: Vec<String>,
    pub(crate) runs: i64,
    pub(crate) completed: i64,
    tokens_sum: i64,
    /// This computer's runs only: (five-hour points, weekly points) per run,
    /// reliable and unreliable kept apart.
    reliable: Vec<(Option<f64>, Option<f64>)>,
    unreliable: Vec<(Option<f64>, Option<f64>)>,
    pub(crate) quota_runs_unreliable: i64,
    pub(crate) runs_left: Option<i64>,
    pub(crate) runs_left_window: Option<&'static str>,
}

/// An average of the runs that have a number; `None` when none has.
fn average(values: impl Iterator<Item = Option<f64>>) -> (Option<f64>, i64) {
    let numbers: Vec<f64> = values.flatten().collect();
    if numbers.is_empty() {
        return (None, 0);
    }
    let n = numbers.len();
    (Some(numbers.iter().sum::<f64>() / n as f64), n as i64)
}

impl Row {
    pub(crate) fn tokens_avg(&self) -> Option<f64> {
        (self.tokens_sum != 0 && self.runs != 0).then(|| self.tokens_sum as f64 / self.runs as f64)
    }

    /// (five-hour average, weekly average, runs behind the five-hour one,
    /// whether the estimate is the account's own). The reliable runs when the
    /// group has any, whether or not they carry a number (review of attempt
    /// 2: a reliable run without numbers gives null averages, not the
    /// unreliable runs' ones); only a group without any reliable run falls
    /// back to the unreliable ones, marked as not the account's own.
    fn quota(&self) -> (Option<f64>, Option<f64>, i64, bool) {
        let (runs, own) = if self.reliable.is_empty() {
            (&self.unreliable, false)
        } else {
            (&self.reliable, !self.shared)
        };
        let (five, n) = average(runs.iter().map(|r| r.0));
        let (seven, _) = average(runs.iter().map(|r| r.1));
        (five, seven, n, own)
    }

    pub(crate) fn quota_points_avg(&self) -> Option<f64> {
        self.quota().0
    }

    pub(crate) fn quota_points_weekly_avg(&self) -> Option<f64> {
        self.quota().1
    }

    pub(crate) fn estimate_reliable(&self) -> bool {
        self.quota().3
    }

    pub(crate) fn json(&self) -> Value {
        let (five, seven, quota_runs, reliable) = self.quota();
        json!({
            "role": self.role,
            "profile": self.profile,
            "model": self.model,
            "effort": self.effort,
            "task_type": self.task_type,
            "plan": self.plan,
            "plan_label": self.plan_label,
            "shared": self.shared,
            "machines": self.machines,
            "runs": self.runs,
            "completed": self.completed,
            "tokens_avg": self.tokens_avg(),
            "quota_runs": quota_runs,
            "quota_points_avg": five,
            "quota_points_weekly_avg": seven,
            "quota_runs_unreliable": self.quota_runs_unreliable,
            "estimate_reliable": reliable,
            "runs_left": self.runs_left,
            "runs_left_window": self.runs_left_window,
        })
    }

    pub(crate) fn human(&self) -> String {
        let num = |v: Option<f64>| v.map_or("null".to_string(), |x| format!("{x:.2}"));
        let left = match (self.runs_left, self.runs_left_window) {
            (Some(n), Some(w)) => format!("runs left {n} ({w})"),
            // An exhausted account whose runs give no average: the 0 is
            // known, the window it would come from is not.
            (Some(n), None) => format!("runs left {n} (window unknown)"),
            _ => "runs left unknown".to_string(),
        };
        format!(
            "usage: {} {} {} effort {} {}: runs {} (completed {}), tokens avg {}, quota avg {}/{} per run (5h/7d, {} run(s)), {}{}",
            self.role,
            plan_label::named(&self.profile, self.plan_label.as_deref(), crate::lang::Lang::En),
            self.model,
            self.effort.as_deref().unwrap_or("null"),
            self.task_type.as_deref().unwrap_or("null"),
            self.runs,
            self.completed,
            num(self.tokens_avg()),
            num(self.quota_points_avg()),
            num(self.quota_points_weekly_avg()),
            self.quota().2,
            left,
            if self.estimate_reliable() { "" } else { " (estimate unreliable)" }
        )
    }
}

/// How many more runs the account's windows allow: each window's remaining
/// share (95% minus what is used; a reset window counts as 0% used) over the
/// average cost of one run, the smaller of the two, with the window it came
/// from. An exhausted account has 0; a window without a reading or without a
/// positive average gives nothing; nothing from either is `None`.
pub(crate) fn runs_left(
    five_hour_used: Option<f64>,
    seven_day_used: Option<f64>,
    five_hour_avg: Option<f64>,
    seven_day_avg: Option<f64>,
    exhausted: bool,
) -> (Option<i64>, Option<&'static str>) {
    let per_window = |used: Option<f64>, avg: Option<f64>| -> Option<i64> {
        let avg = avg.filter(|a| *a > 0.0)?;
        let remaining = (EXHAUSTED_PERCENT - used?).max(0.0);
        Some((remaining / avg).floor() as i64)
    };
    let five = per_window(five_hour_used, five_hour_avg).map(|n| (n, "five_hour"));
    let seven = per_window(seven_day_used, seven_day_avg).map(|n| (n, "seven_day"));
    let best = match (five, seven) {
        (Some(a), Some(b)) => Some(if b.0 < a.0 { b } else { a }),
        (a, b) => a.or(b),
    };
    if exhausted {
        return (Some(0), best.map(|(_, w)| w));
    }
    match best {
        Some((n, w)) => (Some(n), Some(w)),
        None => (None, None),
    }
}

/// A window's used percent for `runs_left`: 0 once it has reset.
fn used_percent(windows: &[Window], name: &str) -> Option<f64> {
    let w = windows.iter().find(|w| w.name == name)?;
    Some(if w.is_reset() { 0.0 } else { w.used.clamp(0.0, 100.0) })
}

fn role_matches(role: &str, line_role: &str) -> bool {
    match role {
        "reviewer" => line_role == "reviewer" || line_role == "spec_reviewer",
        other => line_role == other,
    }
}

/// The usage rows for `role`: this computer's runs plus the shared folder's
/// lines whose run is not here (each run once), grouped by the run's own
/// role, profile, model, effort and task type (review of attempt 3: a spec
/// reviewer run is not merged into the reviewer row of the same account and
/// model); only accounts registered here.
pub(crate) fn build(
    paths: &Paths,
    role: &str,
    candidates: &[RankedCandidate],
    plans: &BTreeMap<String, crate::db::global::StoredPlan>,
) -> Result<Vec<Row>> {
    let local = crate::records::collect(paths)?;
    let shared = crate::records::load_shared(paths);
    // Each run once, as `decision_table::build` merges the lines: this
    // computer's run ids first, then every remote run id as it is taken, so a
    // run repeated in one shared file or across files counts a single time.
    let mut seen: HashSet<&str> = local.iter().map(|l| l.run_id.as_str()).collect();
    let global = crate::commands::usage::global_read_only(paths)?;
    let shared_marks: BTreeMap<&str, bool> = candidates
        .iter()
        .map(|c| {
            let id = c.profile.profile_id.as_str();
            // A global.db older than the mark has no column: not shared.
            (id, crate::db::global::profile_shared(&global, id).unwrap_or(false))
        })
        .collect();
    let mut rows: Vec<Row> = Vec::new();
    let mut add = |line: &Line, local_run: bool| {
        if !role_matches(role, &line.role) {
            return;
        }
        let Some(candidate) = candidates.iter().find(|c| c.profile.profile_id == line.profile) else {
            return;
        };
        let index = rows.iter().position(|r| {
            r.role == line.role
                && r.profile == line.profile
                && r.model == line.model
                && r.effort == line.effort
                && r.task_type == line.task_type
        });
        let row = match index {
            Some(i) => &mut rows[i],
            None => {
                let plan = plans.get(&line.profile).map(|s| s.plan.clone());
                let label = plan_label::label(plan.as_deref(), candidate.profile.account_label.as_deref());
                rows.push(Row {
                    role: line.role.clone(),
                    profile: line.profile.clone(),
                    model: line.model.clone(),
                    effort: line.effort.clone(),
                    task_type: line.task_type.clone(),
                    plan,
                    plan_label: label,
                    shared: shared_marks.get(line.profile.as_str()).copied().unwrap_or(false),
                    machines: Vec::new(),
                    runs: 0,
                    completed: 0,
                    tokens_sum: 0,
                    reliable: Vec::new(),
                    unreliable: Vec::new(),
                    quota_runs_unreliable: 0,
                    runs_left: None,
                    runs_left_window: None,
                });
                rows.last_mut().expect("just pushed")
            }
        };
        row.runs += 1;
        row.completed += i64::from(line.status == "completed");
        let t = &line.tokens;
        row.tokens_sum += t
            .total
            .unwrap_or(0)
            .max(t.input.unwrap_or(0) + t.output.unwrap_or(0));
        if !row.machines.contains(&line.machine) {
            row.machines.push(line.machine.clone());
            row.machines.sort();
        }
        // Quota averages come from this computer's runs only: an account of
        // the same name on another computer may be another subscription.
        if local_run {
            if let Some(q) = &line.quota {
                let points = (q.five_hour_points, q.seven_day_points);
                if q.reliable {
                    row.reliable.push(points);
                } else {
                    row.unreliable.push(points);
                    row.quota_runs_unreliable += 1;
                }
            }
        }
    };
    for line in &local {
        add(line, true);
    }
    for line in &shared.lines {
        if seen.insert(line.run_id.as_str()) {
            add(line, false);
        }
    }
    for row in &mut rows {
        let Some(candidate) = candidates.iter().find(|c| c.profile.profile_id == row.profile) else {
            continue;
        };
        if candidate.profile.harness == "antigravity" {
            continue;
        }
        let (five, seven, _, _) = row.quota();
        let (left, window) = runs_left(
            used_percent(&candidate.windows, "five_hour"),
            used_percent(&candidate.windows, "seven_day"),
            five,
            seven,
            candidate.quota_state == "exhausted",
        );
        row.runs_left = left;
        row.runs_left_window = window;
    }
    // The decision table's order; rows it would never have apart (the same
    // group under two run roles) then by role.
    rows.sort_by(|a, b| {
        a.model
            .cmp(&b.model)
            .then_with(|| a.profile.cmp(&b.profile))
            .then_with(|| rank(&a.task_type, &TASK_TYPE_ORDER).cmp(&rank(&b.task_type, &TASK_TYPE_ORDER)))
            .then_with(|| {
                rank(&a.effort, crate::adapter::effort::EFFORT_LEVELS)
                    .cmp(&rank(&b.effort, crate::adapter::effort::EFFORT_LEVELS))
            })
            .then_with(|| a.role.cmp(&b.role))
    });
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_left_divides_the_remaining_share_by_the_average() {
        assert_eq!(runs_left(Some(55.0), None, Some(7.5), None, false), (Some(5), Some("five_hour")));
        // two windows: the smaller count wins and names its window
        assert_eq!(
            runs_left(Some(55.0), Some(80.0), Some(7.5), Some(2.0), false),
            (Some(5), Some("five_hour"))
        );
        assert_eq!(
            runs_left(Some(55.0), Some(85.0), Some(7.5), Some(5.0), false),
            (Some(2), Some("seven_day"))
        );
        // a window whose average is 0 or null is skipped
        assert_eq!(runs_left(Some(55.0), Some(10.0), Some(0.0), Some(5.0), false), (Some(17), Some("seven_day")));
        assert_eq!(runs_left(Some(55.0), Some(10.0), None, Some(5.0), false), (Some(17), Some("seven_day")));
        // no reading, or no average at all
        assert_eq!(runs_left(None, None, Some(7.5), Some(5.0), false), (None, None));
        assert_eq!(runs_left(Some(55.0), Some(10.0), None, None, false), (None, None));
        // exhausted is 0 whatever the arithmetic says
        assert_eq!(runs_left(Some(96.0), None, Some(7.5), None, true), (Some(0), Some("five_hour")));
        assert_eq!(runs_left(None, None, None, None, true), (Some(0), None));
        // past 95% without the exhausted state still floors at 0
        assert_eq!(runs_left(Some(99.0), None, Some(1.0), None, false), (Some(0), Some("five_hour")));
    }

    fn window(name: &str, used: f64, resets_at: Option<i64>) -> Window {
        Window {
            name: name.into(),
            used,
            resets_at,
            observed_at: "2026-10-16T00:00:00Z".into(),
            source: "thoth_events",
            group: None,
            group_description: None,
        }
    }

    #[test]
    fn a_reset_window_counts_as_zero_percent_used() {
        let windows = vec![window("five_hour", 80.0, Some(0)), window("seven_day", 30.0, Some(4_102_444_800))];
        assert_eq!(used_percent(&windows, "five_hour"), Some(0.0));
        assert_eq!(used_percent(&windows, "seven_day"), Some(30.0));
        assert_eq!(used_percent(&windows, "weekly"), None);
    }

    // The averages (the reliable runs whenever the group has one, numbers or
    // not; else the unreliable ones with numbers; else null) are checked from
    // `recommend --json` in tests/recommend_facts.rs, on runs read from the
    // database.
}
