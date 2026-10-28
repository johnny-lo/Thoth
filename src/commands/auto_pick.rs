//! RJ-1: Thoth no longer picks the profile, model or effort for `dispatch`,
//! `review start` and `spec review start`; the commander gives them, and
//! this module checks and records what was given.
//!
//! Everything here only reads, except `mark_recovered` and
//! `mark_logged_in_again`, which the callers run once the run row exists. A
//! refusal therefore leaves nothing behind.

use crate::adapter::effort::{self, SupportedLevels};
use crate::db::global::{self, Profile};
use crate::db::state::Run;
use crate::ids;
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// The value of `--profile`, `--model` or `--effort` that used to ask Thoth
/// to pick. Refused now; still why `profile add auto` is refused.
pub(crate) const AUTO: &str = "auto";

/// Whether a `--model` or `--effort` value is `auto` (trimmed, any case).
/// `--profile` is compared exactly, like every profile id.
pub(crate) fn is_auto(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case(AUTO)
}

/// RJ-1 (AC1): refuses `--profile auto`, `--model auto` and `--effort auto`,
/// the first check of the three launch commands. `role` is the role
/// `thoth recommend` takes (`implementer` or `reviewer`).
pub(crate) fn refuse_auto(
    role: &str,
    task: &str,
    profile: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<()> {
    let mut flags = Vec::new();
    if profile == AUTO {
        flags.push("--profile");
    }
    if model.is_some_and(is_auto) {
        flags.push("--model");
    }
    if effort.is_some_and(is_auto) {
        flags.push("--effort");
    }
    if flags.is_empty() {
        return Ok(());
    }
    let named: Vec<String> = flags.iter().map(|f| format!("{f} auto")).collect();
    bail!(
        "{} was removed: Thoth no longer picks the profile, model or effort for you. Give {} a \
         value yourself; thoth recommend --role {role} --task {task} shows each profile's quota",
        named.join(", "),
        flags.join(", ")
    )
}

/// How a launch treats a left-out `--model` or `--effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retry {
    /// `dispatch` and `review start`: a reroute in the same attempt keeps the
    /// previous run's model and effort.
    KeepsPrevious,
    /// `spec review start`: there is no attempt to continue, so every launch
    /// gives its own.
    EveryTime,
}

/// What the commander gave, and the run a left-out value is kept from.
pub(crate) struct Request<'a> {
    /// The resolved task id.
    pub(crate) task_id: &'a str,
    /// The role `thoth recommend` takes: `implementer` or `reviewer`.
    pub(crate) role: &'a str,
    pub(crate) model: Option<&'a str>,
    /// `--effort`, already checked by `effort::parse_level`.
    pub(crate) effort: Option<String>,
    /// DT-2: the newest run of this role and attempt (`None` for a spec
    /// review start).
    pub(crate) previous: Option<&'a Run>,
    pub(crate) retry: Retry,
}

/// The model and effort level a launch uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) model: String,
    /// The level asked for: `--effort`, or the previous run's. `None` only
    /// for a model without an effort setting.
    pub(crate) effort: Option<String>,
    pub(crate) inherited_model: bool,
    pub(crate) inherited_effort: bool,
    /// The run a kept value came from.
    pub(crate) previous_run: Option<String>,
}

impl Choice {
    /// AC2 (5): the warning line saying what was kept from which run.
    pub(crate) fn warning(&self) -> Option<String> {
        let run = self.previous_run.as_deref()?;
        let kept = match (self.inherited_model, self.inherited_effort) {
            (true, true) => format!(
                "model {} and effort {}",
                self.model,
                self.effort.as_deref().unwrap_or("none")
            ),
            (true, false) => format!("model {}", self.model),
            (false, true) => format!("effort {}", self.effort.as_deref().unwrap_or("none")),
            (false, false) => return None,
        };
        Some(format!("{kept} kept from run {run} (same attempt)"))
    }
}

/// Whether `model` on `profile` can run without an effort level: the harness
/// has no effort setting, the model has none (haiku), or (AGY-1) the agy
/// model name carries its level itself (`gemini-3.8-flash-high`), in which
/// case `launch_settings` records that level through `effort::resolve`.
fn effort_optional(profile: &Profile, model: &str) -> bool {
    effort::carried_level(&profile.harness, model).is_some()
        || matches!(
            effort::supported_levels(&profile.harness, model, Path::new(&profile.config_dir)),
            SupportedLevels::NoEffort | SupportedLevels::NotApplicable
        )
}

/// RJ-1 (AC2): the model, then `check_model` on it, then the effort; refuses
/// a left-out `--model` or `--effort` that cannot be kept from the previous
/// run. Runs after the profile lookup; reads only.
pub(crate) fn choose(
    gconn: &Connection,
    req: &Request<'_>,
    profile: &Profile,
    check_model: impl Fn(&str) -> Result<()>,
) -> Result<Choice> {
    let previous = req.previous.filter(|_| req.retry == Retry::KeepsPrevious);
    let (model, inherited_model) = match (req.model, previous) {
        (Some(m), _) => (Some(m.to_string()), false),
        (None, Some(prev)) => {
            // `check_model` cannot tell a codex or agy model from a Claude
            // one in every direction, so a model kept across harnesses is
            // refused here.
            let prev_harness = global::get_profile(gconn, &prev.account)?.map(|p| p.harness);
            if prev_harness.as_deref() != Some(profile.harness.as_str()) {
                bail!(
                    "task {}: --model was left out, but the previous run {}'s model {} is for \
                     another harness (it ran on profile {}, {}; profile {} is {}); pass --model \
                     for this profile",
                    req.task_id,
                    prev.run_id,
                    prev.model,
                    prev.account,
                    prev_harness.as_deref().unwrap_or("no longer registered"),
                    profile.profile_id,
                    profile.harness
                );
            }
            (Some(prev.model.clone()), true)
        }
        (None, None) => (None, false),
    };
    if let Some(m) = &model {
        check_model(m)?;
    }
    let kept_effort = previous
        .and_then(|prev| effort::parse_launch(&prev.spec))
        .and_then(|l| l.effort_requested);
    let (effort, inherited_effort) = match (&req.effort, kept_effort) {
        (Some(level), _) => (Some(level.clone()), false),
        (None, Some(level)) => (Some(level), true),
        (None, None) => (None, false),
    };
    let mut missing = Vec::new();
    if model.is_none() {
        missing.push("--model");
    }
    let needs_effort = model.as_deref().is_none_or(|m| !effort_optional(profile, m));
    if effort.is_none() && needs_effort {
        missing.push("--effort");
    }
    if !missing.is_empty() {
        let when = match req.retry {
            Retry::KeepsPrevious => {
                "Give them on the first launch; a reroute in the same attempt keeps the previous \
                 run's"
            }
            Retry::EveryTime => "spec review start needs them every time",
        };
        bail!(
            "task {}: {} {} missing: Thoth no longer picks the model or effort for you. {when}; \
             --effort can be left out only for a model without an effort setting (haiku) or an \
             agy model whose name ends in its level (gemini-3.8-flash-high). thoth recommend \
             --role {} --task {} shows each profile's quota",
            req.task_id,
            missing.join(" and "),
            if missing.len() == 1 { "is" } else { "are" },
            req.role,
            req.task_id
        );
    }
    let Some(model) = model else {
        unreachable!("a left-out model is refused above")
    };
    let previous_run = (inherited_model || inherited_effort)
        .then(|| previous.map(|p| p.run_id.clone()))
        .flatten();
    Ok(Choice { model, effort, inherited_model, inherited_effort, previous_run })
}

/// RJ-1 (AC3 (2)): with `require-reason` on, every launch needs a reason.
pub(crate) fn require_reason_gate(
    task_id: &str,
    command: &str,
    reason: Option<&str>,
    require_reason: bool,
) -> Result<()> {
    if require_reason && reason.is_none() {
        bail!(
            "task {task_id}: require-reason is on, so every {command} needs --reason TEXT saying \
             why this profile, model and effort were chosen (MCP: the reason parameter); a human \
             can run thoth config set require-reason off"
        );
    }
    Ok(())
}

/// The launch's `effort::LaunchSettings`: the level asked for, resolved for
/// the profile's harness and model. Every new run's spec carries it.
pub(crate) fn launch_settings(choice: &Choice, profile: &Profile) -> effort::LaunchSettings {
    let r = effort::resolve(
        &profile.harness,
        &choice.model,
        Path::new(&profile.config_dir),
        choice.effort.as_deref(),
    );
    effort::LaunchSettings { effort_requested: r.requested, effort: r.sent, effort_note: r.note }
}

/// `launch_choice.source`.
pub(crate) const SOURCE_DISPATCH: &str = "dispatch";
pub(crate) const SOURCE_REVIEW_START: &str = "review_start";
pub(crate) const SOURCE_SPEC_REVIEW_START: &str = "spec_review_start";
/// SR-3: the author's check of a reviewer's edits.
pub(crate) const SOURCE_SPEC_REVIEW_CONFIRM: &str = "spec_review_confirm";

/// RJ-1 (AC3): one launch as it was chosen, for the `launch_choice` event.
pub(crate) struct LaunchChoice<'a> {
    pub(crate) role: &'static str,
    pub(crate) source: &'static str,
    /// The task's spec, for `task_type` and `difficulty` as written.
    pub(crate) task_spec: &'a str,
    pub(crate) risk: &'a str,
    pub(crate) profile: &'a str,
    pub(crate) choice: &'a Choice,
    pub(crate) launch: &'a effort::LaunchSettings,
    /// AC2's previous run of the same attempt, kept from or not.
    pub(crate) previous: Option<&'a Run>,
    /// Already trimmed and not empty.
    pub(crate) reason: Option<&'a str>,
    pub(crate) require_reason: bool,
}

impl LaunchChoice<'_> {
    /// The `launch_choice` event's detail.
    pub(crate) fn detail(&self) -> serde_json::Value {
        let (task_type, difficulty) = crate::spec::labels(self.task_spec);
        serde_json::json!({
            "role": self.role,
            "source": self.source,
            "task_type": task_type,
            "difficulty": difficulty,
            "risk": self.risk,
            "actual": {
                "profile": self.profile,
                "model": self.choice.model,
                "effort_requested": self.launch.effort_requested,
                "effort": self.launch.effort,
            },
            "inherited": {
                "model": self.choice.inherited_model,
                "effort": self.choice.inherited_effort,
            },
            "previous_run": self.previous.map(|p| p.run_id.clone()),
            "reason": self.reason,
            "require_reason": self.require_reason,
        })
    }
}

/// The trimmed `--reason`: `None` when it was not given; an error when it is
/// empty after trimming.
pub(crate) fn clean_reason(reason: Option<&str>) -> Result<Option<String>> {
    match reason.map(str::trim) {
        None => Ok(None),
        Some("") => bail!("--reason must not be empty"),
        Some(text) => Ok(Some(text.to_string())),
    }
}

/// AC4: `profiles.status: rate_limited -> active` plus a `window_reset` quota
/// event with `source: auto`, in one transaction. Called only once the run
/// row exists. Returns whether it changed anything (a concurrent
/// `profile check` may have cleared the flag already). FC-1: a named profile
/// recovers too.
pub(crate) fn mark_recovered(
    gconn: &Connection,
    profile_id: &str,
    task_id: &str,
    run_id: &str,
) -> Result<bool> {
    let tx = crate::db::write_tx(gconn)?;
    let parked = global::get_profile(&tx, profile_id)?.is_some_and(|p| p.status == "rate_limited");
    if parked {
        global::set_profile_status(&tx, profile_id, "active")?;
        global::insert_quota_event(
            &tx,
            &ids::now_rfc3339(),
            profile_id,
            "window_reset",
            &serde_json::json!({
                "source": "auto",
                "task_id": task_id,
                "run_id": run_id,
            }),
        )?;
    }
    tx.commit()?;
    Ok(parked)
}

/// FC-1: a named dispatch or review start whose auth check passed on a
/// `logged_out` profile: `profiles.status: logged_out -> active` plus a
/// `login_restored` quota event, in one transaction, once the run row exists.
/// `source` is `dispatch` or `review_start`. Returns whether it changed
/// anything.
pub(crate) fn mark_logged_in_again(
    gconn: &Connection,
    profile_id: &str,
    task_id: &str,
    run_id: &str,
    source: &str,
) -> Result<bool> {
    let tx = crate::db::write_tx(gconn)?;
    let logged_out = global::get_profile(&tx, profile_id)?.is_some_and(|p| p.status == "logged_out");
    if logged_out {
        global::set_profile_status(&tx, profile_id, "active")?;
        global::insert_quota_event(
            &tx,
            &ids::now_rfc3339(),
            profile_id,
            "login_restored",
            &serde_json::json!({"source": source, "task_id": task_id, "run_id": run_id}),
        )?;
    }
    tx.commit()?;
    Ok(logged_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_matched_trimmed_and_in_any_case() {
        assert!(is_auto("auto"));
        assert!(is_auto(" AUTO "));
        assert!(!is_auto("automatic"));
        assert!(!is_auto("high"));
    }

    #[test]
    fn refuse_auto_names_every_auto_flag_and_points_to_recommend() {
        assert!(refuse_auto("implementer", "T1", "claude-a", Some("sonnet"), Some("high")).is_ok());
        // a profile id is compared exactly
        assert!(refuse_auto("implementer", "T1", "AUTO", None, None).is_ok());
        let e = refuse_auto("reviewer", "T1", "auto", Some(" Auto "), Some("AUTO"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("--profile auto, --model auto, --effort auto was removed"), "{e}");
        assert!(e.contains("thoth recommend --role reviewer --task T1"), "{e}");
    }

    #[test]
    fn the_kept_values_are_named_in_one_warning() {
        let choice = |m: bool, e: bool| Choice {
            model: "sonnet".into(),
            effort: Some("high".into()),
            inherited_model: m,
            inherited_effort: e,
            previous_run: (m || e).then(|| "R1".to_string()),
        };
        assert_eq!(
            choice(true, true).warning().as_deref(),
            Some("model sonnet and effort high kept from run R1 (same attempt)")
        );
        assert_eq!(choice(true, false).warning().as_deref(), Some("model sonnet kept from run R1 (same attempt)"));
        assert_eq!(choice(false, true).warning().as_deref(), Some("effort high kept from run R1 (same attempt)"));
        assert_eq!(choice(false, false).warning(), None);
    }

    #[test]
    fn require_reason_refuses_only_when_on_and_missing() {
        assert!(require_reason_gate("T1", "dispatch", None, false).is_ok());
        assert!(require_reason_gate("T1", "dispatch", Some("why"), true).is_ok());
        let e = require_reason_gate("T1", "review start", None, true).unwrap_err().to_string();
        assert!(e.contains("--reason") && e.contains("require-reason off"), "{e}");
    }

    #[test]
    fn a_blank_reason_is_refused_and_a_given_one_is_trimmed() {
        assert_eq!(clean_reason(None).unwrap(), None);
        assert_eq!(clean_reason(Some("  why  ")).unwrap().as_deref(), Some("why"));
        assert!(clean_reason(Some(" 	 ")).unwrap_err().to_string().contains("--reason must not be empty"));
    }

    /// AC5 (3): no command shows quota events, so tests/launch_choice.rs's
    /// `spec_review_start_restores_a_reset_profile` checks the status through
    /// the CLI and this pins the event behind it.
    #[test]
    fn mark_recovered_writes_one_window_reset_without_profile_auto() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_global(&conn).unwrap();
        let profile = |id: &str, status: &str| Profile {
            profile_id: id.into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: None,
            config_dir: "C:/cfg".into(),
            default_models: None,
            status: status.into(),
        };
        global::insert_profile(&conn, &profile("claude-a", "active")).unwrap();
        global::insert_profile(&conn, &profile("claude-b", "rate_limited")).unwrap();

        assert!(mark_recovered(&conn, "claude-b", "T1", "R1").unwrap());
        assert_eq!(global::get_profile(&conn, "claude-b").unwrap().unwrap().status, "active");
        let events = global::list_quota_events(&conn, "claude-b").unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, "window_reset");
        let detail: serde_json::Value = serde_json::from_str(&events[0].detail).unwrap();
        assert_eq!(
            detail,
            serde_json::json!({"source": "auto", "task_id": "T1", "run_id": "R1"})
        );

        // already active: nothing changes, nothing is written
        assert!(!mark_recovered(&conn, "claude-b", "T1", "R2").unwrap());
        assert!(!mark_recovered(&conn, "claude-a", "T1", "R2").unwrap());
        assert_eq!(global::list_quota_events(&conn, "claude-b").unwrap().len(), 1);
        assert!(global::list_quota_events(&conn, "claude-a").unwrap().is_empty());
    }
}
