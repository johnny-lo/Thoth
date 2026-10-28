//! Protected files (P7-2).
//!
//! v6 §4: "修改測試讓它通過，是最經典的逃逸路徑". A worker that edits or deletes
//! an existing test file, or a file an approved check reads, has changed what
//! the check proves. Thoth does not judge whether the change weakened anything
//! (that is the reviewer's job); it only works out *which* such files a diff
//! touched, records them on the diff evidence, shows them to the reviewer, and
//! requires the worker to declare each one (ingest rule 6).
//!
//! Everything here is a pure function of a path and the repo's catalogue. The
//! catalogue lives in `state.db` (the `checks` table and one `meta` row), where
//! a worker cannot reach it.
//!
//! Limit: only paths are looked at. An inline `#[cfg(test)]` module inside a
//! source file is not detected.
//!
//! SMALL-4: "existing" means existing at the task's base commit, not at the
//! run's (a rework starts from the previous candidate), and a file whose
//! content at the candidate equals its content at the integration tip (the
//! main worktree's HEAD) came in through a merge and is not counted. See
//! `attributed` and `evidence::record_diff_attributed`.

use crate::checks::Check;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

/// The `meta` key holding the repo's extra patterns: a JSON array of strings,
/// unique and sorted.
pub const META_KEY: &str = "protected_patterns";

/// The claim kind a worker declares a protected change with.
pub const CLAIM_KIND: &str = "protected_change";

/// Why a path is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A test file: a built-in rule or one of the repo's extra patterns.
    Test,
    /// An input file of a check in the catalogue (approved or not).
    CheckInput,
    /// A file named like a word of a check's command (AC9).
    CheckWord,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Test => "test",
            Kind::CheckInput => "check_input",
            Kind::CheckWord => "check_word",
        }
    }
}

/// Backslashes folded to `/`, everything lower-cased. Every comparison in this
/// module is made on this form.
pub fn normalize(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

/// The built-in test-file rules, on a normalized path.
fn is_builtin_test(p: &str) -> bool {
    let mut segments: Vec<&str> = p.split('/').collect();
    let base = segments.pop().unwrap_or("");
    if segments
        .iter()
        .any(|s| matches!(*s, "tests" | "test" | "__tests__") || s.ends_with(".tests"))
    {
        return true;
    }
    const SUFFIXES: [&str; 12] = [
        "_test.rs", "_test.py", "_test.go", "_test.ts", ".test.ts", ".test.tsx", ".test.js",
        ".spec.ts", ".spec.tsx", ".spec.js", "tests.cs", "test.cs",
    ];
    base == "conftest.py" || base.starts_with("test_") || SUFFIXES.iter().any(|s| base.ends_with(s))
}

/// Validates one extra pattern and returns its stored (normalized) form, or
/// says why it is not one of the three accepted shapes:
///
/// - `name/` - a path segment: any path one of whose segments is `name`;
/// - no `/`, at most one `*` at the start or the end - the file name
///   (`*.snap`, `golden_*`, or an exact name);
/// - anything else - the whole normalized path, compared for equality.
pub fn parse_pattern(raw: &str) -> std::result::Result<String, String> {
    let p = normalize(raw.trim());
    let stars = p.matches('*').count();
    let bad = |why: &str| {
        Err(format!(
            "pattern '{raw}' is not accepted: {why}. A pattern is 'name/' (a directory segment), \
             a file name with at most one '*' at its start or end ('*.snap', 'golden_*'), or a \
             whole path without '*'"
        ))
    };
    if p.is_empty() {
        return bad("it is empty");
    }
    if let Some(segment) = p.strip_suffix('/') {
        if segment.is_empty() || segment.contains('/') || stars > 0 {
            return bad("a directory pattern is one segment, without '*' or another '/'");
        }
        return Ok(p);
    }
    if !p.contains('/') {
        let edge = p.starts_with('*') || p.ends_with('*');
        if stars == 0 || (stars == 1 && edge && p.len() > 1) {
            return Ok(p);
        }
        return bad("a file-name pattern takes at most one '*', at its start or its end");
    }
    if stars > 0 {
        return bad("a whole-path pattern takes no '*'");
    }
    Ok(p)
}

/// Whether a stored pattern (see `parse_pattern`) matches a normalized path.
fn pattern_matches(pattern: &str, p: &str) -> bool {
    if let Some(segment) = pattern.strip_suffix('/') {
        return p.split('/').any(|s| s == segment);
    }
    if !pattern.contains('/') {
        let base = p.rsplit('/').next().unwrap_or(p);
        if let Some(suffix) = pattern.strip_prefix('*') {
            return base.ends_with(suffix);
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            return base.starts_with(prefix);
        }
        return base == pattern;
    }
    p == pattern
}

/// A name's stem: its last path segment (`/` and `\` both separate), up to the
/// first `.`, lower-cased. May be empty.
fn stem_of(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    last.split('.').next().unwrap_or("").to_lowercase()
}

/// AC9: the stems of every word of every check's command (approved or not).
/// `argv` is split as its run splits it (`process::split_argv`), any other
/// shell on whitespace; a word starting with `-` is skipped, and so is an
/// empty stem. Sorted, unique.
pub fn check_stems(checks: &[Check]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in checks {
        let words: Vec<String> = if c.shell == "argv" {
            crate::process::split_argv(&c.cmd)
        } else {
            c.cmd.split_whitespace().map(str::to_string).collect()
        };
        for w in words.iter().filter(|w| !w.starts_with('-')) {
            let stem = stem_of(w);
            if !stem.is_empty() {
                out.push(stem);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// AC9: whether a path's file name has the stem of a check word.
pub fn is_check_word(path: &str, check_stems: &[String]) -> bool {
    let stem = stem_of(path);
    !stem.is_empty() && check_stems.contains(&stem)
}

/// Why `path` is protected, or `None`. `CheckInput` wins over `Test`, which
/// wins over `CheckWord`. A path matching one of the repo's extra patterns is
/// a `Test`.
pub fn classify(
    path: &str,
    extra_patterns: &[String],
    check_inputs: &[String],
    check_stems: &[String],
) -> Option<Kind> {
    let p = normalize(path);
    if check_inputs.iter().any(|i| normalize(i) == p) {
        return Some(Kind::CheckInput);
    }
    if is_builtin_test(&p) || extra_patterns.iter().any(|pat| pattern_matches(pat, &p)) {
        return Some(Kind::Test);
    }
    is_check_word(&p, check_stems).then_some(Kind::CheckWord)
}

/// What `classify` is asked against for one repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// The repo's extra patterns, as stored.
    pub patterns: Vec<String>,
    /// The inputs of every check, approved or not.
    pub inputs: Vec<String>,
    /// `check_stems` of every check.
    pub stems: Vec<String>,
}

impl Catalogue {
    pub fn from_parts(patterns: Vec<String>, checks: &[Check]) -> Self {
        let mut inputs: Vec<String> = checks.iter().flat_map(|c| c.inputs.clone()).collect();
        inputs.sort();
        inputs.dedup();
        Catalogue {
            patterns,
            inputs,
            stems: check_stems(checks),
        }
    }

    pub fn load(conn: &Connection) -> Result<Self> {
        Ok(Self::from_parts(
            load_patterns(conn)?,
            &crate::checks::list_checks(conn)?,
        ))
    }

    pub fn classify(&self, path: &str) -> Option<Kind> {
        classify(path, &self.patterns, &self.inputs, &self.stems)
    }
}

/// The repo's extra patterns; none when the row is missing or unreadable.
pub fn load_patterns(conn: &Connection) -> Result<Vec<String>> {
    Ok(crate::db::state::get_meta(conn, META_KEY)?
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default())
}

/// Stores the patterns unique and sorted, and returns them that way.
pub fn store_patterns(conn: &Connection, mut patterns: Vec<String>) -> Result<Vec<String>> {
    patterns.sort();
    patterns.dedup();
    crate::db::state::set_meta(conn, META_KEY, &serde_json::to_string(&patterns)?)?;
    Ok(patterns)
}

/// One entry of a diff evidence's `protected_changes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// As git printed it (`/`-separated), not normalized.
    pub path: String,
    /// `modified`, `deleted`, `type_changed` or `added`.
    pub status: String,
    pub kind: String,
}

impl Change {
    pub fn to_json(&self) -> Value {
        json!({"path": self.path, "status": self.status, "kind": self.kind})
    }
}

/// The protected changes among a diff's `(status letter, path)` entries,
/// sorted by path. `M`, `D` and `T` paths are classified; an `A` path counts
/// only when its name is a check word (AC9), whatever else it also is.
pub fn changes(catalogue: &Catalogue, name_status: &[(char, String)]) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    for (letter, path) in name_status {
        let (status, kind) = match letter {
            'M' | 'D' | 'T' => {
                let Some(kind) = catalogue.classify(path) else {
                    continue;
                };
                let status = match letter {
                    'M' => "modified",
                    'D' => "deleted",
                    _ => "type_changed",
                };
                (status, kind)
            }
            'A' if is_check_word(path, &catalogue.stems) => ("added", Kind::CheckWord),
            _ => continue,
        };
        out.push(Change {
            path: path.clone(),
            status: status.to_string(),
            kind: kind.as_str().to_string(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// SMALL-4: the entries of `own` (a diff against the task's base) that are
/// this task's own. `differs_from_tip` is the paths whose content at the
/// candidate differs from the integration tip; `None` keeps everything. Paths
/// are compared as git printed them. Order is kept.
pub fn attributed(own: &[(char, String)], differs_from_tip: Option<&[String]>) -> Vec<(char, String)> {
    match differs_from_tip {
        None => own.to_vec(),
        Some(set) => own.iter().filter(|(_, p)| set.contains(p)).cloned().collect(),
    }
}

/// Reads `protected_changes` back out of a diff evidence's `detail`. A row
/// written before the key existed yields none.
pub fn changes_in_detail(detail: &Value) -> Vec<Change> {
    detail["protected_changes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some(Change {
                        path: c["path"].as_str()?.to_string(),
                        status: c["status"].as_str().unwrap_or("").to_string(),
                        kind: c["kind"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `protected_changes` read strictly: `None` ("not known") unless the key is
/// an array whose every entry has string `path`, `status` and `kind`, the kind
/// being one this module writes. A diff evidence recorded before the key
/// existed, or a damaged one, is `None`, never an empty list.
pub fn strict_changes(detail: &Value) -> Option<Vec<Change>> {
    detail["protected_changes"]
        .as_array()?
        .iter()
        .map(|c| {
            let kind = c["kind"].as_str()?;
            if ![Kind::Test, Kind::CheckInput, Kind::CheckWord]
                .iter()
                .any(|k| k.as_str() == kind)
            {
                return None;
            }
            Some(Change {
                path: c["path"].as_str()?.to_string(),
                status: c["status"].as_str()?.to_string(),
                kind: kind.to_string(),
            })
        })
        .collect()
}

/// A run's newest diff evidence: the row with `run_id` = the run and `kind` =
/// `diff` whose code has the largest number.
pub struct LatestDiff {
    pub code: String,
    /// `Null` when the stored detail is not valid JSON.
    pub detail: Value,
    /// `strict_changes` of `detail`.
    pub changes: Option<Vec<Change>>,
}

/// The one way the newest diff evidence of a run is picked (ingest rule 6,
/// `checks approve --run`, and the gate in `review start` / `review record`).
/// `None` when the run has no diff evidence.
pub fn latest_diff(conn: &Connection, run: &crate::db::state::Run) -> Result<Option<LatestDiff>> {
    let code_number = |code: &str| -> u64 {
        code.trim_start_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .unwrap_or(0)
    };
    let newest = crate::db::state::list_evidence_for_task(conn, &run.task_id)?
        .into_iter()
        .filter(|e| e.kind == "diff" && e.run_id.as_deref() == Some(run.run_id.as_str()))
        .max_by_key(|e| code_number(&e.code));
    Ok(newest.map(|e| {
        let detail: Value = serde_json::from_str(&e.detail).unwrap_or(Value::Null);
        let changes = strict_changes(&detail);
        LatestDiff { code: e.code, detail, changes }
    }))
}

/// Every `(path, status)` approved for `run_id` by a `protected_change_approved`
/// event (`checks approve --run`), oldest first.
pub fn approved_pairs(conn: &Connection, run_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")?;
    let details = stmt
        .query_map(
            rusqlite::params![run_id, crate::model::event_type::PROTECTED_CHANGE_APPROVED],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let mut out: Vec<(String, String)> = Vec::new();
    for d in details {
        let v: Value = serde_json::from_str(&d).unwrap_or(Value::Null);
        for c in v["changes"].as_array().into_iter().flatten() {
            if let (Some(p), Some(s)) = (c["path"].as_str(), c["status"].as_str()) {
                out.push((p.to_string(), s.to_string()));
            }
        }
    }
    Ok(out)
}

/// The `test` and `check_word` changes no approval covers yet, in input
/// order. `check_input` changes are never listed: verify's fingerprint check
/// already refuses a changed check input (and lets a version-only change
/// through), so approving them here as well would add nothing.
pub fn uncovered(changes: &[Change], approved: &[(String, String)]) -> Vec<Change> {
    changes
        .iter()
        .filter(|c| c.kind == Kind::Test.as_str() || c.kind == Kind::CheckWord.as_str())
        .filter(|c| !approved.iter().any(|(p, s)| *p == c.path && *s == c.status))
        .cloned()
        .collect()
}

/// Rule 6: whether a `protected_change` claim's text declares `path`. The
/// text, trimmed and with `\` folded to `/`, must start with the path, and the
/// path must be followed by `:`, whitespace, or nothing.
pub fn claim_declares(text: &str, path: &str) -> bool {
    let t = text.trim().replace('\\', "/");
    match t.strip_prefix(path) {
        Some(rest) => rest.chars().next().is_none_or(|c| c == ':' || c.is_whitespace()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(shell: &str, cmd: &str, inputs: &[&str]) -> Check {
        Check {
            check_id: "c".into(),
            shell: shell.into(),
            cmd: cmd.into(),
            cwd: ".".into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            created_at: "n".into(),
            approved_at: None,
            approved_fingerprint: None,
            approved_detail: None,
            approved_commit: None,
            exe_pin: "hash".into(),
        }
    }

    fn plain(path: &str) -> Option<Kind> {
        classify(path, &[], &[], &[])
    }

    /// AC1: every built-in rule, one path it catches and one near miss.
    #[test]
    fn each_builtin_test_rule_has_a_hit_and_a_near_miss() {
        let table: [(&str, &str, &str); 17] = [
            ("tests/ segment", "crates/core/tests/api.rs", "src/contests/api.rs"),
            ("test/ segment", "test/helper.js", "src/latest/helper.js"),
            ("__tests__/ segment", "src/__tests__/button.tsx", "src/my__tests__x/button.tsx"),
            (".tests/ segment", "Foo.Tests/Bar.cs", "Foo.Testsuite/Bar.cs"),
            ("conftest.py", "backend/conftest.py", "backend/conftest.pyc"),
            ("test_ prefix", "src/test_parser.py", "src/attest_parser.py"),
            ("_test.rs", "src/thing_test.rs", "src/thing_tests.rs"),
            ("_test.py", "pkg/thing_test.py", "pkg/thing_test.pyi"),
            ("_test.go", "pkg/handler_test.go", "pkg/handler_test.gox"),
            ("_test.ts", "web/button_test.ts", "web/button_test.tsx"),
            (".test.ts", "web/button.test.ts", "web/button.testing.ts"),
            (".test.tsx", "web/button.test.tsx", "web/button.tested.tsx"),
            (".test.js", "web/button.test.js", "web/button.test.json"),
            (".spec.ts", "web/button.spec.ts", "web/button.specs.ts"),
            (".spec.tsx", "web/button.spec.tsx", "web/button.spec.tsxx"),
            (".spec.js", "web/button.spec.js", "web/button.spec.json"),
            ("tests.cs", "src/FooTests.cs", "src/FooTests.csx"),
        ];
        for (rule, yes, no) in table {
            assert_eq!(plain(yes), Some(Kind::Test), "{rule}: {yes} is a test file");
            assert_eq!(plain(no), None, "{rule}: {no} is not");
        }
        // `test.cs` is its own rule: a name that ends in it without ending in
        // `tests.cs`
        assert_eq!(plain("src/FooTest.cs"), Some(Kind::Test));
        assert_eq!(plain("src/FooTest.css"), None);
    }

    /// AC1's three named paths.
    #[test]
    fn the_three_paths_the_spec_names() {
        assert_eq!(plain("src/foo.rs"), None);
        assert_eq!(plain("Tests/Foo.cs"), Some(Kind::Test));
        assert_eq!(plain("src/attest.rs"), None);
    }

    /// AC1: backslashes fold to `/` and case is ignored before any comparison.
    #[test]
    fn paths_are_normalized_before_they_are_compared() {
        assert_eq!(plain(r"Tests\Windows_Style.rs"), Some(Kind::Test));
        assert_eq!(plain("SRC/CONFTEST.PY"), Some(Kind::Test));
        assert_eq!(
            classify(r"Sub\Cargo.TOML", &[], &["sub/Cargo.toml".to_string()], &[]),
            Some(Kind::CheckInput)
        );
    }

    /// AC1: a check input is the normalized path equal to one of the inputs,
    /// and it wins over the test rules.
    #[test]
    fn a_check_input_is_an_exact_path_and_wins_over_the_test_rules() {
        let inputs = vec!["Cargo.toml".to_string(), "tests/data.json".to_string()];
        assert_eq!(classify("Cargo.toml", &[], &inputs, &[]), Some(Kind::CheckInput));
        assert_eq!(classify("sub/Cargo.toml", &[], &inputs, &[]), None);
        // both a test path and an input: CheckInput
        assert_eq!(classify("tests/data.json", &[], &inputs, &[]), Some(Kind::CheckInput));
        assert_eq!(classify("tests/other.json", &[], &inputs, &[]), Some(Kind::Test));
    }

    /// AC9: CheckInput, then Test, then CheckWord.
    #[test]
    fn classify_prefers_check_input_then_test_then_check_word() {
        let inputs = vec!["cargo.lock".to_string()];
        let stems = vec!["cargo".to_string()];
        assert_eq!(classify("cargo.lock", &[], &inputs, &stems), Some(Kind::CheckInput));
        assert_eq!(classify("tests/cargo.rs", &[], &inputs, &stems), Some(Kind::Test));
        assert_eq!(classify("tools/cargo.cmd", &[], &inputs, &stems), Some(Kind::CheckWord));
        assert_eq!(classify("tools/cargo_helper.rs", &[], &inputs, &stems), None);
    }

    /// AC2: the three pattern shapes, each with a hit and a miss.
    #[test]
    fn the_three_pattern_shapes_match_as_documented() {
        let pats = |p: &str| vec![parse_pattern(p).unwrap()];
        // a directory segment, anywhere in the path
        assert_eq!(classify("a/fixtures/b.json", &pats("fixtures/"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("fixtures/b.json", &pats("fixtures/"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("a/myfixtures/b.json", &pats("fixtures/"), &[], &[]), None);
        // the file name, `*` at the start
        assert_eq!(classify("a/b.snap", &pats("*.snap"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("a/b.snapshot", &pats("*.snap"), &[], &[]), None);
        // the file name, `*` at the end
        assert_eq!(classify("golden_x.txt", &pats("golden_*"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("golden_dir/x.txt", &pats("golden_*"), &[], &[]), None);
        // the file name, no `*`
        assert_eq!(classify("a/Makefile", &pats("Makefile"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("a/Makefile.in", &pats("Makefile"), &[], &[]), None);
        // anything else: the whole path
        assert_eq!(classify("data/expected.json", &pats("data/expected.json"), &[], &[]), Some(Kind::Test));
        assert_eq!(classify("x/data/expected.json", &pats("data/expected.json"), &[], &[]), None);
        // stored normalized, so case and backslashes in the pattern do not matter
        assert_eq!(parse_pattern(r"Data\Expected.JSON").unwrap(), "data/expected.json");
    }

    /// AC2: patterns outside the three shapes are refused.
    #[test]
    fn patterns_outside_the_three_shapes_are_refused() {
        for bad in ["a*b", "**/x", "", "  ", "*", "*a*", "a/*.snap", "fix*/", "a/b/", "/"] {
            assert!(parse_pattern(bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["fixtures/", "*.snap", "golden_*", "Makefile", "data/expected.json"] {
            assert!(parse_pattern(good).is_ok(), "{good:?} must be accepted");
        }
    }

    /// AC9: how the stems are taken from the checks' commands.
    #[test]
    fn check_stems_come_from_every_word_that_is_not_an_option() {
        let checks = [
            check("argv", "cargo clippy --all-targets -- -D warnings", &[]),
            check("argv", "python scripts/test_public_tree.py", &[]),
            check("argv", r#""./my tool.exe" --version"#, &[]),
            check("cmd", r"tools\Build.CMD .", &[]),
        ];
        assert_eq!(
            check_stems(&checks),
            ["build", "cargo", "clippy", "my tool", "python", "test_public_tree", "warnings"]
        );
        // a non-argv shell splits on whitespace only
        assert_eq!(check_stems(&[check("sh", "make  all", &[])]), ["all", "make"]);
    }

    /// One row of AC9's table: the checks' commands, one diff entry, and what
    /// should be listed for it (`None` = nothing).
    struct Ac9Row {
        checks: &'static [(&'static str, &'static str)],
        letter: char,
        path: &'static str,
        listed: Option<(&'static str, &'static str, &'static str)>,
    }

    /// AC9's table: every case, each with its own checks, run by one loop.
    #[test]
    fn ac9_table_of_check_word_cases() {
        const CARGO_AND_PYTHON: &[(&str, &str)] =
            &[("argv", "cargo test"), ("argv", "python scripts/test_public_tree.py")];
        const MY_TOOL: &[(&str, &str)] = &[("argv", r#""./my tool.exe" --version"#)];
        let row = |checks, letter, path, listed| Ac9Row { checks, letter, path, listed };
        let table = [
            row(CARGO_AND_PYTHON, 'A', "cargo.bat", Some(("cargo.bat", "added", "check_word"))),
            row(CARGO_AND_PYTHON, 'A', "tools/Cargo.cmd", Some(("tools/Cargo.cmd", "added", "check_word"))),
            row(CARGO_AND_PYTHON, 'A', "src/cargo_helper.rs", None),
            row(
                CARGO_AND_PYTHON,
                'A',
                "x/test_public_tree.sh",
                Some(("x/test_public_tree.sh", "added", "check_word")),
            ),
            row(CARGO_AND_PYTHON, 'A', "docs/python.md", Some(("docs/python.md", "added", "check_word"))),
            row(CARGO_AND_PYTHON, 'A', "tests/new_case.rs", None),
            row(
                CARGO_AND_PYTHON,
                'M',
                "tools/cargo.toml.bak",
                Some(("tools/cargo.toml.bak", "modified", "check_word")),
            ),
            row(MY_TOOL, 'A', "my tool.cmd", Some(("my tool.cmd", "added", "check_word"))),
        ];
        assert_eq!(table.len(), 8);
        for r in &table {
            let checks: Vec<Check> = r.checks.iter().map(|(sh, cmd)| check(sh, cmd, &[])).collect();
            let cat = Catalogue::from_parts(vec![], &checks);
            let got = changes(&cat, &[(r.letter, r.path.to_string())]);
            match r.listed {
                None => assert!(got.is_empty(), "{} must not be listed: {got:?}", r.path),
                Some((path, status, kind)) => {
                    assert_eq!(got.len(), 1, "{} must be listed exactly once", r.path);
                    assert_eq!(got[0].path, path, "{}", r.path);
                    assert_eq!(got[0].status, status, "{}", r.path);
                    assert_eq!(got[0].kind, kind, "{}", r.path);
                }
            }
        }
    }

    /// AC9: an added test file is listed only when its name is also a check
    /// word, and then as `check_word`.
    #[test]
    fn an_added_test_file_counts_only_when_it_is_also_a_check_word() {
        let cat = Catalogue::from_parts(vec![], &[check("argv", "cargo test", &["Cargo.toml"])]);
        let got = changes(
            &cat,
            &[
                ('A', "tests/cargo.rs".to_string()),
                ('A', "tests/other.rs".to_string()),
                ('M', "tests/cargo.rs2".to_string()),
                ('D', "Cargo.toml".to_string()),
                ('T', "tests/link.rs".to_string()),
                ('M', "src/main.rs".to_string()),
            ],
        );
        let rows: Vec<(&str, &str, &str)> = got
            .iter()
            .map(|c| (c.path.as_str(), c.status.as_str(), c.kind.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Cargo.toml", "deleted", "check_input"),
                ("tests/cargo.rs", "added", "check_word"),
                ("tests/cargo.rs2", "modified", "test"),
                ("tests/link.rs", "type_changed", "test"),
            ],
            "sorted by path"
        );
    }

    /// AC4: what counts as declaring a path.
    #[test]
    fn a_claim_declares_a_path_only_when_its_text_starts_with_exactly_that_path() {
        assert!(claim_declares("tests/x.rs: the old assertion contradicted AC2", "tests/x.rs"));
        assert!(claim_declares("  tests/x.rs because of AC2", "tests/x.rs"));
        assert!(claim_declares("tests/x.rs", "tests/x.rs"));
        assert!(claim_declares(r"tests\x.rs: windows separators", "tests/x.rs"));
        assert!(!claim_declares("tests/x.rs2: another file", "tests/x.rs"));
        assert!(!claim_declares("x.rs: not the whole path", "tests/x.rs"));
        assert!(!claim_declares("changed tests/x.rs: path not first", "tests/x.rs"));
    }

    #[test]
    fn patterns_round_trip_through_meta_unique_and_sorted() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        assert!(load_patterns(&conn).unwrap().is_empty());
        let stored = store_patterns(&conn, vec!["b/".into(), "*.snap".into(), "b/".into()]).unwrap();
        assert_eq!(stored, ["*.snap", "b/"]);
        assert_eq!(load_patterns(&conn).unwrap(), ["*.snap", "b/"]);
        let raw = crate::db::state::get_meta(&conn, META_KEY).unwrap().unwrap();
        assert_eq!(raw, r#"["*.snap","b/"]"#);
    }

    #[test]
    fn changes_in_detail_reads_back_what_to_json_wrote() {
        let c = Change { path: "tests/x.rs".into(), status: "modified".into(), kind: "test".into() };
        let detail = json!({"protected_changes": [c.to_json()]});
        assert_eq!(changes_in_detail(&detail), [c]);
        assert!(changes_in_detail(&json!({"files_changed": []})).is_empty());
    }

    fn change(path: &str, status: &str, kind: &str) -> Change {
        Change { path: path.into(), status: status.into(), kind: kind.into() }
    }

    #[test]
    fn uncovered_keeps_unapproved_test_and_check_word_changes_only() {
        let changes = [
            change("Cargo.toml", "modified", "check_input"),
            change("tests/a.rs", "modified", "test"),
            change("tests/b.rs", "modified", "test"),
            change("tests/c.rs", "deleted", "test"),
            change("cargo.bat", "added", "check_word"),
        ];
        let approved = vec![
            ("tests/a.rs".to_string(), "modified".to_string()),
            ("tests/c.rs".to_string(), "modified".to_string()),
        ];
        assert_eq!(
            uncovered(&changes, &approved),
            [
                change("tests/b.rs", "modified", "test"),
                change("tests/c.rs", "deleted", "test"),
                change("cargo.bat", "added", "check_word"),
            ]
        );
        assert!(uncovered(&changes[..1], &[]).is_empty());
    }

    #[test]
    fn strict_changes_treats_a_missing_or_broken_list_as_unknown() {
        let good = json!({"protected_changes": [
            {"path": "tests/a.rs", "status": "modified", "kind": "test"},
            {"path": "Cargo.toml", "status": "modified", "kind": "check_input"},
        ]});
        assert_eq!(
            strict_changes(&good),
            Some(vec![
                change("tests/a.rs", "modified", "test"),
                change("Cargo.toml", "modified", "check_input"),
            ])
        );
        assert_eq!(strict_changes(&json!({"protected_changes": []})), Some(vec![]));
        assert_eq!(strict_changes(&json!({"files_changed": []})), None);
        assert_eq!(strict_changes(&json!({"protected_changes": "tests/a.rs"})), None);
        assert_eq!(
            strict_changes(&json!({"protected_changes": [
                {"path": "tests/a.rs", "status": "modified", "kind": "test"},
                {"path": "tests/b.rs", "kind": "test"},
            ]})),
            None
        );
        assert_eq!(
            strict_changes(&json!({"protected_changes": [
                {"path": "tests/a.rs", "status": "modified", "kind": "golden"},
            ]})),
            None
        );
        assert_eq!(strict_changes(&Value::Null), None);
    }

    /// SMALL-4 AC1 (u1).
    #[test]
    fn attribution_keeps_only_what_differs_from_the_integration_tip() {
        let own: Vec<(char, String)> = vec![
            ('M', "tests/pre_test.rs".to_string()),
            ('A', "tests/new_test.rs".to_string()),
            ('M', "src/x.rs".to_string()),
            ('D', "data/g.json".to_string()),
        ];
        let tip: Vec<String> = vec!["tests/new_test.rs".to_string(), "src/x.rs".to_string()];
        let expected: Vec<(char, String)> =
            vec![('A', "tests/new_test.rs".to_string()), ('M', "src/x.rs".to_string())];
        assert_eq!(attributed(&own, Some(tip.as_slice())), expected);
        assert_eq!(attributed(&own, None), own);
        assert!(attributed(&own, Some(&[][..])).is_empty());
        let a: Vec<String> = vec!["a".to_string()];
        assert!(attributed(&[], Some(a.as_slice())).is_empty());
    }
}
