//! The prompt of Thoth's own spec reviewer (SR-2, decision H9's `start`),
//! its edit mode and the author's check (SR-3).
//!
//! The reviewer reads the task's spec from `INPUT_DIR/INPUT_FILE` (the stored
//! bytes when there is no secret in them; with one, the masked and
//! re-serialized spec, while the fingerprint still covers the stored spec)
//! and judges it against the code
//! in a read-only checkout: the HEAD of the worktree `spec review start` ran
//! in when that contains the task's base, else the base. The prompt lists only
//! the criterion ids, never the criteria themselves: a spec can be tens of KB
//! and agy's prompt travels on the command line. In edit mode the same
//! prompt also says how to propose edits; the author's check reads the spec,
//! the proposed spec and the edit list from `INPUT_DIR` and judges only the
//! edits.

use crate::spec::TaskSpec;
use crate::spec_edits::ReviewMode;

/// Where the runner writes the reviewer's input, in the spec-review worktree.
pub const INPUT_DIR: &str = ".agent/spec-review-input";
pub const INPUT_FILE: &str = "spec.json";
/// SR-3, the author's check: the spec with the edits applied, and the edits.
pub const PROPOSED_FILE: &str = "proposed.json";
pub const EDITS_FILE: &str = "edits.json";
/// The report the runner ingests, in the spec-review worktree.
pub const OUTPUT_FILE: &str = ".agent/spec-review.json";

/// How the harness delivers the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// Claude Code: it has Bash, and writes the file with a redirect.
    ShellRedirect,
    /// Codex: a shell in a read-only sandbox; the final message is the
    /// document.
    FinalMessageWithShell,
    /// Antigravity: no shell; the document is its `structured_output`.
    FinalMessageNoShell,
}

/// The three lessons of decisions.md, and the read-only rule.
pub const RULE_READ_BODIES: &str = "For every function, file, test or command the spec names, open it in this checkout and read its body; a name that exists is not enough, the body has to do what the sentence says.";
pub const RULE_EDGE_CASES: &str = "List the edge cases you considered under the criterion they belong to, even the ones you decide need no change.";
pub const RULE_ACTIONS: &str = "For every action the spec tells the worker or the user to take (a command, a flag, a status transition, a file to edit), check that the current code allows it.";
pub const RULE_READ_ONLY: &str = "Do not modify any file, do not run cargo build or cargo test, and do not run thoth: judge against the source in this checkout, not against an installed binary.";
/// TESTS-1 (AC6): a spec that makes the worker run or prove the whole suite
/// is asking for evidence the worker's command time limit cannot hold; the
/// whole suite is the commander's verify.
pub const RULE_SUITE: &str = "A spec that tells the worker to run the whole test suite in one foreground command, or to prove the whole suite itself, is a finding: the whole suite is the control plane's verify, so the spec should name the tests to run and say that the whole-suite evidence is the commander's verify.";

/// SR-3: the edit-mode paragraph's first sentence, pinned by tests.
pub const EDIT_INTRO: &str = "Besides judging, you may change the spec directly: list every change in \"edits\" and thoth applies them in order; you write no file yourself.";

fn opening(how: How) -> String {
    let mut s = String::new();
    match how {
        How::ShellRedirect => {}
        How::FinalMessageWithShell => s.push_str(
            " It is a read-only sandbox: you cannot change any file, and you do not need to.",
        ),
        How::FinalMessageNoShell => s.push_str(" You have no shell and cannot write files."),
    }
    s.push('\n');
    s
}

fn read_verb(how: How) -> &'static str {
    match how {
        How::ShellRedirect | How::FinalMessageWithShell => "Read it with cat or your file tool",
        How::FinalMessageNoShell => "Open it with your file-viewing tool",
    }
}

fn output_paragraph(how: How) -> String {
    match how {
        How::ShellRedirect => format!(
            "You have no Write tool, so create the file with a shell redirect (a Bash heredoc works). Write {OUTPUT_FILE} in the current directory with exactly this shape:\n"
        ),
        How::FinalMessageWithShell | How::FinalMessageNoShell => format!(
            "Your final message is the spec review document: a JSON object in the output schema you were given, and nothing else. thoth stores it as {OUTPUT_FILE}; do not try to write that file yourself. For example:\n"
        ),
    }
}

pub fn build(task_id: &str, spec: &TaskSpec, criterion_ids: &[String], how: How, mode: ReviewMode) -> String {
    if mode == ReviewMode::Confirm {
        return build_confirm(task_id, how);
    }
    let mut s = String::from(
        "You are an independent spec reviewer. The current directory is a read-only checkout of the repository at the commit the task will be implemented against.",
    );
    s.push_str(&opening(how));

    s.push_str("\n## Goal\n");
    s.push_str(spec.goal.trim());
    s.push('\n');

    s.push_str("\n## Your input\n");
    s.push_str(&format!(
        "The spec is at {INPUT_DIR}/{INPUT_FILE}: read it whole. {}. Its acceptance_criteria array holds the criteria in order; their ids are listed below. Its constraints, non_goals and excluded_alternatives are part of what you judge: a remark about one of them goes on the criterion it affects most, with location spec.\n",
        read_verb(how)
    ));

    s.push_str("\n## The four checks\n");
    s.push_str("verifiable — every acceptance criterion can be verified: the sentence names what to run or read and what the result must be.\n");
    s.push_str("exists — every file, function, command, flag, test and status the spec names exists in this checkout, at the lines it cites when it cites lines.\n");
    s.push_str("consistent — the criteria, constraints and non-goals do not contradict each other.\n");
    s.push_str("steps — 'how to test' is written as step-by-step actions (do this, save that, compare with this), not as a result to achieve.\n");

    s.push_str("\n## Rules\n");
    s.push_str(&format!("1. {RULE_READ_BODIES}\n"));
    s.push_str(&format!("2. {RULE_EDGE_CASES}\n"));
    s.push_str(&format!("3. {RULE_ACTIONS}\n"));
    s.push_str(&format!("4. {RULE_READ_ONLY}\n"));
    s.push_str(&format!("5. {RULE_SUITE}\n"));
    s.push_str("6. APPROVE only when every criterion is ok. REVISE when any criterion needs a change; a revise criterion needs at least one finding. Every finding has a check (one of verifiable, exists, consistent, steps), a location (file:line in this checkout, or spec) and a note that says what to change.\n");

    if mode == ReviewMode::Edit {
        s.push_str("\n## You may edit the spec\n");
        s.push_str(EDIT_INTRO);
        s.push_str(" A change you are sure of (a wrong name, a missing step, a sentence that contradicts another, a scope line that was forgotten) goes in \"edits\"; a question only the author can settle stays a finding. Each edit has these keys, all present: \"field\" (one of goal, acceptance_criteria, constraints, non_goals, excluded_alternatives; nothing else can be edited), \"op\" (replace, insert or delete; goal and acceptance_criteria allow only replace, and a replaced acceptance criterion keeps the id before its colon), \"at\" (the entry's position from 0 in that list, counted after the earlier edits were applied; for insert it may equal the list's length to append; 0 for goal), \"anchor\" (for replace and delete: the first 10 characters of the entry as it is now, trimmed, or the whole entry when it is shorter; \"\" for insert), \"text\" (the whole new entry for replace and insert; \"\" for delete), \"criterion\" (the acceptance criterion this edit serves, one of the ids below) and \"reason\" (why, one sentence). With edits the verdict is REVISE, even when every criterion is ok; APPROVE proposes no edits. The author checks each edit before it lands, so keep each one small and explain it.\n");
    }

    s.push_str("\n## Output\n");
    s.push_str(&output_paragraph(how));
    let first = criterion_ids.first().map(|s| s.as_str()).unwrap_or("AC1");
    match mode {
        ReviewMode::Edit => {
            s.push_str("{\"verdict\":\"REVISE\",\"criteria\":[{\"id\":\"");
            s.push_str(first);
            s.push_str("\",\"verdict\":\"ok\",\"findings\":[]}],\"edits\":[{\"field\":\"constraints\",\"op\":\"insert\",\"at\":2,\"anchor\":\"\",\"text\":\"...\",\"criterion\":\"");
            s.push_str(first);
            s.push_str("\",\"reason\":\"...\"}]}\n");
        }
        _ => {
            s.push_str("{\"verdict\":\"REVISE\",\"criteria\":[{\"id\":\"");
            s.push_str(first);
            s.push_str("\",\"verdict\":\"revise\",\"findings\":[{\"check\":\"exists\",\"location\":\"src/lib.rs:12\",\"note\":\"...\"}]}]}\n");
        }
    }
    s.push_str(&format!(
        "Every id in \"criteria\" must be one of: {}.\n",
        criterion_ids.join(", ")
    ));
    s.push_str(&format!("This spec review is for task {task_id}.\n"));
    s
}

/// SR-3: the author's check. The spec, the proposed spec and the edits are
/// only in the files, never in the prompt.
fn build_confirm(task_id: &str, how: How) -> String {
    let mut s = String::from(
        "You are the author of this task's spec, checking the edits a reviewer proposed. The current directory is a read-only checkout of the repository at the commit the task will be implemented against.",
    );
    s.push_str(&opening(how));

    s.push_str("\n## Your input\n");
    s.push_str(&format!(
        "{INPUT_DIR}/{INPUT_FILE} is the spec as it is now, {INPUT_DIR}/{PROPOSED_FILE} is the spec with every edit applied, and {INPUT_DIR}/{EDITS_FILE} lists the edits: index, field, op, at, criterion, anchor, before, after, reason. {}; read all three whole.\n",
        read_verb(how)
    ));

    s.push_str("\n## What to judge\n");
    s.push_str("Only the edits, one by one. For each edit: (1) open in this checkout every function, file, test or command the edit names and read its body; the edit has to match the code, a name that exists is not enough. (2) Check that it contradicts nothing else in the spec: the other criteria, the constraints, the non-goals, the excluded alternatives. (3) Check that it keeps the original intent of the entry it changes. Do not review the rest of the spec again, and do not add findings of your own: an edit you cannot accept is revise, with a note that says why; every other edit is ok.\n");

    s.push_str("\n## Rules\n");
    s.push_str(&format!("1. {RULE_READ_ONLY}\n"));
    s.push_str("2. APPROVE only when every edit is ok; REVISE when any edit is revise, and every revise has a note. On APPROVE thoth replaces the task's spec with the proposed spec; on REVISE the spec stays as it is.\n");

    s.push_str("\n## Output\n");
    s.push_str(&output_paragraph(how));
    s.push_str("{\"verdict\":\"REVISE\",\"edits\":[{\"index\":0,\"verdict\":\"ok\",\"note\":\"\"},{\"index\":1,\"verdict\":\"revise\",\"note\":\"...\"}]}\n");
    s.push_str(&format!(
        "Every index listed in {INPUT_DIR}/{EDITS_FILE} appears in \"edits\" exactly once, and no other.\n"
    ));
    s.push_str(&format!("This author check is for task {task_id}.\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;

    fn spec() -> TaskSpec {
        crate::spec::parse_spec(SPEC).unwrap()
    }

    fn ids() -> Vec<String> {
        vec!["AC1".to_string(), "AC2".to_string()]
    }

    /// TESTS-1 (AC6): the suite rule as English, so a changed constant and a
    /// changed test cannot drift together unnoticed.
    const SUITE_RULE: &str = "A spec that tells the worker to run the whole test suite in one foreground command, or to prove the whole suite itself, is a finding: the whole suite is the control plane's verify, so the spec should name the tests to run and say that the whole-suite evidence is the commander's verify.";

    const HOWS: [How; 3] = [How::ShellRedirect, How::FinalMessageWithShell, How::FinalMessageNoShell];

    #[test]
    fn every_variant_carries_the_four_rules_the_input_path_and_the_id_line() {
        let spec = spec();
        let ids = ids();
        for how in HOWS {
            for mode in [ReviewMode::Review, ReviewMode::Edit] {
                let p = build("T1", &spec, &ids, how, mode);
                assert!(p.contains(RULE_READ_BODIES), "{how:?} {mode:?}");
                assert!(p.contains(RULE_EDGE_CASES), "{how:?} {mode:?}");
                assert!(p.contains(RULE_ACTIONS), "{how:?} {mode:?}");
                assert!(p.contains(RULE_READ_ONLY), "{how:?} {mode:?}");
                // TESTS-1 (AC6): rule 5 is the suite rule, once, between the
                // read-only rule and the verdict rule, inside ## Rules
                assert_eq!(p.matches(SUITE_RULE).count(), 1, "{how:?} {mode:?}: {p}");
                assert!(
                    p.contains(&format!(
                        "\n4. {RULE_READ_ONLY}\n5. {SUITE_RULE}\n6. APPROVE only when every criterion is ok."
                    )),
                    "{how:?} {mode:?}: {p}"
                );
                let rules = p.find("\n## Rules\n").unwrap();
                let output = p.find("\n## Output\n").unwrap();
                let suite = p.find(SUITE_RULE).unwrap();
                assert!(rules < suite && suite < output, "{how:?} {mode:?}");
                assert!(!p.contains("\n7. "), "{how:?} {mode:?}: {p}");
                assert!(p.contains(".agent/spec-review-input/spec.json"), "{how:?} {mode:?}");
                assert!(p.contains("verifiable — "), "{how:?} {mode:?}");
                assert!(p.contains("steps — "), "{how:?} {mode:?}");
                assert!(
                    p.lines().any(|l| l == "Every id in \"criteria\" must be one of: AC1, AC2."),
                    "{how:?} {mode:?}"
                );
                assert_eq!(p.lines().last(), Some("This spec review is for task T1."), "{how:?} {mode:?}");
                assert!(!p.contains("rejects a double spend"), "{how:?} {mode:?}");
            }
        }
    }

    #[test]
    fn the_output_paragraph_matches_the_harness() {
        let spec = spec();
        let ids = ids();
        let redirect = build("T1", &spec, &ids, How::ShellRedirect, ReviewMode::Review);
        assert!(redirect.contains("shell redirect"));
        assert!(redirect.contains("Write .agent/spec-review.json"));
        assert!(!redirect.contains("final message"));
        let with_shell = build("T1", &spec, &ids, How::FinalMessageWithShell, ReviewMode::Review);
        assert!(with_shell.contains("Your final message is the spec review document"));
        assert!(with_shell.contains("read-only sandbox"));
        assert!(!with_shell.contains("shell redirect"));
        let no_shell = build("T1", &spec, &ids, How::FinalMessageNoShell, ReviewMode::Review);
        assert!(no_shell.contains("Your final message is the spec review document"));
        assert!(no_shell.contains("You have no shell"));
    }

    #[test]
    fn edit_mode_adds_the_edit_section_and_review_mode_does_not() {
        let spec = spec();
        let ids = ids();
        for how in HOWS {
            let review = build("T1", &spec, &ids, how, ReviewMode::Review);
            assert!(!review.contains(EDIT_INTRO), "{how:?}");
            assert!(!review.contains("\"edits\""), "{how:?}");
            let edit = build("T1", &spec, &ids, how, ReviewMode::Edit);
            assert!(edit.contains("## You may edit the spec"), "{how:?}");
            assert!(edit.contains(EDIT_INTRO), "{how:?}");
            for word in ["\"field\"", "\"op\"", "\"at\"", "\"anchor\"", "\"text\"", "\"criterion\"", "\"reason\""] {
                assert!(edit.contains(word), "{how:?} lacks {word}");
            }
            assert!(edit.contains("goal and acceptance_criteria allow only replace"), "{how:?}");
            assert!(edit.contains("APPROVE proposes no edits"), "{how:?}");
            assert!(edit.contains("\"edits\":[{\"field\":\"constraints\""), "{how:?}");
        }
    }

    #[test]
    fn the_author_check_names_the_three_files_and_judges_only_the_edits() {
        let spec = spec();
        let ids = ids();
        for how in HOWS {
            let p = build("T1", &spec, &ids, how, ReviewMode::Confirm);
            assert!(p.starts_with("You are the author of this task's spec"), "{how:?}");
            for f in [
                ".agent/spec-review-input/spec.json",
                ".agent/spec-review-input/proposed.json",
                ".agent/spec-review-input/edits.json",
            ] {
                assert!(p.contains(f), "{how:?} lacks {f}");
            }
            assert!(p.contains("Only the edits"), "{how:?}");
            assert!(p.contains("Do not review the rest of the spec again"), "{how:?}");
            assert!(p.contains(RULE_READ_ONLY), "{how:?}");
            assert!(!p.contains(RULE_EDGE_CASES), "{how:?}");
            // TESTS-1 (AC6): the author check judges only the edits, so it
            // carries no whole-spec rule such as the suite rule
            assert!(!p.contains(SUITE_RULE), "{how:?}");
            assert!(!p.contains("guard the ledger"), "{how:?}: the spec stays in the files");
            assert!(!p.contains("Every id in \"criteria\""), "{how:?}");
            assert!(p.contains("{\"verdict\":\"REVISE\",\"edits\":[{\"index\":0"), "{how:?}");
            assert_eq!(p.lines().last(), Some("This author check is for task T1."), "{how:?}");
        }
        assert!(build("T1", &spec, &ids, How::ShellRedirect, ReviewMode::Confirm).contains("shell redirect"));
        assert!(build("T1", &spec, &ids, How::FinalMessageNoShell, ReviewMode::Confirm).contains("You have no shell"));
    }

    #[test]
    fn the_adapters_pick_their_delivery() {
        let spec = spec();
        let ids = ids();
        for mode in [ReviewMode::Review, ReviewMode::Edit, ReviewMode::Confirm] {
            assert_eq!(
                crate::adapter::for_harness("claude-code").build_spec_review_prompt("T1", &spec, &ids, mode),
                build("T1", &spec, &ids, How::ShellRedirect, mode)
            );
            assert_eq!(
                crate::adapter::for_harness("codex").build_spec_review_prompt("T1", &spec, &ids, mode),
                build("T1", &spec, &ids, How::FinalMessageWithShell, mode)
            );
            assert_eq!(
                crate::adapter::for_harness("antigravity").build_spec_review_prompt("T1", &spec, &ids, mode),
                build("T1", &spec, &ids, How::FinalMessageNoShell, mode)
            );
        }
    }
}
