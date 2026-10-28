//! `thoth retro`: which kinds of review finding keep coming back.
//!
//! Every finding a reviewer reported in the last N days - each unmet
//! criterion and each extra finding - is grouped by the `category` the
//! reviewer picked from `review_json::CATEGORIES`. Grouping is plain counting,
//! so the same reviews always give the same answer. A category seen twice or
//! more, even in the same task's reworks, is worth turning into a lint rule or
//! a check.
//!
//! Reads every registered repo on this machine through `usage::read_only`, the
//! way `thoth report` does, and writes nothing.

use crate::commands::usage;
use crate::config::Paths;
use crate::output;
use crate::review_json::{CATEGORIES, FIX_SIZES};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The group of findings with no category, or one not in `CATEGORIES`.
pub const UNCLASSIFIED: &str = "unclassified";

/// One stored review, as read from a repo's state.db.
#[derive(Debug, Clone)]
pub struct ReviewRow {
    /// The repo's root path, from global.db's `repos` table.
    pub repo: String,
    pub task_id: String,
    pub title: String,
    pub review_id: String,
    pub created_at: String,
    /// `reviews.findings` as stored.
    pub findings: String,
}

/// How many findings of each fix size; `unsized` counts those without one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sizes {
    pub s: usize,
    pub m: usize,
    pub l: usize,
    pub none: usize,
}

impl Sizes {
    fn add(&mut self, fix_size: Option<&str>) {
        match fix_size {
            Some("S") => self.s += 1,
            Some("M") => self.m += 1,
            Some("L") => self.l += 1,
            _ => self.none += 1,
        }
    }

    fn to_json(self) -> Value {
        json!({"S": self.s, "M": self.m, "L": self.l, "unsized": self.none})
    }

    fn human(self) -> String {
        format!("S {} M {} L {} unsized {}", self.s, self.m, self.l, self.none)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRef {
    pub repo: String,
    pub task_id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub repo: String,
    pub task_id: String,
    pub review_id: String,
    pub created_at: String,
    /// `criterion` or `extra`.
    pub source: &'static str,
    /// The criterion id; `None` for an extra finding.
    pub id: Option<String>,
    /// An extra finding's severity; `None` for a criterion.
    pub severity: Option<String>,
    pub fix_size: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub category: String,
    /// In the order each task first appears.
    pub tasks: Vec<TaskRef>,
    pub sizes: Sizes,
    pub suggest_check: bool,
    pub findings: Vec<Item>,
}

#[derive(Debug, Clone)]
pub struct Retro {
    pub days: u32,
    pub since: OffsetDateTime,
    /// Reviews created inside the window.
    pub reviews: usize,
    pub sizes: Sizes,
    pub categories: Vec<Group>,
}

impl Retro {
    pub fn finding_count(&self) -> usize {
        self.categories.iter().map(|g| g.findings.len()).sum()
    }
}

/// Groups the findings of the reviews created no earlier than `days` × 24
/// hours before `now`. A review whose `created_at` is not RFC 3339 is skipped.
pub fn summarise(rows: &[ReviewRow], days: u32, now: OffsetDateTime) -> Retro {
    let since = now - Duration::days(i64::from(days));
    // Oldest first, so each group lists its tasks in the order they first
    // appeared.
    let mut in_window: Vec<(OffsetDateTime, &ReviewRow)> = rows
        .iter()
        .filter_map(|r| OffsetDateTime::parse(&r.created_at, &Rfc3339).ok().map(|t| (t, r)))
        .filter(|(t, _)| *t >= since)
        .collect();
    in_window.sort_by_key(|(t, _)| *t);

    let mut sizes = Sizes::default();
    let mut groups: Vec<Group> = Vec::new();
    for (_, review) in &in_window {
        for (category, item) in findings_of(review) {
            sizes.add(item.fix_size.as_deref());
            let at = match groups.iter().position(|g| g.category == category) {
                Some(at) => at,
                None => {
                    groups.push(Group {
                        category: category.clone(),
                        tasks: Vec::new(),
                        sizes: Sizes::default(),
                        suggest_check: false,
                        findings: Vec::new(),
                    });
                    groups.len() - 1
                }
            };
            let g = &mut groups[at];
            if !g.tasks.iter().any(|t| t.repo == review.repo && t.task_id == review.task_id) {
                g.tasks.push(TaskRef {
                    repo: review.repo.clone(),
                    task_id: review.task_id.clone(),
                    title: review.title.clone(),
                });
            }
            g.sizes.add(item.fix_size.as_deref());
            g.findings.push(item);
        }
    }
    for g in &mut groups {
        g.suggest_check =
            g.findings.len() >= 2 && g.category != "other" && g.category != UNCLASSIFIED;
    }
    groups.sort_by(|a, b| {
        b.findings.len().cmp(&a.findings.len()).then_with(|| a.category.cmp(&b.category))
    });
    Retro { days, since, reviews: in_window.len(), sizes, categories: groups }
}

/// A review's findings with the group each belongs to: every unmet criterion
/// and every extra finding. Stored findings are already normalised by
/// `review_json::parse_review`; anything else is unclassified or unsized.
fn findings_of(review: &ReviewRow) -> Vec<(String, Item)> {
    let stored: Value = serde_json::from_str(&review.findings).unwrap_or(Value::Null);
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let category = |v: &Value| {
        text(v, "category")
            .filter(|c| CATEGORIES.iter().any(|(name, _)| name == c))
            .unwrap_or_else(|| UNCLASSIFIED.to_string())
    };
    let fix_size = |v: &Value| text(v, "fix_size").filter(|s| FIX_SIZES.contains(&s.as_str()));
    let item = |v: &Value, source, id, severity| Item {
        repo: review.repo.clone(),
        task_id: review.task_id.clone(),
        review_id: review.review_id.clone(),
        created_at: review.created_at.clone(),
        source,
        id,
        severity,
        fix_size: fix_size(v),
        note: text(v, "note").unwrap_or_default(),
    };
    let list = |key: &str| stored.get(key).and_then(Value::as_array).cloned().unwrap_or_default();

    let mut out = Vec::new();
    for c in list("criteria") {
        if text(&c, "verdict").as_deref().map(str::trim) == Some("unmet") {
            out.push((category(&c), item(&c, "criterion", text(&c, "id"), None)));
        }
    }
    for e in list("extra_findings") {
        out.push((category(&e), item(&e, "extra", None, Some(text(&e, "severity").unwrap_or_default()))));
    }
    out
}

pub fn run(paths: &Paths, days: u32, json_output: bool) -> Result<i32> {
    let rows = load(paths)?;
    let retro = summarise(&rows, days, OffsetDateTime::now_utc());
    output::emit(json_output, &to_json(&retro), &human(&retro));
    Ok(0)
}

/// Every review of every registered repo whose state.db exists. No global.db
/// means nothing has been reviewed on this machine yet.
fn load(paths: &Paths) -> Result<Vec<ReviewRow>> {
    if !paths.global_db().is_file() {
        return Ok(Vec::new());
    }
    let global = usage::global_read_only(paths)?;
    let mut repos = Vec::new();
    {
        let mut stmt = global.prepare(
            "SELECT repo_id, COALESCE(root_path, '') FROM repos ORDER BY root_path, repo_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (repo_id, root) = row?;
            let path = paths.state_db(&repo_id);
            if path.is_file() {
                repos.push((root, usage::read_only(&path)?));
            }
        }
    }
    let mut out = Vec::new();
    for (root, conn) in &repos {
        out.extend(load_reviews(root, conn)?);
    }
    Ok(out)
}

fn load_reviews(repo: &str, conn: &Connection) -> Result<Vec<ReviewRow>> {
    let mut stmt = conn.prepare(
        "SELECT v.review_id, COALESCE(v.created_at, ''), COALESCE(v.findings, ''),
                COALESCE(r.task_id, ''), COALESCE(t.title, '')
         FROM reviews v
         JOIN runs r ON r.run_id = v.target_run_id
         LEFT JOIN tasks t ON t.task_id = r.task_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ReviewRow {
            repo: repo.to_string(),
            review_id: r.get(0)?,
            created_at: r.get(1)?,
            findings: r.get(2)?,
            task_id: r.get(3)?,
            title: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn to_json(r: &Retro) -> Value {
    let categories: Vec<Value> = r
        .categories
        .iter()
        .map(|g| {
            json!({
                "category": g.category,
                "count": g.findings.len(),
                "task_count": g.tasks.len(),
                "tasks": g.tasks.iter().map(|t| json!({
                    "repo": t.repo, "task_id": t.task_id, "title": t.title,
                })).collect::<Vec<_>>(),
                "sizes": g.sizes.to_json(),
                "suggest_check": g.suggest_check,
                "findings": g.findings.iter().map(|f| json!({
                    "repo": f.repo,
                    "task_id": f.task_id,
                    "review_id": f.review_id,
                    "created_at": f.created_at,
                    "source": f.source,
                    "id": f.id,
                    "severity": f.severity,
                    "fix_size": f.fix_size,
                    "note": f.note,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "days": r.days,
        "since": r.since.format(&Rfc3339).unwrap_or_default(),
        "reviews": r.reviews,
        "findings": r.finding_count(),
        "sizes": r.sizes.to_json(),
        "categories": categories,
    })
}

fn human(r: &Retro) -> String {
    if r.categories.is_empty() {
        return format!("retro: no review findings in the last {} days", r.days);
    }
    let mut lines = vec![format!(
        "retro: last {} days (since {}): {} reviews, {} findings ({})",
        r.days,
        r.since.format(&Rfc3339).unwrap_or_default(),
        r.reviews,
        r.finding_count(),
        r.sizes.human()
    )];
    for g in &r.categories {
        lines.push(format!(
            "{}  {} findings in {} tasks  ({})",
            g.category,
            g.findings.len(),
            g.tasks.len(),
            g.sizes.human()
        ));
        if g.suggest_check {
            lines.push(format!(
                "  seen {} times in {} tasks: consider turning it into a lint rule or a check",
                g.findings.len(),
                g.tasks.len()
            ));
        }
        for t in &g.tasks {
            lines.push(format!("  - {}  {}", t.task_id, t.title));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-10-13T12:00:00Z", &Rfc3339).unwrap()
    }

    fn row(task: &str, created_at: &str, findings: &str) -> ReviewRow {
        ReviewRow {
            repo: "/repo".to_string(),
            task_id: task.to_string(),
            title: format!("title of {task}"),
            review_id: format!("R-{task}-{created_at}"),
            created_at: created_at.to_string(),
            findings: findings.to_string(),
        }
    }

    fn unmet(category: &str, size: &str) -> String {
        format!(
            r#"{{"criteria":[{{"id":"AC1","verdict":"unmet","evidence":[],"note":"n","fix_size":"{size}","category":"{category}"}},
                {{"id":"AC2","verdict":"met","evidence":["E001"],"note":null}}],"extra_findings":[]}}"#
        )
    }

    fn group<'a>(r: &'a Retro, category: &str) -> &'a Group {
        r.categories.iter().find(|g| g.category == category).unwrap_or_else(|| panic!("no {category} group"))
    }

    #[test]
    fn retro_counts_reviews_from_the_start_of_the_window_on() {
        let rows = vec![
            row("T1", "2026-10-06T12:00:00Z", &unmet("docs", "S")),
            row("T2", "2026-10-06T11:59:59Z", &unmet("docs", "S")),
            row("T3", "yesterday", &unmet("docs", "S")),
        ];
        let r = summarise(&rows, 7, now());
        assert_eq!(r.since.format(&Rfc3339).unwrap(), "2026-10-06T12:00:00Z");
        assert_eq!(r.reviews, 1, "exactly at the start counts, a second earlier does not, unreadable is skipped");
        assert_eq!(r.finding_count(), 1);
        assert_eq!(group(&r, "docs").tasks[0].task_id, "T1");
    }

    #[test]
    fn retro_counts_unmet_criteria_and_extras_but_not_met_ones() {
        let findings = r#"{"criteria":[
            {"id":"AC1","verdict":" unmet ","evidence":[],"note":"n","category":"docs","fix_size":"M"},
            {"id":"AC2","verdict":"met","evidence":["E001"],"category":"docs","fix_size":"S"}],
            "extra_findings":[{"severity":"minor","note":"x","evidence":[],"category":"docs","fix_size":"L"}]}"#;
        let r = summarise(&[row("T1", "2026-10-12T00:00:00Z", findings)], 7, now());
        let g = group(&r, "docs");
        assert_eq!(g.findings.len(), 2);
        assert_eq!(g.findings[0].source, "criterion");
        assert_eq!(g.findings[0].id.as_deref(), Some("AC1"));
        assert_eq!(g.findings[0].severity, None);
        assert_eq!(g.findings[1].source, "extra");
        assert_eq!(g.findings[1].id, None);
        assert_eq!(g.findings[1].severity.as_deref(), Some("minor"));
        assert_eq!(g.sizes, Sizes { s: 0, m: 1, l: 1, none: 0 });
    }

    #[test]
    fn retro_suggests_a_check_for_a_category_repeated_across_tasks() {
        let rows = vec![
            row("T1", "2026-10-10T00:00:00Z", &unmet("test_interface", "S")),
            row("T2", "2026-10-11T00:00:00Z", &unmet("test_interface", "M")),
        ];
        let r = summarise(&rows, 7, now());
        let g = group(&r, "test_interface");
        assert_eq!(g.findings.len(), 2);
        assert_eq!(g.tasks.len(), 2);
        assert!(g.suggest_check);
    }

    #[test]
    fn retro_suggests_a_check_for_one_task_twice() {
        let rows = vec![
            row("T1", "2026-10-10T00:00:00Z", &unmet("error_path", "S")),
            row("T1", "2026-10-11T00:00:00Z", &unmet("error_path", "S")),
        ];
        let r = summarise(&rows, 7, now());
        let g = group(&r, "error_path");
        assert_eq!(g.findings.len(), 2);
        assert_eq!(g.tasks.len(), 1);
        assert!(g.suggest_check);
    }

    #[test]
    fn retro_does_not_suggest_a_check_for_a_category_seen_once() {
        let r = summarise(&[row("T1", "2026-10-10T00:00:00Z", &unmet("error_path", "S"))], 7, now());
        let g = group(&r, "error_path");
        assert_eq!(g.findings.len(), 1);
        assert!(!g.suggest_check);
    }

    #[test]
    fn retro_never_suggests_a_check_for_other_or_unclassified() {
        let no_label = r#"{"criteria":[{"id":"AC1","verdict":"unmet","evidence":[],"note":"n"}],"extra_findings":[]}"#;
        let rows = vec![
            row("T1", "2026-10-10T00:00:00Z", &unmet("other", "S")),
            row("T2", "2026-10-11T00:00:00Z", &unmet("other", "S")),
            row("T1", "2026-10-10T00:00:00Z", no_label),
            row("T2", "2026-10-11T00:00:00Z", no_label),
        ];
        let r = summarise(&rows, 7, now());
        assert_eq!(group(&r, "other").tasks.len(), 2);
        assert!(!group(&r, "other").suggest_check);
        let u = group(&r, UNCLASSIFIED);
        assert_eq!(u.tasks.len(), 2);
        assert!(!u.suggest_check);
        assert_eq!(u.sizes, Sizes { s: 0, m: 0, l: 0, none: 2 });
        assert_eq!(u.findings[0].fix_size, None);
    }

    #[test]
    fn retro_counts_every_size_and_orders_groups_by_count_then_name() {
        let rows = vec![
            row("T1", "2026-10-10T00:00:00Z", &unmet("platform", "S")),
            row("T2", "2026-10-10T00:00:00Z", &unmet("platform", "L")),
            row("T3", "2026-10-10T00:00:00Z", &unmet("docs", "M")),
            row("T4", "2026-10-10T00:00:00Z", r#"{"criteria":[{"id":"AC1","verdict":"unmet","note":"n","category":"concurrency"}]}"#),
        ];
        let r = summarise(&rows, 7, now());
        let names: Vec<&str> = r.categories.iter().map(|g| g.category.as_str()).collect();
        assert_eq!(names, vec!["platform", "concurrency", "docs"]);
        assert_eq!(r.sizes, Sizes { s: 1, m: 1, l: 1, none: 1 });
        assert_eq!(r.reviews, 4);
    }
}
