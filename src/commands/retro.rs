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
use crate::db::{reports, state};
use crate::output;
use crate::review_json::{category_blocks, CATEGORIES, FIX_SIZES};
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

impl Group {
    /// REV-2 (AC6): whether this category blocks, by REV-1's definition
    /// (`review_json::category_blocks`); `unclassified` blocks.
    pub fn blocking(&self) -> bool {
        category_blocks(Some(&self.category))
    }
}

impl Retro {
    pub fn finding_count(&self) -> usize {
        self.categories.iter().map(|g| g.findings.len()).sum()
    }

    /// REV-2 (AC6): the findings in blocking categories.
    pub fn blocking_count(&self) -> usize {
        self.categories.iter().filter(|g| g.blocking()).map(|g| g.findings.len()).sum()
    }

    /// REV-2 (AC6): the findings in advisory categories.
    pub fn advisory_count(&self) -> usize {
        self.finding_count() - self.blocking_count()
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
    // RFIX-1: an author check's record repeats the edit review it confirms,
    // which is counted already; it is one review with it, not two.
    let reviews = in_window.iter().filter(|(_, r)| author_check_copied(r).is_none()).count();
    Retro { days, since, reviews, sizes, categories: groups }
}

/// RFIX-1: how many leading extra findings of an author check's record were
/// copied from the edit review it confirms (`findings.author_check`), or
/// `None` for any other review.
fn author_check_copied(review: &ReviewRow) -> Option<usize> {
    let stored: Value = serde_json::from_str(&review.findings).ok()?;
    let marker = stored.get("author_check")?;
    Some(marker.get("copied_extra_findings").and_then(Value::as_u64).unwrap_or(0) as usize)
}

/// A review's findings with the group each belongs to: every unmet criterion
/// and every extra finding. Stored findings are already normalised by
/// `review_json::parse_review`; anything else is unclassified or unsized. An
/// author check's record gives only the extra findings it added: its
/// criteria and its first extras are the edit review's, counted there.
fn findings_of(review: &ReviewRow) -> Vec<(String, Item)> {
    let copied = author_check_copied(review);
    let stored: Value = serde_json::from_str(&review.findings).unwrap_or(Value::Null);
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let category = |v: &Value| {
        text(v, "category")
            .filter(|c| CATEGORIES.iter().any(|cat| cat.name == c))
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
    if copied.is_none() {
        for c in list("criteria") {
            if text(&c, "verdict").as_deref().map(str::trim) == Some("unmet") {
                out.push((category(&c), item(&c, "criterion", text(&c, "id"), None)));
            }
        }
    }
    for e in list("extra_findings").into_iter().skip(copied.unwrap_or(0)) {
        out.push((category(&e), item(&e, "extra", None, Some(text(&e, "severity").unwrap_or_default()))));
    }
    out
}

pub fn run(paths: &Paths, days: u32, json_output: bool) -> Result<i32> {
    let (rows, providers, attempts) = load(paths)?;
    let now = OffsetDateTime::now_utc();
    let retro = summarise(&rows, days, now);
    let split = provider_split(&rows, &providers, days, now);
    let fbb = fixed_by_behaviour(&rows, &attempts, &providers, days, now);
    let mut v = to_json(&retro);
    if let Some(o) = v.as_object_mut() {
        o.insert("provider_split".to_string(), split.to_json());
        o.insert("fixed_by_behaviour".to_string(), fbb.to_json());
    }
    let mut text = human(&retro);
    if retro.reviews > 0 {
        text.push('\n');
        text.push_str(&split.human());
        text.push('\n');
        text.push_str(&fbb.human());
    }
    output::emit(json_output, &v, &text);
    Ok(0)
}

/// MEAS-0: which side of the same-/cross-provider split a review falls on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Same,
    Cross,
    Unknown,
}

impl Side {
    const ALL: [Side; 3] = [Side::Same, Side::Cross, Side::Unknown];

    fn key(self) -> &'static str {
        match self {
            Side::Same => "same_provider",
            Side::Cross => "cross_provider",
            Side::Unknown => "unknown",
        }
    }

    /// Same or cross only when both runs exist and both stored a non-blank
    /// provider; an external review (no reviewer run) is unknown.
    fn of(p: Option<&state::ReviewProviders>) -> Side {
        let known = |s: &Option<String>| s.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
        match p.map(|p| (known(&p.implementer_provider), known(&p.reviewer_provider))) {
            Some((Some(i), Some(r))) if i == r => Side::Same,
            Some((Some(_), Some(_))) => Side::Cross,
            _ => Side::Unknown,
        }
    }
}

/// MEAS-0: one side's counts, by the same rules as the top level.
#[derive(Debug, Clone, Default)]
struct SideCounts {
    reviews: usize,
    pass: usize,
    reject: usize,
    unverified: usize,
    findings: usize,
    categories: Vec<(String, usize)>,
}

impl SideCounts {
    fn to_json(&self) -> Value {
        json!({
            "reviews": self.reviews,
            "verdicts": {"PASS": self.pass, "REJECT": self.reject, "UNVERIFIED": self.unverified},
            "findings": self.findings,
            "categories": self.categories.iter()
                .map(|(c, n)| json!({"category": c, "count": n}))
                .collect::<Vec<_>>(),
        })
    }
}

struct ProviderSplit([SideCounts; 3]);

impl ProviderSplit {
    fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        for (side, counts) in Side::ALL.iter().zip(&self.0) {
            o.insert(side.key().to_string(), counts.to_json());
        }
        Value::Object(o)
    }

    fn human(&self) -> String {
        let mut lines = vec!["by provider (reviewer vs implementer):".to_string()];
        for (side, c) in Side::ALL.iter().zip(&self.0) {
            lines.push(format!(
                "  {}: {} reviews (PASS {} REJECT {} UNVERIFIED {}), {} findings",
                side.key(),
                c.reviews,
                c.pass,
                c.reject,
                c.unverified,
                c.findings
            ));
        }
        lines.join("\n")
    }
}

/// MEAS-0: the reviews `summarise` counts, split by whether the reviewer run
/// and the implementer run stored the same provider. The window, the skipped
/// unreadable `created_at`, the author-check rule and the categories are
/// `summarise`'s, so each side's counts add up to the top level's.
fn provider_split(
    rows: &[ReviewRow],
    providers: &Providers,
    days: u32,
    now: OffsetDateTime,
) -> ProviderSplit {
    let since = now - Duration::days(i64::from(days));
    let mut sides: [SideCounts; 3] = Default::default();
    for review in rows {
        match OffsetDateTime::parse(&review.created_at, &Rfc3339) {
            Ok(t) if t >= since => {}
            _ => continue,
        }
        let p = providers.get(&(review.repo.clone(), review.review_id.clone()));
        let at = Side::ALL.iter().position(|s| *s == Side::of(p)).unwrap_or(2);
        let side = &mut sides[at];
        if author_check_copied(review).is_none() {
            side.reviews += 1;
            match p.map(|p| p.verdict.as_str()) {
                Some("PASS") => side.pass += 1,
                Some("REJECT") => side.reject += 1,
                Some("UNVERIFIED") => side.unverified += 1,
                _ => {}
            }
        }
        for (category, _) in findings_of(review) {
            side.findings += 1;
            match side.categories.iter_mut().find(|(c, _)| *c == category) {
                Some((_, n)) => *n += 1,
                None => side.categories.push((category, 1)),
            }
        }
    }
    for side in &mut sides {
        side.categories.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    ProviderSplit(sides)
}

type Providers = std::collections::HashMap<(String, String), state::ReviewProviders>;

/// MEAS-1: one review with its task and its target run's attempt; a repo's
/// rows in age order.
#[derive(Debug, Clone)]
pub struct AttemptRow {
    pub repo: String,
    pub review_id: String,
    pub task_id: String,
    pub attempt: Option<i64>,
    pub verdict: String,
}

/// MEAS-1: the four answers' counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Marked {
    yes: usize,
    no: usize,
    not_fixed: usize,
    unmarked: usize,
}

impl Marked {
    fn add(&mut self, other: Marked) {
        self.yes += other.yes;
        self.no += other.no;
        self.not_fixed += other.not_fixed;
        self.unmarked += other.unmarked;
    }

    fn total(self) -> usize {
        self.yes + self.no + self.not_fixed + self.unmarked
    }

    fn to_json(self) -> Value {
        json!({"yes": self.yes, "no": self.no, "not_fixed": self.not_fixed, "unmarked": self.unmarked})
    }

    fn human(self) -> String {
        format!("yes {} no {} not_fixed {} unmarked {}", self.yes, self.no, self.not_fixed, self.unmarked)
    }

    /// A stored check's `counts`.
    fn of_counts(c: &Value) -> Marked {
        let n = |k: &str| c[k].as_u64().unwrap_or(0) as usize;
        Marked { yes: n("yes"), no: n("no"), not_fixed: n("not_fixed"), unmarked: n("unmarked") }
    }
}

/// MEAS-1 (AC7): `fixed_by_behaviour`, the marks on previous rejections.
#[derive(Debug, Clone, Default)]
pub struct FixedByBehaviour {
    reviews: usize,
    not_recorded: usize,
    total: Marked,
    /// By the rejected finding's category, in output order.
    categories: Vec<(String, Marked)>,
    /// `Side::ALL` order.
    sides: [Marked; 3],
}

impl FixedByBehaviour {
    pub fn to_json(&self) -> Value {
        let mut sides = serde_json::Map::new();
        for (side, m) in Side::ALL.iter().zip(&self.sides) {
            sides.insert(side.key().to_string(), m.to_json());
        }
        json!({
            "reviews": self.reviews,
            "not_recorded": self.not_recorded,
            "findings": self.total.total(),
            "yes": self.total.yes,
            "no": self.total.no,
            "not_fixed": self.total.not_fixed,
            "unmarked": self.total.unmarked,
            "categories": self.categories.iter().map(|(c, m)| json!({
                "category": c,
                "blocking": category_blocks(Some(c)),
                "count": m.total(),
                "yes": m.yes,
                "no": m.no,
                "not_fixed": m.not_fixed,
                "unmarked": m.unmarked,
            })).collect::<Vec<_>>(),
            "by_rejecting_review": Value::Object(sides),
        })
    }

    pub fn human(&self) -> String {
        let mut lines = vec![
            format!(
                "fixed by behaviour (marks on the previous rejection): {} reviews, {} not recorded (before MEAS-1)",
                self.reviews, self.not_recorded
            ),
            format!("  {}", self.total.human()),
        ];
        for (c, m) in &self.categories {
            let class = if category_blocks(Some(c)) { "blocking" } else { "advisory" };
            lines.push(format!("  {c}: {}  ({class})", m.human()));
        }
        for (side, m) in Side::ALL.iter().zip(&self.sides) {
            lines.push(format!("  rejected by {}: {}", side.key(), m.human()));
        }
        lines.join("\n")
    }
}

/// MEAS-1 (AC7): the marks of the reviews `summarise` counts (same window,
/// unreadable `created_at` skipped, an author check's record left out so an
/// edit-then-confirm chain counts once). A review counts when its record
/// stores a check with status `checked`; a code review that had a REJECT to
/// mark by `prior_rejection`'s rule but stores no check (written before
/// MEAS-1) is `not_recorded` and counts in no bucket. `attempts` holds each
/// repo's reviews in age order; `providers` gives the rejecting review's
/// side (unknown when it is not found).
pub fn fixed_by_behaviour(
    rows: &[ReviewRow],
    attempts: &[AttemptRow],
    providers: &Providers,
    days: u32,
    now: OffsetDateTime,
) -> FixedByBehaviour {
    let since = now - Duration::days(i64::from(days));
    let mut out = FixedByBehaviour::default();
    for review in rows {
        match OffsetDateTime::parse(&review.created_at, &Rfc3339) {
            Ok(t) if t >= since => {}
            _ => continue,
        }
        if author_check_copied(review).is_some() {
            continue;
        }
        let stored: Value = serde_json::from_str(&review.findings).unwrap_or(Value::Null);
        if stored.get("sources").and_then(Value::as_array).is_some_and(|s| !s.is_empty()) {
            continue;
        }
        let check = stored.get(crate::prior_rejection::CHECK_KEY).filter(|c| c.is_object());
        match check {
            Some(c) if c["status"] == "checked" => {
                out.reviews += 1;
                out.total.add(Marked::of_counts(&c["counts"]));
                for e in c["entries"].as_array().into_iter().flatten() {
                    let name = e["category"]
                        .as_str()
                        .filter(|n| CATEGORIES.iter().any(|cat| cat.name == *n))
                        .unwrap_or(UNCLASSIFIED)
                        .to_string();
                    let mut one = Marked::default();
                    match (e["marked"].as_bool(), e["fixed_by_behaviour"].as_str()) {
                        (Some(true), Some("yes")) => one.yes = 1,
                        (Some(true), Some("no")) => one.no = 1,
                        (Some(true), Some("not_fixed")) => one.not_fixed = 1,
                        _ => one.unmarked = 1,
                    }
                    match out.categories.iter_mut().find(|(c, _)| *c == name) {
                        Some((_, m)) => m.add(one),
                        None => out.categories.push((name, one)),
                    }
                }
                let rejecting = c["review_id"].as_str().unwrap_or_default().to_string();
                let side = Side::of(providers.get(&(review.repo.clone(), rejecting)));
                let at = Side::ALL.iter().position(|s| *s == side).unwrap_or(2);
                out.sides[at].add(Marked::of_counts(&c["counts"]));
            }
            Some(_) => {}
            None => {
                if had_reject_to_mark(review, attempts) {
                    out.not_recorded += 1;
                }
            }
        }
    }
    out.categories.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(&b.0)));
    out
}

/// MEAS-1: whether `review` had a REJECT to mark by `prior_rejection`'s
/// rule: the newest earlier record of its task with a lower attempt.
fn had_reject_to_mark(review: &ReviewRow, attempts: &[AttemptRow]) -> bool {
    let Some(at) = attempts.iter().position(|a| a.repo == review.repo && a.review_id == review.review_id) else {
        return false;
    };
    let me = &attempts[at];
    let Some(attempt) = me.attempt else { return false };
    let earlier: Vec<&AttemptRow> = attempts[..at]
        .iter()
        .filter(|a| a.repo == me.repo && a.task_id == me.task_id)
        .collect();
    let list: Vec<Option<i64>> = earlier.iter().map(|a| a.attempt).collect();
    crate::prior_rejection::previous_index(&list, attempt)
        .is_some_and(|i| earlier[i].verdict.trim() == crate::review_json::Verdict::Reject.as_str())
}

/// Every review of every registered repo whose state.db exists, (MEAS-0)
/// each review's verdict and run providers keyed by (repo root, review id),
/// and (MEAS-1) each review's target attempt. No global.db means nothing has
/// been reviewed on this machine yet.
fn load(paths: &Paths) -> Result<(Vec<ReviewRow>, Providers, Vec<AttemptRow>)> {
    if !paths.global_db().is_file() {
        return Ok((Vec::new(), Providers::new(), Vec::new()));
    }
    let global = usage::global_read_only(paths)?;
    let mut repos = Vec::new();
    for (repo_id, root) in reports::repo_roots(&global)? {
        let path = paths.state_db(&repo_id);
        if path.is_file() {
            repos.push((root, usage::read_only(&path)?));
        }
    }
    let mut out = Vec::new();
    let mut providers = Providers::new();
    let mut attempts = Vec::new();
    for (root, conn) in &repos {
        out.extend(load_reviews(root, conn)?);
        for p in state::list_review_providers(conn)? {
            providers.insert((root.clone(), p.review_id.clone()), p);
        }
        attempts.extend(reports::review_attempts(conn)?.into_iter().map(|a| AttemptRow {
            repo: root.clone(),
            review_id: a.review_id,
            task_id: a.task_id,
            attempt: a.attempt,
            verdict: a.verdict,
        }));
    }
    Ok((out, providers, attempts))
}

fn load_reviews(repo: &str, conn: &Connection) -> Result<Vec<ReviewRow>> {
    Ok(reports::review_rows_with_task(conn)?
        .into_iter()
        .map(|r| ReviewRow {
            repo: repo.to_string(),
            review_id: r.review_id,
            created_at: r.created_at,
            findings: r.findings,
            task_id: r.task_id,
            title: r.title,
        })
        .collect())
}

fn to_json(r: &Retro) -> Value {
    let categories: Vec<Value> = r
        .categories
        .iter()
        .map(|g| {
            json!({
                "category": g.category,
                "blocking": g.blocking(),
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
        "blocking_findings": r.blocking_count(),
        "advisory_findings": r.advisory_count(),
        "sizes": r.sizes.to_json(),
        "categories": categories,
    })
}

fn human(r: &Retro) -> String {
    if r.categories.is_empty() {
        return format!("retro: no review findings in the last {} days", r.days);
    }
    let mut lines = vec![format!(
        "retro: last {} days (since {}): {} reviews, {} findings, {} blocking, {} advisory ({})",
        r.days,
        r.since.format(&Rfc3339).unwrap_or_default(),
        r.reviews,
        r.finding_count(),
        r.blocking_count(),
        r.advisory_count(),
        r.sizes.human()
    )];
    for g in &r.categories {
        lines.push(format!(
            "{}  {} findings in {} tasks  ({})  ({})",
            g.category,
            g.findings.len(),
            g.tasks.len(),
            g.sizes.human(),
            if g.blocking() { "blocking" } else { "advisory" }
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

    /// REV-2 (AC6): each group says whether it blocks, by REV-1's
    /// definition, and the totals split into blocking and advisory.
    #[test]
    fn retro_splits_findings_into_blocking_and_advisory() {
        let no_label = r#"{"criteria":[{"id":"AC1","verdict":"unmet","evidence":[],"note":"n"}],"extra_findings":[]}"#;
        let rows = vec![
            row("T1", "2026-10-10T00:00:00Z", &unmet("docs", "S")),
            row("T2", "2026-10-10T00:00:00Z", &unmet("test_missing", "S")),
            row("T3", "2026-10-10T00:00:00Z", &unmet("test_incomplete", "S")),
            row("T4", "2026-10-10T00:00:00Z", no_label),
        ];
        let r = summarise(&rows, 7, now());
        assert!(!group(&r, "docs").blocking());
        assert!(!group(&r, "test_incomplete").blocking());
        assert!(group(&r, "test_missing").blocking());
        assert!(group(&r, UNCLASSIFIED).blocking());
        assert_eq!(r.blocking_count(), 2);
        assert_eq!(r.advisory_count(), 2);
        let v = to_json(&r);
        assert_eq!(v["blocking_findings"], 2);
        assert_eq!(v["advisory_findings"], 2);
        let text = human(&r);
        assert!(text.lines().next().unwrap().contains("4 findings, 2 blocking, 2 advisory"), "{text}");
        assert!(text.lines().any(|l| l.starts_with("docs ") && l.ends_with("(advisory)")), "{text}");
        assert!(text.lines().any(|l| l.starts_with("test_missing ") && l.ends_with("(blocking)")), "{text}");
    }

    /// MEAS-1 (AC7): a rework record whose previous attempt was rejected but
    /// that has no stored check (written before MEAS-1) counts in
    /// `not_recorded` and in no bucket; a record with a check counts its
    /// marks by category and by the rejecting review's provider relation; a
    /// rework after a PASS has nothing to mark.
    #[test]
    fn retro_counts_a_rework_record_without_a_check_as_not_recorded_only() {
        let reject = |task: &str, at: &str| row(task, at, &unmet("docs", "S"));
        let t1_reject = reject("T1", "2026-10-10T00:00:00Z");
        let t1_old = reject("T1", "2026-10-11T00:00:00Z");
        let t2_reject = reject("T2", "2026-10-10T00:00:00Z");
        let check = serde_json::json!({
            "status": "checked", "review_id": t2_reject.review_id, "previous_verdict": "REJECT",
            "expected": ["AC1"],
            "entries": [{"ref": "AC1", "category": "docs", "fix_size": "S", "fixed_by_behaviour": "yes",
                         "marked": true, "reason": null, "detail": null, "index": 0}],
            "extra_entries": [], "array_error": null,
            "counts": {"yes": 1, "no": 0, "not_fixed": 0, "unmarked": 0}
        });
        let mut marked: Value = serde_json::from_str(&unmet("spec_mismatch", "M")).unwrap();
        marked["prior_rejection_check"] = check;
        let t2_marked = row("T2", "2026-10-11T00:00:00Z", &marked.to_string());
        let t3_pass = row("T3", "2026-10-10T00:00:00Z", r#"{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]}],"extra_findings":[]}"#);
        let t3_next = reject("T3", "2026-10-11T00:00:00Z");
        let rows = vec![t1_reject.clone(), t1_old.clone(), t2_reject.clone(), t2_marked.clone(), t3_pass.clone(), t3_next.clone()];
        let attempt = |r: &ReviewRow, n: i64, verdict: &str| AttemptRow {
            repo: r.repo.clone(),
            review_id: r.review_id.clone(),
            task_id: r.task_id.clone(),
            attempt: Some(n),
            verdict: verdict.to_string(),
        };
        let attempts = vec![
            attempt(&t1_reject, 1, "REJECT"),
            attempt(&t1_old, 2, "REJECT"),
            attempt(&t2_reject, 1, "REJECT"),
            attempt(&t2_marked, 2, "REJECT"),
            attempt(&t3_pass, 1, "PASS"),
            attempt(&t3_next, 2, "REJECT"),
        ];
        let mut providers = Providers::new();
        providers.insert(
            (t2_reject.repo.clone(), t2_reject.review_id.clone()),
            state::ReviewProviders {
                review_id: t2_reject.review_id.clone(),
                verdict: "REJECT".to_string(),
                implementer_provider: Some("anthropic".to_string()),
                reviewer_provider: Some("anthropic".to_string()),
            },
        );
        let fbb = fixed_by_behaviour(&rows, &attempts, &providers, 7, now());
        let v = fbb.to_json();
        assert_eq!(v["reviews"], 1, "{v}");
        assert_eq!(v["not_recorded"], 1, "T1's rework only; T3's follows a PASS: {v}");
        assert_eq!(v["findings"], 1, "{v}");
        assert_eq!((v["yes"].clone(), v["no"].clone(), v["not_fixed"].clone(), v["unmarked"].clone()), (json!(1), json!(0), json!(0), json!(0)), "{v}");
        assert_eq!(
            v["categories"],
            json!([{"category": "docs", "blocking": false, "count": 1, "yes": 1, "no": 0, "not_fixed": 0, "unmarked": 0}]),
            "{v}"
        );
        let zero = json!({"yes": 0, "no": 0, "not_fixed": 0, "unmarked": 0});
        assert_eq!(v["by_rejecting_review"]["same_provider"], json!({"yes": 1, "no": 0, "not_fixed": 0, "unmarked": 0}), "{v}");
        assert_eq!(v["by_rejecting_review"]["cross_provider"], zero, "{v}");
        assert_eq!(v["by_rejecting_review"]["unknown"], zero, "{v}");

        // Nothing at all: every key, zeros and [].
        let empty = fixed_by_behaviour(&[], &[], &Providers::new(), 7, now()).to_json();
        assert_eq!(
            empty,
            json!({"reviews": 0, "not_recorded": 0, "findings": 0, "yes": 0, "no": 0, "not_fixed": 0, "unmarked": 0,
                   "categories": [], "by_rejecting_review": {"same_provider": zero, "cross_provider": zero, "unknown": zero}})
        );
    }
}
