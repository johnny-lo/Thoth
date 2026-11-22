//! SMALL-13: the CI job that runs when no systemd user manager is usable keeps
//! a fixed list of integration test files that start no runner. The comment
//! above that step says how the list is derived. This test derives it the same
//! way and compares. It checks the repository files CI reads and runs no thoth
//! command.
//!
//! This file never writes one of the derivation's patterns as a literal of its
//! own: it reads them all from the workflow, so it belongs in the list.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const STEP_NAME: &str = "- name: test-without-user-manager";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_lossy(p: &Path) -> String {
    let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The comment block directly above the step, each line without its `#`.
fn comment_above(lines: &[&str], step: usize) -> Vec<String> {
    let mut block = Vec::new();
    let mut i = step;
    while i > 0 {
        let t = lines[i - 1].trim_start();
        match t.strip_prefix('#') {
            Some(rest) => block.push(rest.to_string()),
            None => break,
        }
        i -= 1;
    }
    block.reverse();
    block
}

/// Every `-e '...'` argument of the comment's grep command.
fn patterns_of(comment: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in comment {
        let mut rest = line.as_str();
        while let Some(at) = rest.find("-e '") {
            let after = &rest[at + 4..];
            let Some(end) = after.find('\'') else { break };
            out.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
    }
    out
}

/// The `#   NAME  (reason)` lines: files left out by hand.
fn hand_left_out_of(comment: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in comment {
        let Some(rest) = line.strip_prefix("   ") else { continue };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        if rest[name.len()..].starts_with("  (") {
            out.push(name);
        }
    }
    out
}

/// The names between `tests=(` and its closing `)` in the step.
fn array_of(lines: &[&str], step: usize) -> Vec<String> {
    let open = lines[step..]
        .iter()
        .position(|l| l.trim() == "tests=(")
        .expect("the step has a tests=( array")
        + step;
    let mut out = Vec::new();
    for l in &lines[open + 1..] {
        let t = l.trim();
        if t == ")" {
            return out;
        }
        out.push(t.to_string());
    }
    panic!("the tests=( array is never closed");
}

/// Paths the public tree leaves out. A checkout of the public tree has no such
/// file and nothing left to exclude.
fn public_tree_exclusions() -> Vec<String> {
    let p = root().join("scripts/public_tree.exclude");
    if !p.is_file() {
        return Vec::new();
    }
    read_lossy(&p)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn excluded(path: &str, entries: &[String]) -> bool {
    entries.iter().any(|e| if e.ends_with('/') { path.starts_with(e.as_str()) } else { path == e })
}

fn stem_list(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_no_runner_list_in_ci_matches_the_derivation_its_comment_gives() {
    let ci = read_lossy(&root().join(".github/workflows/ci.yml"));
    let lines: Vec<&str> = ci.lines().collect();
    let step = lines
        .iter()
        .position(|l| l.trim() == STEP_NAME)
        .expect("ci.yml has the test-without-user-manager step");

    let comment = comment_above(&lines, step);
    let patterns = patterns_of(&comment);
    let by_hand = hand_left_out_of(&comment);
    assert!(!patterns.is_empty(), "no -e '...' pattern read from the comment above the step: was it reworded?");
    assert!(!by_hand.is_empty(), "no '#   NAME  (reason)' line read from the comment above the step: was it reworded?");

    let array = array_of(&lines, step);
    let mut sorted = array.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(array, sorted, "the tests array must be sorted, one name per line, without duplicates");

    // tests/*.rs, not tests/common/ or other folders.
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(root().join("tests")).expect("tests/ exists") {
        let path = entry.unwrap().path();
        if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            files.push((stem, read_lossy(&path)));
        }
    }
    let existing: BTreeSet<String> = files.iter().map(|(s, _)| s.clone()).collect();
    for name in array.iter().chain(by_hand.iter()) {
        assert!(existing.contains(name), "{name} is listed but tests/{name}.rs does not exist");
    }

    let exclusions = public_tree_exclusions();
    let derived: BTreeSet<String> = files
        .iter()
        .filter(|(_, text)| !patterns.iter().any(|p| text.contains(p.as_str())))
        .filter(|(stem, _)| !excluded(&format!("tests/{stem}.rs"), &exclusions))
        .map(|(stem, _)| stem.clone())
        .filter(|stem| !by_hand.contains(stem))
        .collect();
    let listed: BTreeSet<String> = array.iter().cloned().collect();

    let to_add: BTreeSet<String> = derived.difference(&listed).cloned().collect();
    let to_drop: BTreeSet<String> = listed.difference(&derived).cloned().collect();
    assert!(
        to_add.is_empty() && to_drop.is_empty(),
        "the tests array of the test-without-user-manager step differs from the derivation its comment gives.\n  add: {}\n  drop: {}",
        stem_list(&to_add),
        stem_list(&to_drop)
    );
}

// ---------------------------------------------------------------------------
// CI-2: the workflow can be started by hand anywhere, pushes elsewhere stay
// skipped, and a manual run is not cancelled by a push. Reads the workflow
// file, which is the interface GitHub reads.

const REPO_CONDITION: &str = "github.repository == 'johnny-lo/Thoth'";
const DISPATCH_CONDITION: &str = "github.event_name == 'workflow_dispatch'";

/// Lines of a top-level section (`key:` at column 0): everything up to the
/// next top-level line, comments and blank lines removed.
fn top_level_section<'a>(lines: &[&'a str], key: &str) -> Vec<&'a str> {
    let header = format!("{key}:");
    let start = lines
        .iter()
        .position(|l| l.trim_end() == header)
        .unwrap_or_else(|| panic!("ci.yml has no top-level `{header}`"));
    lines[start + 1..]
        .iter()
        .copied()
        .take_while(|l| l.is_empty() || l.starts_with(' ') || l.starts_with('#'))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn ci_can_be_started_by_hand_and_a_push_does_not_cancel_a_manual_run() {
    let ci = read_lossy(&root().join(".github/workflows/ci.yml"));
    let lines: Vec<&str> = ci.lines().collect();
    let mut problems: Vec<String> = Vec::new();

    // `on:` keeps a trigger for workflow_dispatch.
    let on = top_level_section(&lines, "on");
    if !on.iter().any(|l| l.trim_end() == "  workflow_dispatch:") {
        problems.push("the `on:` block has no `workflow_dispatch:`".to_string());
    }

    // Every job key directly under `jobs:` carries the guard.
    let jobs = top_level_section(&lines, "jobs");
    let job_starts: Vec<usize> = jobs
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            l.starts_with("  ")
                && !l.starts_with("   ")
                && l.trim_end().ends_with(':')
        })
        .map(|(i, _)| i)
        .collect();
    if job_starts.is_empty() {
        problems.push("no job found under `jobs:`".to_string());
    }
    for (n, &start) in job_starts.iter().enumerate() {
        let end = job_starts.get(n + 1).copied().unwrap_or(jobs.len());
        let name = jobs[start].trim().trim_end_matches(':');
        let cond = jobs[start + 1..end]
            .iter()
            .find_map(|l| l.strip_prefix("    if:"))
            .map(|v| v.trim().trim_start_matches("${{").trim_end_matches("}}").trim().to_string());
        match cond {
            None => problems.push(format!("job `{name}` has no `if:`")),
            Some(c) => {
                let parts: Vec<&str> = c.split("||").map(str::trim).collect();
                for want in [REPO_CONDITION, DISPATCH_CONDITION] {
                    if !parts.contains(&want) {
                        problems.push(format!(
                            "job `{name}`: `if: {c}` lacks `{want}` as one side of an `||`"
                        ));
                    }
                }
            }
        }
    }

    // The concurrency group tells the event name apart.
    let concurrency = top_level_section(&lines, "concurrency");
    match concurrency.iter().find(|l| l.trim_start().starts_with("group:")) {
        None => problems.push("the concurrency block has no `group:`".to_string()),
        Some(g) if !g.contains("github.event_name") => {
            problems.push(format!("the concurrency group lacks github.event_name: {}", g.trim()))
        }
        Some(_) => {}
    }
    if !concurrency.iter().any(|l| l.trim() == "cancel-in-progress: true") {
        problems.push("cancel-in-progress is no longer true".to_string());
    }

    assert!(problems.is_empty(), "ci.yml: {}", problems.join("; "));
}
