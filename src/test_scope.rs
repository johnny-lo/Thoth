//! VSCOPE-1: which test targets a scoped check (`checks add --scope
//! cargo-test-targets`) runs for one task.
//!
//! The list comes from exactly two inputs: the task's current spec text and
//! the name-status diff from the task's base commit to the commit under
//! verification (`--no-renames`). Nothing a worker wrote - its result.json,
//! its `verification_requests`, the files in its worktree - is read, so a
//! worker cannot narrow the list.
//!
//! Rules:
//! - a spec names a target with a `tests/NAME.rs` path, a `tests/NAME/main.rs`
//!   path, or `cargo test --test NAME` (source `spec`);
//! - the diff adds (`diff_added`) or modifies (`diff_modified`) such a file;
//! - a deleted file is no target; a spec-named target the diff deletes goes
//!   to `dropped`; a spec-named target that is simply missing stays a target
//!   (cargo then fails - the conservative choice);
//! - a change to a shared test file (anything under a `tests/` subfolder that
//!   is not that subfolder's `main.rs`), to anything under `src/bin/`, or to
//!   `Cargo.toml`, `Cargo.lock` or `build.rs` falls back to every test target
//!   (`all_tests`, the paths in `all_tests_because`) - a conservative list;
//! - unit tests are always in the approved command itself, never added here.

use serde_json::{json, Value};

pub const SOURCE_SPEC: &str = "spec";
pub const SOURCE_DIFF_ADDED: &str = "diff_added";
pub const SOURCE_DIFF_MODIFIED: &str = "diff_modified";

pub const MODE_TARGETS: &str = "targets";
pub const MODE_ALL_TESTS: &str = "all_tests";

/// Why a target the rules found is not run.
pub const DROPPED_DELETED: &str = "deleted_in_diff";
pub const DROPPED_MISSING_AT_BASE: &str = "missing_at_base";

/// One integration test target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The cargo target name: the file name without `.rs`, or the folder of a
    /// `main.rs`.
    pub name: String,
    /// `tests/NAME.rs` or `tests/NAME/main.rs` when a path named it; `None`
    /// when only `cargo test --test NAME` did.
    pub path: Option<String>,
    /// `spec`, `diff_added`, `diff_modified`, in that order, each once.
    pub sources: Vec<&'static str>,
}

/// A target the rules found but that is not run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub target: Target,
    pub reason: &'static str,
}

/// What a scoped check runs for one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePlan {
    /// `targets` or `all_tests`.
    pub mode: &'static str,
    /// Sorted by name, unique; empty in `all_tests` mode.
    pub targets: Vec<Target>,
    /// The paths that triggered `all_tests`, sorted; empty in `targets` mode.
    pub all_tests_because: Vec<String>,
    pub dropped: Vec<Dropped>,
}

impl ScopePlan {
    /// The arguments appended to the approved command: `--test NAME` per
    /// target in order, `--tests` for `all_tests`, nothing without targets.
    pub fn appended_args(&self) -> Vec<String> {
        if self.mode == MODE_ALL_TESTS {
            return vec!["--tests".to_string()];
        }
        self.targets
            .iter()
            .flat_map(|t| ["--test".to_string(), t.name.clone()])
            .collect()
    }

    /// `results[].scope` of verify, and the review packet's `test_scope`
    /// item without its `check_id`.
    pub fn to_json(&self) -> Value {
        json!({
            "mode": self.mode,
            "targets": self.targets.iter().map(target_json).collect::<Vec<_>>(),
            "all_tests_because": self.all_tests_because,
            "dropped": self.dropped.iter().map(|d| {
                let mut v = target_json(&d.target);
                v["reason"] = json!(d.reason);
                v
            }).collect::<Vec<_>>(),
            "appended_args": self.appended_args(),
        })
    }

    /// `verify --at-base`: moves the targets `exists` says are not at the base
    /// commit into `dropped` (reason `missing_at_base`). `exists` gets the
    /// target and answers whether its file is there.
    pub fn without_missing(mut self, exists: impl Fn(&Target) -> bool) -> ScopePlan {
        let (keep, gone): (Vec<Target>, Vec<Target>) = self.targets.into_iter().partition(|t| exists(t));
        self.targets = keep;
        self.dropped.extend(gone.into_iter().map(|target| Dropped { target, reason: DROPPED_MISSING_AT_BASE }));
        self
    }

    /// The plan when the diff could not be read: every test target, with the
    /// reason as the trigger (conservative).
    pub fn all_tests_because_of(reason: &str) -> ScopePlan {
        ScopePlan {
            mode: MODE_ALL_TESTS,
            targets: Vec::new(),
            all_tests_because: vec![reason.to_string()],
            dropped: Vec::new(),
        }
    }
}

fn target_json(t: &Target) -> Value {
    json!({"name": t.name, "path": t.path, "sources": t.sources})
}

/// The characters a target name may have. A name with anything else is not
/// read as a target, so every appended argument is a plain word for every
/// shell.
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(name_char)
}

/// The target a repository path is, or `None`: `tests/NAME.rs` (one level)
/// or `tests/NAME/main.rs`.
pub fn target_of_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("tests/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let name = match parts.as_slice() {
        [file] => file.strip_suffix(".rs")?,
        [dir, "main.rs"] => dir,
        _ => return None,
    };
    valid_name(name).then(|| (name.to_string(), path.to_string()))
}

/// Whether a changed path makes the plan fall back to every test target.
fn triggers_all_tests(path: &str) -> bool {
    if matches!(path, "Cargo.toml" | "Cargo.lock" | "build.rs") || path.starts_with("src/bin/") {
        return true;
    }
    match path.strip_prefix("tests/") {
        // a file in a subfolder of tests/ that is not that subfolder's main.rs
        Some(rest) => rest.contains('/') && target_of_path(path).is_none(),
        None => false,
    }
}

/// Every string in the spec: each field's text when it is JSON, otherwise the
/// whole text as one field.
fn spec_texts(spec_text: &str) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(m) => m.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    match serde_json::from_str::<Value>(spec_text) {
        Ok(v) => {
            let mut out = Vec::new();
            walk(&v, &mut out);
            out
        }
        Err(_) => vec![spec_text.to_string()],
    }
}

/// The `(name, Some(path))` of every `tests/...` path in `text`, and the
/// `(name, None)` of every `cargo test --test NAME`.
fn spec_mentions(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let path_char = |c: char| name_char(c) || c == '/' || c == '.';
    // paths: `tests/` not preceded by a path character
    let mut from = 0;
    while let Some(i) = text[from..].find("tests/") {
        let at = from + i;
        from = at + "tests/".len();
        if text[..at].chars().next_back().is_some_and(|c| path_char(c) || c == '\\') {
            continue;
        }
        let end = text[at..].find(|c: char| !path_char(c)).map(|e| at + e).unwrap_or(text.len());
        // a path ends before trailing dots (end of a sentence)
        let candidate = text[at..end].trim_end_matches('.');
        if let Some((name, path)) = target_of_path(candidate) {
            out.push((name, Some(path)));
        }
    }
    // `cargo test ... --test NAME` (or `--test=NAME`) up to the end of that
    // command: a line end, a backtick, or a sentence or list separator
    let mut from = 0;
    while let Some(i) = text[from..].find("cargo test") {
        let start = from + i + "cargo test".len();
        from = start;
        let end = text[start..]
            .find(['\n', '`', ';', '；', '，', '、', '。', '|'])
            .map(|e| start + e)
            .unwrap_or(text.len());
        let words: Vec<&str> = text[start..end].split_whitespace().collect();
        let clean = |w: &str| w.trim_matches(|c: char| !name_char(c)).to_string();
        let mut k = 0;
        while k < words.len() {
            if words[k] == "--test" {
                if let Some(next) = words.get(k + 1) {
                    let name = clean(next);
                    if valid_name(&name) && !next.starts_with('-') {
                        out.push((name, None));
                    }
                }
                k += 2;
                continue;
            }
            if let Some(v) = words[k].strip_prefix("--test=") {
                let name = clean(v);
                if valid_name(&name) {
                    out.push((name, None));
                }
            }
            k += 1;
        }
    }
    out
}

/// AC2: the scope plan for a task from its spec text and the name-status
/// diff (`(status letter, path)`, `--no-renames`) from its base commit to the
/// commit under verification.
pub fn scope_targets(spec_text: &str, name_status: &[(char, String)]) -> ScopePlan {
    use std::collections::BTreeMap;
    // name -> (path, sources)
    let mut found: BTreeMap<String, (Option<String>, Vec<&'static str>)> = BTreeMap::new();
    let mut add = |name: String, path: Option<String>, source: &'static str, prefer_path: bool| {
        let entry = found.entry(name).or_insert((None, Vec::new()));
        if path.is_some() && (entry.0.is_none() || prefer_path) {
            entry.0 = path;
        }
        if !entry.1.contains(&source) {
            entry.1.push(source);
        }
    };
    for text in spec_texts(spec_text) {
        for (name, path) in spec_mentions(&text) {
            add(name, path, SOURCE_SPEC, false);
        }
    }
    let mut because: Vec<String> = Vec::new();
    let mut deleted: BTreeMap<String, String> = BTreeMap::new();
    for (status, path) in name_status {
        if triggers_all_tests(path) {
            because.push(path.clone());
        }
        let Some((name, path)) = target_of_path(path) else { continue };
        match status {
            'A' => add(name, Some(path), SOURCE_DIFF_ADDED, true),
            'M' | 'T' => add(name, Some(path), SOURCE_DIFF_MODIFIED, true),
            'D' => {
                deleted.insert(name, path);
            }
            _ => {}
        }
    }
    let order = |s: &&str| [SOURCE_SPEC, SOURCE_DIFF_ADDED, SOURCE_DIFF_MODIFIED].iter().position(|x| x == s);
    let mut targets = Vec::new();
    let mut dropped = Vec::new();
    for (name, (path, mut sources)) in found {
        sources.sort_by_key(order);
        let from_diff = sources.iter().any(|s| *s != SOURCE_SPEC);
        match deleted.get(&name) {
            // deleted, and no added or modified file of the same name
            Some(gone) if !from_diff => dropped.push(Dropped {
                target: Target { name, path: path.or_else(|| Some(gone.clone())), sources },
                reason: DROPPED_DELETED,
            }),
            _ => targets.push(Target { name, path, sources }),
        }
    }
    because.sort();
    because.dedup();
    if because.is_empty() {
        ScopePlan { mode: MODE_TARGETS, targets, all_tests_because: because, dropped }
    } else {
        ScopePlan { mode: MODE_ALL_TESTS, targets: Vec::new(), all_tests_because: because, dropped }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(entries: &[(char, &str)]) -> Vec<(char, String)> {
        entries.iter().map(|(s, p)| (*s, p.to_string())).collect()
    }

    fn spec(text: &str) -> String {
        json!({"goal": "g", "acceptance_criteria": [text], "constraints": []}).to_string()
    }

    fn names(plan: &ScopePlan) -> Vec<&str> {
        plan.targets.iter().map(|t| t.name.as_str()).collect()
    }

    fn target<'a>(plan: &'a ScopePlan, name: &str) -> &'a Target {
        plan.targets.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no {name} in {plan:?}"))
    }

    #[test]
    fn scope_targets_reads_the_three_spec_spellings() {
        let plan = scope_targets(
            &spec("tests are in tests/alpha.rs and tests/beta/main.rs; run `cargo test --test gamma`."),
            &[],
        );
        assert_eq!(plan.mode, MODE_TARGETS);
        assert_eq!(names(&plan), ["alpha", "beta", "gamma"]);
        assert_eq!(target(&plan, "alpha").path.as_deref(), Some("tests/alpha.rs"));
        assert_eq!(target(&plan, "beta").path.as_deref(), Some("tests/beta/main.rs"));
        assert_eq!(target(&plan, "gamma").path, None);
        for t in &plan.targets {
            assert_eq!(t.sources, [SOURCE_SPEC]);
        }
        assert_eq!(plan.appended_args(), ["--test", "alpha", "--test", "beta", "--test", "gamma"]);
    }

    #[test]
    fn scope_targets_reads_every_spec_field_and_several_test_flags() {
        let s = json!({"goal": "see tests/one.rs", "constraints": ["x"],
            "acceptance_criteria": ["AC9: cargo test --test two --test=three, and tests/one.rs again"]})
        .to_string();
        let plan = scope_targets(&s, &[]);
        assert_eq!(names(&plan), ["one", "three", "two"]);
    }

    #[test]
    fn scope_targets_ignores_paths_that_are_not_first_level_tests() {
        let plan = scope_targets(
            &spec("src/tests/x.rs, crate/tests/y.rs, tests/common/mod.rs, tests/a/b/main.rs, tests/z.txt, cargo test --bin thoth scope_targets"),
            &[],
        );
        assert_eq!(plan.targets, vec![], "{plan:?}");
        assert_eq!(plan.mode, MODE_TARGETS);
    }

    #[test]
    fn scope_targets_takes_added_and_modified_files_from_the_diff() {
        let plan = scope_targets(
            &spec("nothing named"),
            &diff(&[('A', "tests/new_one.rs"), ('M', "tests/old_one.rs"), ('M', "tests/dir/main.rs"), ('M', "src/lib.rs")]),
        );
        assert_eq!(plan.mode, MODE_TARGETS);
        assert_eq!(names(&plan), ["dir", "new_one", "old_one"]);
        assert_eq!(target(&plan, "new_one").sources, [SOURCE_DIFF_ADDED]);
        assert_eq!(target(&plan, "old_one").sources, [SOURCE_DIFF_MODIFIED]);
        assert_eq!(target(&plan, "dir").path.as_deref(), Some("tests/dir/main.rs"));
    }

    #[test]
    fn scope_targets_lists_every_source_of_one_target_once() {
        let plan = scope_targets(
            &spec("cargo test --test both; and tests/both.rs"),
            &diff(&[('M', "tests/both.rs")]),
        );
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].sources, [SOURCE_SPEC, SOURCE_DIFF_MODIFIED]);
        assert_eq!(plan.targets[0].path.as_deref(), Some("tests/both.rs"));
    }

    #[test]
    fn scope_targets_drops_a_spec_named_file_the_diff_deletes() {
        let plan = scope_targets(&spec("tests/gone.rs and tests/kept.rs"), &diff(&[('D', "tests/gone.rs"), ('D', "tests/other.rs")]));
        assert_eq!(names(&plan), ["kept"]);
        assert_eq!(plan.dropped.len(), 1, "{plan:?}");
        assert_eq!(plan.dropped[0].target.name, "gone");
        assert_eq!(plan.dropped[0].reason, DROPPED_DELETED);
        // a deleted file the spec does not name is neither a target nor dropped
        assert!(!plan.dropped.iter().any(|d| d.target.name == "other"));
        // a target named only by `--test` is dropped when its file is deleted
        let plan = scope_targets(&spec("cargo test --test gone"), &diff(&[('D', "tests/gone/main.rs")]));
        assert_eq!(plan.targets, vec![]);
        assert_eq!(plan.dropped[0].target.path.as_deref(), Some("tests/gone/main.rs"));
    }

    #[test]
    fn scope_targets_keeps_a_spec_named_file_that_is_missing_but_not_deleted() {
        // the diff says nothing about it: still a target, and cargo will fail
        let plan = scope_targets(&spec("tests/never_written.rs"), &diff(&[('M', "src/x.rs")]));
        assert_eq!(names(&plan), ["never_written"]);
        assert!(plan.dropped.is_empty());
    }

    #[test]
    fn scope_targets_keeps_a_target_moved_from_a_file_to_a_folder() {
        let plan = scope_targets(&spec("tests/moved.rs"), &diff(&[('D', "tests/moved.rs"), ('A', "tests/moved/main.rs")]));
        assert_eq!(names(&plan), ["moved"]);
        assert_eq!(target(&plan, "moved").path.as_deref(), Some("tests/moved/main.rs"));
        assert!(plan.dropped.is_empty());
    }

    #[test]
    fn scope_targets_falls_back_to_all_tests_for_each_trigger() {
        for (status, path) in [
            ('M', "tests/common/mod.rs"),
            ('A', "tests/fixtures/data.json"),
            ('D', "tests/helpers/util.rs"),
            ('M', "tests/a/b/main.rs"),
            ('M', "src/bin/fake-check.rs"),
            ('A', "src/bin/support/mod.rs"),
            ('M', "Cargo.toml"),
            ('M', "Cargo.lock"),
            ('A', "build.rs"),
        ] {
            let plan = scope_targets(&spec("tests/named.rs"), &diff(&[(status, path), ('A', "tests/new.rs")]));
            assert_eq!(plan.mode, MODE_ALL_TESTS, "{path}");
            assert_eq!(plan.all_tests_because, [path.to_string()], "{path}");
            assert!(plan.targets.is_empty(), "{path}: {plan:?}");
            assert_eq!(plan.appended_args(), ["--tests"], "{path}");
        }
    }

    #[test]
    fn scope_targets_lists_every_trigger_sorted() {
        let plan = scope_targets("{}", &diff(&[('M', "Cargo.toml"), ('M', "tests/common/mod.rs"), ('M', "Cargo.toml")]));
        assert_eq!(plan.all_tests_because, ["Cargo.toml", "tests/common/mod.rs"]);
    }

    #[test]
    fn scope_targets_has_no_targets_when_only_src_changed_and_the_spec_names_none() {
        let plan = scope_targets(&spec("change the parser"), &diff(&[('M', "src/main.rs"), ('A', "src/new.rs"), ('M', "docs/x.md")]));
        assert_eq!(plan.mode, MODE_TARGETS);
        assert!(plan.targets.is_empty());
        assert!(plan.all_tests_because.is_empty());
        assert!(plan.appended_args().is_empty());
        assert_eq!(plan.to_json()["appended_args"], json!([]));
    }

    #[test]
    fn scope_targets_reads_a_spec_that_is_not_json_as_one_field() {
        let plan = scope_targets("run cargo test --test plain\n", &[]);
        assert_eq!(names(&plan), ["plain"]);
    }

    #[test]
    fn scope_targets_at_base_moves_missing_targets_to_dropped() {
        let plan = scope_targets(&spec("tests/here.rs tests/new_file.rs"), &[]);
        let plan = plan.without_missing(|t| t.name == "here");
        assert_eq!(names(&plan), ["here"]);
        assert_eq!(plan.dropped[0].target.name, "new_file");
        assert_eq!(plan.dropped[0].reason, DROPPED_MISSING_AT_BASE);
        let v = plan.to_json();
        assert_eq!(v["dropped"][0]["reason"], DROPPED_MISSING_AT_BASE);
        assert_eq!(v["targets"][0], json!({"name": "here", "path": "tests/here.rs", "sources": ["spec"]}));
    }
}
