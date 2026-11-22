//! `thoth spec lint FILE` (LESS-3): fixed, hand-written rules for the mistakes
//! spec reviews keep finding. `task create`, `task update-spec` and `spec
//! review start` run the same lint first: an error refuses, a warning does not.
//!
//! The rules would rather miss a problem than report one that is not there:
//! anything they do not describe is left to the spec review.

use crate::config::Paths;
use crate::output;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Over this many bytes a spec is refused at every risk: the task should be
/// split.
pub const TOO_LARGE_BYTES: usize = 40960;
/// Over this many bytes (up to `TOO_LARGE_BYTES`) a spec is `large`: an error
/// at low and normal risk, a warning at important and high (LEAN-1).
pub const LARGE_BYTES: usize = 20480;

/// LEAN-1: the risks at which `large` is an error. They skip the mandatory
/// first-dispatch spec review, so their specs are kept small instead; the
/// gated risks (`spec_review::gated_risk`) are reviewed, and only warned.
const SMALL_SPEC_RISKS: [&str; 2] = ["low", "normal"];

/// The risk `large` is judged at: the spec's own `risk`, `normal` when it has
/// none, and `normal` too when the spec cannot be read (no risk is readable,
/// so the default applies).
fn size_risk(raw: &str) -> String {
    crate::spec::parse_spec(raw).map(|s| s.risk).unwrap_or_else(|_| "normal".to_string())
}

const WRITE_WORDS_ZH: &[&str] = &["改", "加", "寫入", "寫進", "新增", "建立", "刪", "移除", "搬", "覆蓋", "編輯"];
const WRITE_WORDS_EN: &[&str] = &[
    "write", "edit", "modify", "change", "update", "create", "add", "delete", "remove", "move",
    "rename", "replace",
];
const NEGATIONS_ZH: &[&str] = &["不", "別", "勿", "禁", "免", "無", "沒", "只讀", "唯讀", "取自"];
const NEGATIONS_EN: &[&str] =
    &["not", "never", "no", "don't", "without", "read-only", "readonly", "read only"];
/// SPECLINT-1: the adding words of a list entry (`新增：docs/、.claude/`).
const ADDING_ZH: &[&str] = &["新增", "加入"];
const ADDING_EN: &[&str] = &["add"];
const LINE_NUMBER_EXTENSIONS: &[&str] = &[
    "rs", "md", "json", "toml", "lock", "ts", "js", "py", "sh", "ps1", "txt", "yml", "yaml",
];
const CMDLET_VERBS: &[&str] = &[
    "Get", "Set", "New", "Remove", "Select", "Test", "Invoke", "Write", "Out", "Start", "Stop",
    "Copy", "Move", "ForEach", "Where",
];

pub struct Finding {
    pub rule: &'static str,
    pub location: Option<String>,
    pub message: String,
}

impl Finding {
    fn new(rule: &'static str, location: Option<&str>, message: String) -> Self {
        Self { rule, location: location.map(str::to_string), message }
    }

    /// `RULE (LOCATION): MESSAGE`, or `RULE: MESSAGE` without a location.
    pub fn text(&self) -> String {
        match &self.location {
            Some(l) => format!("{} ({l}): {}", self.rule, self.message),
            None => format!("{}: {}", self.rule, self.message),
        }
    }

    fn json(&self) -> Value {
        json!({"rule": self.rule, "location": self.location, "message": self.message})
    }
}

pub struct Report {
    pub bytes: usize,
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
}

impl Report {
    pub fn json(&self) -> Value {
        json!({
            "ok": self.errors.is_empty(),
            "bytes": self.bytes,
            "errors": self.errors.iter().map(Finding::json).collect::<Vec<_>>(),
            "warnings": self.warnings.iter().map(Finding::json).collect::<Vec<_>>(),
        })
    }

    /// What `task create`, `task update-spec` and `spec review start` refuse
    /// with when there is an error.
    pub fn refusal(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        let texts: Vec<String> = self.errors.iter().map(Finding::text).collect();
        Some(format!(
            "spec lint: {} blocking problem(s): {}",
            self.errors.len(),
            texts.join("; ")
        ))
    }

    /// One line per warning, for the `warnings` of those three commands.
    pub fn warning_lines(&self) -> Vec<String> {
        self.warnings.iter().map(|w| format!("spec lint: {}", w.text())).collect()
    }

    fn human(&self) -> String {
        if self.errors.is_empty() && self.warnings.is_empty() {
            return "no problems found".to_string();
        }
        let mut lines: Vec<String> = self.errors.iter().map(|e| format!("error: {}", e.text())).collect();
        lines.extend(self.warnings.iter().map(|w| format!("warning: {}", w.text())));
        lines.push(format!("{} error(s), {} warning(s)", self.errors.len(), self.warnings.len()));
        lines.join("\n")
    }
}

/// The lint as the commands run it: every check id in the catalogue,
/// approved or not, and the main work tree.
pub fn for_repo(conn: &rusqlite::Connection, root: &Path, raw: &str) -> Result<Report> {
    let ids: Vec<String> = crate::checks::list_checks(conn)?
        .into_iter()
        .map(|c| c.check_id)
        .collect();
    Ok(lint(raw, root, &ids))
}

/// `thoth spec lint FILE`: exit 1 when there is an error, with the whole
/// report on stdout either way.
pub fn run(paths: &Paths, cwd: &Path, file: &Path, json: bool) -> Result<i32> {
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("reading spec {}", file.display()))?;
    let report = for_repo(&conn, &root, &raw)?;
    output::emit(json, &report.json(), &report.human());
    Ok(if report.errors.is_empty() { 0 } else { 1 })
}

/// Test seam, debug builds only, so a release build has no such switch and
/// ignores the variable. When `THOTH_TEST_SPEC_LINT_PRE_LEAN` is set, a spec
/// over `LARGE_BYTES` (and up to `TOO_LARGE_BYTES`) at risk low or normal gets
/// the `large` finding as a warning, as before LEAN-1, so a test can store
/// one through the CLI and check how the later commands refuse it.
#[cfg(debug_assertions)]
fn pre_lean_large_is_a_warning() -> bool {
    std::env::var_os("THOTH_TEST_SPEC_LINT_PRE_LEAN").is_some()
}

#[cfg(not(debug_assertions))]
fn pre_lean_large_is_a_warning() -> bool {
    false
}

/// Reads only files under `root`; writes nothing.
pub fn lint(raw: &str, root: &Path, check_ids: &[String]) -> Report {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let bytes = raw.len();
    if bytes > TOO_LARGE_BYTES {
        errors.push(Finding::new(
            "too_large",
            None,
            format!("the spec is {bytes} bytes, over the {TOO_LARGE_BYTES}-byte limit; split the task into smaller tasks"),
        ));
    } else if bytes > LARGE_BYTES {
        let risk = size_risk(raw);
        if SMALL_SPEC_RISKS.contains(&risk.as_str()) && !pre_lean_large_is_a_warning() {
            errors.push(Finding::new(
                "large",
                None,
                format!(
                    "the spec is {bytes} bytes, over the {LARGE_BYTES}-byte limit for risk {risk}: \
                     low and normal risk tasks skip the mandatory spec review, so their specs must \
                     stay at or under {LARGE_BYTES} bytes; shrink the spec or split the task"
                ),
            ));
        } else {
            warnings.push(Finding::new(
                "large",
                None,
                format!("the spec is {bytes} bytes, over {LARGE_BYTES}; consider splitting the task"),
            ));
        }
    }
    let spec = match crate::spec::parse_spec(raw) {
        Ok(s) => s,
        Err(e) => {
            errors.push(Finding::new("invalid_spec", None, e.to_string()));
            return Report { bytes, errors, warnings };
        }
    };

    let mut seen: Vec<(String, usize)> = Vec::new();
    for (i, ac) in spec.acceptance_criteria.iter().enumerate() {
        let location = format!("acceptance_criteria[{i}]");
        let normal = ac.split_whitespace().collect::<Vec<_>>().join(" ");
        if normal.is_empty() {
            errors.push(Finding::new("ac_blank", Some(&location), "the acceptance criterion is blank".into()));
        } else if let Some((_, first)) = seen.iter().find(|(n, _)| *n == normal) {
            errors.push(Finding::new(
                "ac_duplicate",
                Some(&location),
                format!("same text as acceptance_criteria[{first}] once spaces are trimmed and collapsed"),
            ));
        } else {
            seen.push((normal, i));
        }
    }
    if !check_ids.is_empty() {
        for (i, id) in spec.checks.iter().enumerate() {
            if !check_ids.contains(id) {
                errors.push(Finding::new(
                    "unknown_check",
                    Some(&format!("checks[{i}]")),
                    format!(
                        "check '{id}' is not in this repository's catalogue ({}); propose it with thoth checks add, or drop it from the spec",
                        check_ids.join(", ")
                    ),
                ));
            }
        }
    }
    // SMALL-11 (3): verify runs every approved check of the repository for a
    // spec that lists none, which is rarely what a docs or research task
    // wants. Only a warning: what verify runs does not change.
    if spec.checks.is_empty() && !check_ids.is_empty() {
        if let Some(task_type) = crate::spec::labels(raw)
            .0
            .filter(|t| t == "docs" || t == "research")
        {
            warnings.push(Finding::new(
                "no_checks_for_document_task",
                Some("checks"),
                format!(
                    "the spec's task_type is {task_type} and it lists no checks, so verify will run \
                     every approved check of the repository; list the checks the task needs in checks"
                ),
            ));
        }
    }

    let mut fields = vec![Field::new("goal".into(), &spec.goal)];
    for (i, ac) in spec.acceptance_criteria.iter().enumerate() {
        fields.push(Field::new(format!("acceptance_criteria[{i}]"), ac));
    }
    for (i, c) in spec.constraints.iter().enumerate() {
        fields.push(Field::new(format!("constraints[{i}]"), c));
    }
    errors.extend(fields.iter().filter_map(claude_write));
    errors.extend(unknown_test_targets(&fields, root));
    warnings.extend(missing_paths(&fields, root));
    warnings.extend(missing_names(&fields, root));
    for f in &fields {
        warnings.extend(line_number(f));
        warnings.extend(two_dot_diff(f));
        warnings.extend(powershell_in_bash(f));
    }
    Report { bytes, errors, warnings }
}

/// One scanned field, as characters, with its backtick fragments marked.
struct Field {
    location: String,
    chars: Vec<char>,
    /// True for each backtick of a pair and every character between them.
    quoted: Vec<bool>,
    /// The characters between each pair of backticks.
    fragments: Vec<Range<usize>>,
}

impl Field {
    fn new(location: String, text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut quoted = vec![false; chars.len()];
        let mut fragments = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '`' {
                if let Some(off) = chars[i + 1..].iter().position(|&c| c == '`') {
                    let close = i + 1 + off;
                    quoted[i..=close].iter_mut().for_each(|q| *q = true);
                    fragments.push(i + 1..close);
                    i = close + 1;
                    continue;
                }
            }
            i += 1;
        }
        Self { location, chars, quoted, fragments }
    }

    fn text(&self, r: Range<usize>) -> String {
        self.chars[r].iter().collect()
    }

    /// The sentence pieces, split at `。`, `；`, `;`, a newline and `. `; a
    /// separator inside a pair of backticks is part of the quotation.
    fn segments(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let (mut start, mut i) = (0, 0);
        while i < self.chars.len() {
            let width = match self.chars[i] {
                _ if self.quoted[i] => 0,
                '。' | '；' | ';' | '\n' => 1,
                '.' if self.chars.get(i + 1) == Some(&' ') => 2,
                _ => 0,
            };
            if width > 0 {
                out.push(start..i);
                i += width;
                start = i;
            } else {
                i += 1;
            }
        }
        out.push(start..self.chars.len());
        out
    }

    /// SPECLINT-1: the clauses of one sentence, split at `，` and at an ASCII
    /// `,` that is followed by a space or the end of the field (`1,000` and
    /// `a,b` stay whole). `、` does not split, and nothing splits inside
    /// backticks.
    fn clauses(&self, sentence: Range<usize>) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut start = sentence.start;
        for i in sentence.clone() {
            let splits = !self.quoted[i]
                && match self.chars[i] {
                    '，' => true,
                    ',' => self.chars.get(i + 1).is_none_or(|&c| c == ' '),
                    _ => false,
                };
            if splits {
                out.push(start..i);
                start = i + 1;
            }
        }
        out.push(start..sentence.end);
        out
    }

    /// The matched pairs of parentheses (`（…）`, `(…)`) of a sentence, as the
    /// positions of the opener and the closer; backticks are skipped and an
    /// opener with no closer is ignored.
    fn asides(&self, sentence: Range<usize>) -> Vec<(usize, usize)> {
        let mut open = Vec::new();
        let mut out = Vec::new();
        for i in sentence.filter(|&i| !self.quoted[i]) {
            match self.chars[i] {
                '(' | '（' => open.push(i),
                ')' | '）' => out.extend(open.pop().map(|o| (o, i))),
                _ => {}
            }
        }
        out
    }

    /// `r` with every backtick fragment blanked out.
    fn unquoted(&self, r: Range<usize>) -> String {
        r.map(|i| if self.quoted[i] { ' ' } else { self.chars[i] }).collect()
    }

    fn location(&self) -> Option<&str> {
        Some(&self.location)
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `text` holds the ASCII `word` in any case, with no letter, digit
/// or `_` on either side.
fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(off) = lower[from..].find(word) {
        let s = from + off;
        let e = s + word.len();
        if (s == 0 || !is_word_byte(bytes[s - 1])) && (e == bytes.len() || !is_word_byte(bytes[e])) {
            return true;
        }
        from = s + 1;
    }
    false
}

/// Whether `text` holds `name` with no `[A-Za-z0-9_]` on either side.
fn has_identifier(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(off) = text[from..].find(name) {
        let s = from + off;
        let e = s + name.len();
        if (s == 0 || !is_word_byte(bytes[s - 1])) && (e == bytes.len() || !is_word_byte(bytes[e])) {
            return true;
        }
        from = s + 1;
    }
    false
}

/// Where `.claude` stands as a folder name in `chars`: `~/.claude/x` and
/// `.claude/skills` count, `.claude-plugin`, `.claude.json` and `CLAUDE.md`
/// do not.
fn claude_dir_starts(chars: &[char]) -> Vec<usize> {
    let pat: Vec<char> = ".claude".chars().collect();
    (0..chars.len())
        .filter(|&i| {
            chars[i..].starts_with(&pat)
                && (i == 0 || !chars[i - 1].is_ascii_alphanumeric())
                && chars
                    .get(i + pat.len())
                    .is_none_or(|&c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        })
        .collect()
}

const CLAUDE_DIR_LEN: usize = 7;

fn has_write_word(plain: &str) -> bool {
    WRITE_WORDS_ZH.iter().any(|w| plain.contains(w)) || WRITE_WORDS_EN.iter().any(|w| has_word(plain, w))
}

fn has_negation(text: &str) -> bool {
    NEGATIONS_ZH.iter().any(|w| text.contains(w)) || NEGATIONS_EN.iter().any(|w| has_word(text, w))
}

/// A negation word other than the read-only markers, which count only in
/// the clause where they stand (`改 .claude/x，內容取自 a.md` is still a write).
fn has_ban(text: &str) -> bool {
    let markers = ["只讀", "唯讀", "取自", "read-only", "readonly", "read only"];
    NEGATIONS_ZH.iter().chain(NEGATIONS_EN).filter(|w| !markers.contains(w)).any(|w| {
        if w.is_ascii() { has_word(text, w) } else { text.contains(w) }
    })
}

/// `plain` with each adding word that is directly followed by a colon (spaces
/// allowed before it) blanked out, and whether there was one.
fn without_adding_words(plain: &str) -> (String, bool) {
    let chars: Vec<char> = plain.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = chars.clone();
    let mut found = false;
    for (word, ascii) in ADDING_ZH.iter().map(|w| (w, false)).chain(ADDING_EN.iter().map(|w| (w, true))) {
        let w: Vec<char> = word.chars().collect();
        let mut i = 0;
        while i + w.len() <= lower.len() {
            let whole = !ascii
                || ((i == 0 || !lower[i - 1].is_ascii_alphanumeric() && lower[i - 1] != '_')
                    && lower.get(i + w.len()).is_none_or(|c| !c.is_ascii_alphanumeric() && *c != '_'));
            if lower[i..].starts_with(&w) && whole {
                let mut j = i + w.len();
                while chars.get(j) == Some(&' ') {
                    j += 1;
                }
                if matches!(chars.get(j), Some('：') | Some(':')) {
                    out[i..i + w.len()].iter_mut().for_each(|c| *c = ' ');
                    found = true;
                }
            }
            i += 1;
        }
    }
    (out.into_iter().collect(), found)
}

/// What the rule knows about one clause.
struct ClauseFacts {
    range: Range<usize>,
    /// Where each `.claude` name of the clause starts, except a name inside a
    /// quoted ban (a backtick fragment with whitespace and a negation word).
    targets: Vec<usize>,
    /// REVFIX-1: the clause names another path or a file name
    /// (`Field::names_path`).
    names_path: bool,
    writes: bool,
    negated: bool,
    /// An adding word with a colon and no other write word: `新增：…`.
    list_head: bool,
}

impl ClauseFacts {
    /// REVFIX-1: a `.claude` name outside a quoted ban, or another path or
    /// file name.
    fn has_target(&self) -> bool {
        !self.targets.is_empty() || self.names_path
    }
}

impl Field {
    fn clause_facts(&self, range: Range<usize>) -> ClauseFacts {
        let plain = self.unquoted(range.clone());
        let targets = claude_dir_starts(&self.chars[range.clone()])
            .into_iter()
            .map(|s| s + range.start)
            .filter(|&s| {
                !self.fragments.iter().any(|fr| {
                    let t = self.text(fr.clone());
                    fr.contains(&s) && t.chars().any(char::is_whitespace) && has_negation(&t)
                })
            })
            .collect();
        let writes = has_write_word(&plain);
        let (rest, adding) = without_adding_words(&plain);
        let names_path = self.names_path(range.clone());
        ClauseFacts {
            range,
            targets,
            names_path,
            writes,
            negated: has_negation(&plain),
            list_head: adding && !has_write_word(&rest),
        }
    }

    /// REVFIX-1: whether `range` names a path or a file name other than a
    /// `.claude` name: a word (a run of ASCII characters other than spaces,
    /// backticks, quotes, brackets and `,;`), inside or outside backticks,
    /// that holds a `/` or a backslash, or that ends, once trailing dots are
    /// dropped, in a dot followed by letters or digits (`a.txt`, `Cargo.toml`).
    /// A word with a `.claude` folder name in it is left to `targets`, so a
    /// name inside a quoted ban is not a target here either.
    fn names_path(&self, range: Range<usize>) -> bool {
        let is_word_char = |c: char| {
            c.is_ascii_graphic() && !matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';')
        };
        let mut i = range.start;
        while i < range.end {
            if !is_word_char(self.chars[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < range.end && is_word_char(self.chars[i]) {
                i += 1;
            }
            let word = &self.chars[start..i];
            if !claude_dir_starts(word).is_empty() {
                continue;
            }
            let mut trimmed = word;
            while let [rest @ .., '.' | ':' | '!' | '?'] = trimmed {
                trimmed = rest;
            }
            let ends_in_extension = trimmed.iter().rposition(|&c| c == '.').is_some_and(|dot| {
                dot + 1 < trimmed.len() && trimmed[dot + 1..].iter().all(char::is_ascii_alphanumeric)
            });
            if word.iter().any(|&c| c == '/' || c == '\\') || ends_in_extension {
                return true;
            }
        }
        false
    }

    /// Whether the name at `start` is a bare `.claude` or `.claude/` with
    /// another entry of a list beside it (a `、`, `，` or `,` before or after).
    fn bare_list_entry(&self, clause: &Range<usize>, sentence: &Range<usize>, start: usize) -> bool {
        let mut end = start + CLAUDE_DIR_LEN;
        if self.chars.get(end) == Some(&'/') && end < clause.end {
            end += 1;
        }
        let is_sep = |c: char| matches!(c, '、' | '，' | ',');
        let ends_clean = end >= clause.end || self.chars[end].is_whitespace() || is_sep(self.chars[end]) || self.chars[end] == '`';
        let starts_clean = start == clause.start
            || matches!(self.chars[start - 1], '、' | '，' | ',' | '：' | ':' | '`')
            || self.chars[start - 1].is_whitespace();
        if !(ends_clean && starts_clean) {
            return false;
        }
        let mut b = start;
        if b > sentence.start && self.chars[b - 1] == '`' {
            b -= 1;
        }
        while b > sentence.start && self.chars[b - 1].is_whitespace() {
            b -= 1;
        }
        let mut a = end;
        if a < sentence.end && self.chars[a] == '`' {
            a += 1;
        }
        while a < sentence.end && self.chars[a].is_whitespace() {
            a += 1;
        }
        (b > sentence.start && is_sep(self.chars[b - 1])) || (a < sentence.end && is_sep(self.chars[a]))
    }

    /// Whether the clause starts with its first `.claude` name, after spaces,
    /// a backtick, and one of `和`, `與`, `及`, `and`, `or`.
    fn starts_with_name(&self, facts: &ClauseFacts) -> bool {
        let Some(&first) = facts.targets.first() else {
            return false;
        };
        let r = &facts.range;
        let skip_spaces = |mut i: usize| {
            while i < r.end && self.chars[i].is_whitespace() {
                i += 1;
            }
            i
        };
        let skip_tick = |i: usize| if i < r.end && self.chars[i] == '`' { i + 1 } else { i };
        let mut i = skip_spaces(r.start);
        i = skip_spaces(skip_tick(i));
        if i < r.end && matches!(self.chars[i], '和' | '與' | '及') {
            i = skip_spaces(i + 1);
        } else {
            let word: String = self.chars[i..r.end].iter().take(3).collect::<String>().to_ascii_lowercase();
            for w in ["and", "or"] {
                let after = self.chars.get(i + w.len()).filter(|_| i + w.len() < r.end);
                if word.starts_with(w) && after.is_some_and(|c| c.is_whitespace() || *c == '`') {
                    i = skip_spaces(i + w.len());
                    break;
                }
            }
        }
        i = skip_tick(i);
        if i < r.end && self.chars[i] == '~' && self.chars.get(i + 1) == Some(&'/') {
            i += 2;
        }
        i == first
    }
}

/// SPECLINT-1: for each clause, whether it or a clause that shares a
/// parenthetical aside with it has a negation. A clause that opens an aside
/// and the clauses up to the one that closes it are one unit for the
/// negation only (`不改 a、b（含 c，由指揮官改）、.claude/`): the write words and
/// the names stay judged clause by clause.
fn shared_negation(f: &Field, sentence: &Range<usize>, clauses: &[ClauseFacts]) -> Vec<bool> {
    let clause_of = |pos: usize| clauses.iter().position(|c| c.range.contains(&pos) || c.range.end == pos);
    // linked[k]: clause k and clause k + 1 are inside one aside
    let mut linked = vec![false; clauses.len()];
    for (o, c) in f.asides(sentence.clone()) {
        if let (Some(i), Some(j)) = (clause_of(o), clause_of(c)) {
            linked[i..j.max(i)].iter_mut().for_each(|l| *l = true);
        }
    }
    let mut out = vec![false; clauses.len()];
    let mut k = 0;
    while k < clauses.len() {
        let mut end = k;
        while end < clauses.len() - 1 && linked[end] {
            end += 1;
        }
        let any = clauses[k..=end].iter().any(|c| c.negated);
        out[k..=end].iter_mut().for_each(|o| *o = any);
        k = end + 1;
    }
    out
}

fn claude_write(f: &Field) -> Option<Finding> {
    for sentence in f.segments() {
        let facts: Vec<ClauseFacts> =
            f.clauses(sentence.clone()).into_iter().map(|r| f.clause_facts(r)).collect();
        let mut negated = shared_negation(f, &sentence, &facts);
        // A trailing negation clause (`…由指揮官之後自己改，這件不碰`): a negation
        // word, no name and no write word of its own, last in the sentence. It
        // covers the clauses before it; with a write word of its own
        // (`改 .claude/x，不改 src/a.rs`) it is a different instruction.
        if let [before @ .., last] = facts.as_slice() {
            if has_ban(&f.unquoted(last.range.clone())) && last.targets.is_empty() && !last.writes {
                negated[..before.len()].iter_mut().for_each(|n| *n = true);
            }
        }
        // (the verdict of the clause before, whether it began a list entry)
        let mut before: Option<(bool, bool)> = None;
        for (k, clause) in facts.iter().enumerate() {
            let range = clause.range.clone();
            let all_bare = !clause.targets.is_empty()
                && clause.targets.iter().all(|&s| f.bare_list_entry(&range, &sentence, s));
            let continuation = before.is_some() && !clause.writes && f.starts_with_name(clause);
            let (refused, active, list) = match before {
                Some((active, list)) if continuation => {
                    (active && !negated[k] && !(list && all_bare), active && !negated[k], list)
                }
                _ => {
                    let active = clause.writes && !negated[k];
                    let refused = active && !clause.targets.is_empty() && !(clause.list_head && all_bare);
                    (refused, active, clause.list_head)
                }
            };
            before = Some((active, list));
            if refused {
                return Some(claude_write_finding(f, range));
            }
            // REVFIX-1: a write with no target of its own writes to the target
            // of the nearest earlier clause of the sentence that has one
            // (`在 .claude/x 裡，把第一段改掉`). When that clause names a `.claude`
            // folder and neither writes nor is negated itself, the sentence is
            // refused as if the write stood there.
            if clause.writes && !negated[k] && !clause.has_target() {
                let nearest = (0..k).rev().find(|&j| facts[j].has_target());
                if let Some(j) = nearest {
                    let folder = &facts[j];
                    if !folder.targets.is_empty() && !folder.writes && !negated[j] {
                        return Some(claude_write_finding(f, folder.range.start..range.end));
                    }
                }
            }
        }
    }
    None
}

/// The `claude_write` finding, quoting `quoted` of the field.
fn claude_write_finding(f: &Field, quoted: Range<usize>) -> Finding {
    Finding::new(
        "claude_write",
        f.location(),
        format!(
            "asks to write under a .claude folder (\"{}\"); a worker must not change .claude, so do that step yourself or leave it out",
            f.text(quoted).trim()
        ),
    )
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/')
}

/// A path the spec names: where it starts in the field, where it ends, and
/// its text with the trailing `.` removed.
struct PathRef {
    start: usize,
    end: usize,
    path: String,
}

impl PathRef {
    /// SMALL-17 (b): a path with a `*` in it is a glob.
    fn is_glob(&self) -> bool {
        self.path.contains('*')
    }

    /// SMALL-17 (b): whether `missing_path` checks it: a path whose last part
    /// has no dot and that does not end in `/` (`plan/log`, `docs/low`) is
    /// more often a noun than a path, and is checked only when it is the
    /// whole text of a backtick fragment.
    fn checked(&self, f: &Field) -> bool {
        let last = self.path.rsplit('/').next().unwrap_or("");
        self.path.ends_with('/')
            || last.contains('.')
            || f.fragments.iter().any(|fr| fr.start == self.start && fr.end == self.end)
    }
}

/// Every run of path characters with a `/` in it that could be a path of
/// this repository: not after `$~%\:`, not absolute or relative to `.`/`..`,
/// no `//`, `...` or `..` part. SMALL-17 (b): a run directly followed by `*`
/// goes on over the `*` and the path characters after it, and is a glob.
fn path_refs(f: &Field) -> Vec<PathRef> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < f.chars.len() {
        if !is_path_char(f.chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < f.chars.len() && (is_path_char(f.chars[i]) || f.chars[i] == '*') {
            i += 1;
        }
        let token: String = f.chars[start..i].iter().collect();
        let path = token.trim_end_matches('.');
        if !path.contains('/')
            || (start > 0 && matches!(f.chars[start - 1], '$' | '~' | '%' | '\\' | ':'))
            || path.starts_with('/')
            || path.starts_with("./")
            || path.starts_with("../")
            || token.contains("//")
            || token.contains("...")
            || path.split('/').any(|part| part == "..")
        {
            continue;
        }
        out.push(PathRef { start, end: start + path.chars().count(), path: path.to_string() });
    }
    out
}

/// `root` joined with each part of a `/`-separated relative path.
fn under(root: &Path, path: &str) -> PathBuf {
    path.split('/').filter(|p| !p.is_empty()).fold(root.to_path_buf(), |acc, p| acc.join(p))
}

/// SMALL-17 (b): whether `name` matches `pattern`, where `*` matches any run
/// of characters (inside one path part: the caller matches part by part).
fn wildcard_match(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|k| wildcard_match(rest, &name[k..])),
        Some((c, rest)) => name.first() == Some(c) && wildcard_match(rest, &name[1..]),
    }
}

/// SMALL-17 (b): whether anything under `dir` matches the `/`-separated
/// `parts`, a directory when `dir_wanted` and it is the last part.
fn glob_exists(dir: &Path, parts: &[&str], dir_wanted: bool) -> bool {
    let Some((part, rest)) = parts.split_first() else {
        return if dir_wanted { dir.is_dir() } else { dir.exists() };
    };
    if !part.contains('*') {
        return glob_exists(&dir.join(part), rest, dir_wanted);
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let pattern: Vec<char> = part.chars().collect();
    entries.flatten().any(|e| {
        let name: Vec<char> = e.file_name().to_string_lossy().chars().collect();
        wildcard_match(&pattern, &name) && glob_exists(&e.path(), rest, dir_wanted)
    })
}

/// SMALL-17 (b): the words that may stand between a new marker and its path
/// (`new test files P`, `新檔 P`).
const NEW_MARKER_WORDS: &[&str] = &[
    "test", "tests", "file", "files", "module", "modules", "fixture", "fixtures", "script", "scripts",
    "folder", "檔", "檔案", "測試檔", "模組",
];

/// SMALL-17 (b): the words that may join two paths of one list.
const LIST_WORDS: &[&str] = &["and", "or", "與", "和", "及", "或"];

/// Whether `text` is only `words` (ASCII words in any case), with spaces,
/// backticks and the characters `extra_separators` between them.
fn only_words(text: &[char], words: &[&str], extra_separators: &[char]) -> bool {
    text.split(|c| c.is_whitespace() || *c == '`' || extra_separators.contains(c))
        .filter(|w| !w.is_empty())
        .all(|w| {
            let w: String = w.iter().collect::<String>().to_ascii_lowercase();
            words.contains(&w.as_str())
        })
}

/// Whether the path is said to be created: a `新`, `new` or `create` in the
/// 12 characters before it, or (SMALL-17) one earlier in the same sentence
/// with only `NEW_MARKER_WORDS`, spaces, backticks and colons in between.
fn marked_new(f: &Field, r: &PathRef) -> bool {
    let before: String = f.chars[r.start.saturating_sub(12)..r.start].iter().collect();
    let lower = before.to_ascii_lowercase();
    if before.contains('新') || lower.contains("new") || lower.contains("create") {
        return true;
    }
    let Some(sentence) = f.segments().into_iter().find(|s| s.start <= r.start && r.start <= s.end) else {
        return false;
    };
    let lower: Vec<char> = f.chars.iter().map(char::to_ascii_lowercase).collect();
    ["新", "new", "create"].iter().any(|marker| {
        let m: Vec<char> = marker.chars().collect();
        (sentence.start..r.start).any(|i| {
            let end = i + m.len();
            end <= r.start
                && lower[i..end] == m[..]
                && only_words(&f.chars[end..r.start], NEW_MARKER_WORDS, &[':', '：'])
        })
    })
}

fn missing_paths(fields: &[Field], root: &Path) -> Vec<Finding> {
    let mut new_paths = BTreeSet::new();
    let mut named = Vec::new();
    for f in fields {
        // SMALL-17 (b): the end of the path before and whether it was new,
        // for a list of new paths (`新檔 a.rs、b.rs`).
        let mut previous: Option<(usize, bool)> = None;
        for r in path_refs(f) {
            let follows_new = previous.is_some_and(|(end, new)| {
                new && end <= r.start && only_words(&f.chars[end..r.start], LIST_WORDS, &['、', ',', '，'])
            });
            let new = follows_new || marked_new(f, &r);
            if new {
                new_paths.insert(r.path.clone());
            }
            previous = Some((r.end, new));
            if r.checked(f) {
                named.push((f, r.path));
            }
        }
    }
    let mut reported = BTreeSet::new();
    let mut out = Vec::new();
    for (f, path) in named {
        if new_paths.contains(&path) || reported.contains(&path) {
            continue;
        }
        let first = path.split('/').next().unwrap_or("");
        if first.is_empty() || !root.join(first).exists() {
            continue;
        }
        let there = if path.contains('*') {
            let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
            glob_exists(root, &parts, path.ends_with('/'))
        } else {
            let target = under(root, &path);
            if path.ends_with('/') { target.is_dir() } else { target.exists() }
        };
        if !there {
            out.push(Finding::new(
                "missing_path",
                f.location(),
                format!(
                    "{path} is not in the main work tree, so the spec may be out of date; if the task creates it, say new (or 新) just before it"
                ),
            ));
            reported.insert(path);
        }
    }
    out
}

/// Whether `name` is an integration test target of the repository at `root`:
/// `tests/NAME.rs` or `tests/NAME/main.rs` is a file there. `name` has only
/// letters, digits, `_` and `-` (`test_scope` reads no other), so it cannot
/// leave `tests/`.
fn is_test_target(root: &Path, name: &str) -> bool {
    let tests = root.join("tests");
    tests.join(format!("{name}.rs")).is_file() || tests.join(name).join("main.rs").is_file()
}

/// Whether targets may live elsewhere or under other names, so a file check
/// cannot tell: no readable `Cargo.toml` at the top of `root`, or one with
/// `[workspace]` or `[[test]]`.
fn targets_unknowable(root: &Path) -> bool {
    match std::fs::read_to_string(root.join("Cargo.toml")) {
        Ok(text) => text.contains("[workspace]") || text.contains("[[test]]"),
        Err(_) => true,
    }
}

/// The names after `--test` in a `cargo test` command that verify would append
/// to the approved test command (`test_scope::cargo_test_names`, the same
/// reading) and that are no integration test target of `root`. Once per name,
/// at its first mention. A name whose test file the spec names by path is left: the
/// task creates the file and verify takes it from the diff.
fn unknown_test_targets(fields: &[Field], root: &Path) -> Vec<Finding> {
    let texts: Vec<String> = fields.iter().map(|f| f.chars.iter().collect()).collect();
    let mut by_path = BTreeSet::new();
    let mut mentioned = Vec::new();
    for (f, text) in fields.iter().zip(&texts) {
        by_path.extend(crate::test_scope::test_path_mentions(text).into_iter().map(|(name, _)| name));
        mentioned.extend(crate::test_scope::cargo_test_names(text).into_iter().map(|name| (f, name)));
    }
    if mentioned.is_empty() || targets_unknowable(root) {
        return Vec::new();
    }
    let mut reported = BTreeSet::new();
    let mut out = Vec::new();
    for (f, name) in mentioned {
        if by_path.contains(&name) || is_test_target(root, &name) || !reported.insert(name.clone()) {
            continue;
        }
        out.push(Finding::new(
            "unknown_test_target",
            f.location(),
            format!(
                "--test {name} is no integration test target of the main work tree: there is neither tests/{name}.rs nor tests/{name}/main.rs, \
                 so verify would fail at once with cargo's exit 101 before any test ran; fix the name, or name the new test file by its path in the spec"
            ),
        ));
    }
    out
}

/// The name a backtick fragment gives, when it is an identifier: an optional
/// `fn ` or `pub fn ` before it and `(…)` after it are dropped.
fn fragment_identifier(text: &str) -> Option<String> {
    let mut t = text.trim();
    for prefix in ["pub fn ", "fn "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest.trim_start();
            break;
        }
    }
    if t.ends_with(')') {
        if let Some(open) = t.find('(') {
            t = t[..open].trim_end();
        }
    }
    let mut chars = t.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(t.to_string())
}

/// The names written right after an existing file `P`: `P 的 \`a\`、\`b\``
/// (or `裡的`, `裡`, `中的`), and `P:name`.
fn names_after(f: &Field, r: &PathRef) -> Vec<String> {
    let chars = &f.chars;
    let skip_space = |mut i: usize| {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    };
    if chars.get(r.end) == Some(&':') && chars.get(r.end + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') {
        let mut e = r.end + 1;
        while e < chars.len() && (chars[e].is_ascii_alphanumeric() || chars[e] == '_') {
            e += 1;
        }
        return vec![f.text(r.end + 1..e)];
    }
    let mut i = skip_space(r.end);
    let Some(word) = ["裡的", "中的", "的", "裡"].iter().find(|w| {
        let w: Vec<char> = w.chars().collect();
        chars[i.min(chars.len())..].starts_with(&w)
    }) else {
        return Vec::new();
    };
    i += word.chars().count();
    let mut out = Vec::new();
    loop {
        i = skip_space(i);
        let Some(frag) = f.fragments.iter().find(|fr| fr.start == i + 1) else {
            break;
        };
        if chars.get(i) != Some(&'`') {
            break;
        }
        if let Some(name) = fragment_identifier(&f.text(frag.clone())) {
            out.push(name);
        }
        i = skip_space(frag.end + 1);
        if i < chars.len() && matches!(chars[i], '、' | ',' | '，' | '與' | '和') {
            i += 1;
        } else {
            break;
        }
    }
    out
}

fn missing_names(fields: &[Field], root: &Path) -> Vec<Finding> {
    let mut contents: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut reported = BTreeSet::new();
    let mut out = Vec::new();
    for f in fields {
        for r in path_refs(f) {
            let target = under(root, &r.path);
            if r.is_glob() || r.path.ends_with('/') || !target.is_file() {
                continue;
            }
            for name in names_after(f, &r) {
                let text = contents
                    .entry(r.path.clone())
                    .or_insert_with(|| std::fs::read_to_string(&target).ok());
                let Some(text) = text else { continue };
                if has_identifier(text, &name) || !reported.insert((r.path.clone(), name.clone())) {
                    continue;
                }
                out.push(Finding::new(
                    "missing_name",
                    f.location(),
                    format!("{} has no {name} in the main work tree, so the spec may be out of date", r.path),
                ));
            }
        }
    }
    out
}

/// A field's first example of a literal rule and how many there were.
fn summed(f: &Field, rule: &'static str, mut hits: Vec<(usize, String)>, why: &str) -> Option<Finding> {
    hits.sort();
    let (_, first) = hits.first()?;
    Some(Finding::new(rule, f.location(), format!("{why} (first: {first}; {} in this field)", hits.len())))
}

fn digits_from(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    i
}

fn line_number(f: &Field) -> Option<Finding> {
    let chars = &f.chars;
    let mut hits = Vec::new();
    for i in 0..chars.len() {
        if chars[i] == ':' && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
            let mut end = digits_from(chars, i + 1);
            if chars.get(end) == Some(&'-') && chars.get(end + 1).is_some_and(char::is_ascii_digit) {
                end = digits_from(chars, end + 1);
            }
            let mut s = i;
            while s > 0 && is_path_char(chars[s - 1]) {
                s -= 1;
            }
            let name: String = chars[s..i].iter().collect();
            let file_ext = name
                .rfind('.')
                .filter(|&dot| dot > 0)
                .is_some_and(|dot| LINE_NUMBER_EXTENSIONS.contains(&name[dot + 1..].to_ascii_lowercase().as_str()));
            if file_ext {
                hits.push((s, f.text(s..end)));
            } else if i > 0 && chars[i - 1] == '`' {
                let open = f.fragments.iter().find(|fr| fr.end == i - 1).map_or(i - 1, |fr| fr.start - 1);
                hits.push((open, f.text(open..end)));
            }
        }
        if chars[i] == '第' {
            let skip = |mut j: usize| {
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                j
            };
            let mut j = skip(i + 1);
            let after = digits_from(chars, j);
            if after == j {
                continue;
            }
            j = skip(after);
            if j < chars.len() && matches!(chars[j], '-' | '–' | '~') {
                let k = skip(j + 1);
                let after = digits_from(chars, k);
                if after == k {
                    continue;
                }
                j = skip(after);
            }
            if chars.get(j) == Some(&'行') {
                hits.push((i, f.text(i..j + 1)));
            }
        }
    }
    summed(f, "line_number", hits, "line numbers go stale as the code moves; point at a function or a heading instead")
}

/// `A..B` with exactly two dots, neither side empty or starting or ending with `/`.
fn two_dot_range(t: &str) -> bool {
    if t.contains("...") || t.matches("..").count() != 1 {
        return false;
    }
    let Some((a, b)) = t.split_once("..") else { return false };
    [a, b].iter().all(|s| !s.is_empty() && !s.starts_with('/') && !s.ends_with('/'))
}

/// SMALL-17 (b): `diff` as a whole word, ASCII-case-insensitive: no ASCII
/// letter, digit or `_` right before or after it (`record_diff`, `different`
/// and `diffs` are not the word).
fn diff_word_at(lower: &[char], i: usize) -> bool {
    let word = |c: &char| c.is_ascii_alphanumeric() || *c == '_';
    lower[i..].starts_with(&['d', 'i', 'f', 'f'])
        && (i == 0 || !word(&lower[i - 1]))
        && !lower.get(i + 4).is_some_and(word)
}

fn two_dot_diff(f: &Field) -> Option<Finding> {
    let lower: Vec<char> = f.chars.iter().map(char::to_ascii_lowercase).collect();
    let diff: Vec<char> = "diff".chars().collect();
    let mut hits: BTreeMap<usize, String> = BTreeMap::new();
    for i in 0..lower.len() {
        if !diff_word_at(&lower, i) {
            continue;
        }
        let from = i + diff.len();
        let mut end = from;
        while end < f.chars.len() && end - from < 100 && f.chars[end] != '\n' {
            end += 1;
        }
        let mut j = from;
        while j < end {
            if f.chars[j].is_whitespace() || f.chars[j] == '`' {
                j += 1;
                continue;
            }
            let s = j;
            while j < end && !(f.chars[j].is_whitespace() || f.chars[j] == '`') {
                j += 1;
            }
            // SMALL-17 (b): brackets and punctuation around a token are not
            // part of a range, so Rust's `(..` and `..,` have an empty side.
            let token = f.text(s..j);
            let token = token
                .trim_start_matches(['(', '[', '（'])
                .trim_end_matches([',', '.', ';', ':', ')', ']', '）', '。', '，', '、']);
            if two_dot_range(token) {
                hits.insert(s, token.to_string());
            }
        }
    }
    summed(
        f,
        "two_dot_diff",
        hits.into_iter().collect(),
        "a two-dot range in a diff compares the two tips, so changes made on the base after the branch point show up too; use A...B",
    )
}

/// `Verb-Noun` with a listed verb and a noun starting with a capital letter.
fn has_cmdlet(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    CMDLET_VERBS.iter().any(|verb| {
        let v: Vec<char> = verb.chars().collect();
        (0..chars.len()).any(|i| {
            chars[i..].starts_with(&v)
                && (i == 0 || !chars[i - 1].is_ascii_alphanumeric())
                && chars.get(i + v.len()) == Some(&'-')
                && chars.get(i + v.len() + 1).is_some_and(char::is_ascii_uppercase)
        })
    })
}

fn powershell_in_bash(f: &Field) -> Option<Finding> {
    let all: String = f.chars.iter().collect::<String>().to_ascii_lowercase();
    if all.contains("powershell") || all.contains("pwsh") {
        return None;
    }
    let hits = f
        .fragments
        .iter()
        .filter_map(|fr| {
            let t = f.text(fr.clone());
            (t.contains("$env:") || t.contains("$null") || t.contains("-ErrorAction") || has_cmdlet(&t))
                .then(|| (fr.start, format!("`{t}`")))
        })
        .collect();
    summed(
        f,
        "powershell_in_bash",
        hits,
        "a command in PowerShell syntax, but this field never says PowerShell; the shell tool runs bash",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src").join("present.rs"), "fn present_fn() {}").unwrap();
        d
    }

    fn spec(goal: &str, acs: &[&str], constraints: &[&str]) -> String {
        json!({"goal": goal, "acceptance_criteria": acs, "constraints": constraints}).to_string()
    }

    fn rules(found: &[Finding]) -> Vec<&'static str> {
        found.iter().map(|f| f.rule).collect()
    }

    fn of<'a>(found: &'a [Finding], rule: &str) -> Vec<&'a Finding> {
        found.iter().filter(|f| f.rule == rule).collect()
    }

    #[test]
    fn lint_blocks_the_mechanical_errors() {
        let d = root();
        let ids = vec!["c1".to_string()];
        let raw = json!({
            "goal": "g",
            "acceptance_criteria": ["AC1: x", "  ", "AC1:  x ", "AC2: y"],
            "checks": ["c1", "ghost"],
        })
        .to_string();
        let r = lint(&raw, d.path(), &ids);
        assert_eq!(rules(&r.errors), vec!["ac_blank", "ac_duplicate", "unknown_check"]);
        assert_eq!(r.errors[0].location.as_deref(), Some("acceptance_criteria[1]"));
        assert_eq!(r.errors[1].location.as_deref(), Some("acceptance_criteria[2]"));
        assert!(r.errors[1].message.contains("acceptance_criteria[0]"), "{}", r.errors[1].message);
        assert_eq!(r.errors[2].location.as_deref(), Some("checks[1]"));
        assert!(r.errors[2].message.contains("ghost"));
        assert!(r.warnings.is_empty());
        assert_eq!(r.bytes, raw.len());

        // parse_spec's own error, verbatim, with no location
        let r = lint("{not json", d.path(), &ids);
        assert_eq!(rules(&r.errors), vec!["invalid_spec"]);
        assert_eq!(r.errors[0].location, None);
        assert!(r.errors[0].message.starts_with("spec is not valid JSON"), "{}", r.errors[0].message);
        let r = lint(r#"{"goal":"g","acceptance_criteria":[]}"#, d.path(), &ids);
        assert_eq!(rules(&r.errors), vec!["invalid_spec"]);
        assert!(r.errors[0].message.contains("at least one entry"));

        // a clean spec has nothing to say, and the same input says the same thing
        let clean = spec("g", &["AC1: x", "AC2: y"], &["src/present.rs stays"]);
        let r = lint(&clean, d.path(), &ids);
        assert!(r.errors.is_empty() && r.warnings.is_empty());
        let a = lint(&raw, d.path(), &ids);
        let b = lint(&raw, d.path(), &ids);
        let texts = |r: &Report| r.errors.iter().map(|f| f.message.clone()).collect::<Vec<_>>();
        assert_eq!(texts(&a), texts(&b));
    }

    #[test]
    fn lint_claude_write_only_when_asked_to_write() {
        let d = root();
        for yes in ["改 .claude/skills/x/SKILL.md", "delete ~/.claude/agents/y.md"] {
            let r = lint(&spec("g", &[yes], &[]), d.path(), &[]);
            assert_eq!(rules(&r.errors), vec!["claude_write"], "{yes}");
            assert_eq!(r.errors[0].location.as_deref(), Some("acceptance_criteria[0]"));
        }
        // in the goal and a constraint too, and once per field
        let r = lint(
            &spec(
                "Edit .claude/a. Also update .claude/b",
                &["AC1: x"],
                &["write ~/.claude/x/y"],
            ),
            d.path(),
            &[],
        );
        assert_eq!(rules(&r.errors), vec!["claude_write", "claude_write"]);
        assert_eq!(r.errors[0].location.as_deref(), Some("goal"));
        assert_eq!(r.errors[1].location.as_deref(), Some("constraints[0]"));
        for no in [
            "plugins/、.claude/、scripts/ 不碰",
            "改 plugins/thoth/.claude-plugin/plugin.json",
            "讀 .claude/skills/x",
            "`改 .claude/skills/x/SKILL.md`",
            "update ~/.claude.json",
            "edit CLAUDE.md",
            "never edit .claude/skills",
            "address the .claude/x layout",
        ] {
            let r = lint(&spec("g", &["AC1: x"], &[no]), d.path(), &[]);
            assert!(r.errors.is_empty(), "{no}: {:?}", rules(&r.errors));
        }
        // a different sentence of the same field does not lend its write word
        let r = lint(&spec("g", &["AC1: x"], &["改 a.txt。.claude/x 不動"]), d.path(), &[]);
        assert!(r.errors.is_empty());
        let r = lint(&spec("g", &["AC1: x"], &["改 a.txt; read .claude/x"]), d.path(), &[]);
        assert!(r.errors.is_empty());
        // non_goals and excluded_alternatives are not scanned
        let raw = json!({
            "goal": "g",
            "acceptance_criteria": ["AC1: x"],
            "non_goals": ["改 .claude/skills/x/SKILL.md"],
            "excluded_alternatives": ["delete ~/.claude/agents/y.md"],
        })
        .to_string();
        assert!(lint(&raw, d.path(), &[]).errors.is_empty());
    }

    #[test]
    fn lint_warns_about_stale_references_but_not_new_files() {
        let d = root();
        let r = lint(
            &spec(
                "g",
                &["AC1: src/gone.rs keeps working", "AC2: 新檔 src/new_one.rs"],
                &[
                    "johnny-lo/x stays",
                    "$THOTH_HOME/repos/x is real state",
                    "see https://a.b/c/d",
                    "src/present.rs stays, and so does src/ and src/gone.rs.",
                    "src/present.rs 的 `present_fn`、`absent_fn`",
                ],
            ),
            d.path(),
            &[],
        );
        assert!(r.errors.is_empty());
        let missing = of(&r.warnings, "missing_path");
        assert_eq!(missing.len(), 1, "{:?}", missing.iter().map(|f| &f.message).collect::<Vec<_>>());
        assert!(missing[0].message.contains("src/gone.rs"));
        assert_eq!(missing[0].location.as_deref(), Some("acceptance_criteria[0]"));
        let names = of(&r.warnings, "missing_name");
        assert_eq!(names.len(), 1);
        assert!(names[0].message.contains("absent_fn"));
        assert!(!names[0].message.contains("present_fn"));
        assert_eq!(names[0].location.as_deref(), Some("constraints[4]"));

        // a path marked new anywhere is never reported; a folder must be a folder
        let r = lint(
            &spec("g", &["AC1: create src/later.rs", "AC2: src/later.rs works"], &["src/present.rs/"]),
            d.path(),
            &[],
        );
        let missing = of(&r.warnings, "missing_path");
        assert_eq!(missing.len(), 1);
        assert!(missing[0].message.contains("src/present.rs/"));

        // the other spellings of a name in a file
        let r = lint(
            &spec(
                "g",
                &["AC1: x"],
                &[
                    "src/present.rs裡的 `pub fn present_fn()` and `fn gone_fn(a, b)`",
                    "src/present.rs:other_fn",
                    "src/present.rs 中的 `Report { bytes }`",
                ],
            ),
            d.path(),
            &[],
        );
        let names: Vec<&str> = of(&r.warnings, "missing_name").iter().map(|f| f.message.as_str()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].contains("other_fn"));
        let r = lint(
            &spec("g", &["AC1: x"], &["src/present.rs 的 `present_fn` 和 `gone_fn`"]),
            d.path(),
            &[],
        );
        let names = of(&r.warnings, "missing_name");
        assert_eq!(names.len(), 1);
        assert!(names[0].message.contains("gone_fn"));
    }

    #[test]
    fn lint_literal_warnings_and_their_negatives() {
        let d = root();
        let one = |text: &str| lint(&spec("g", &["AC1: x"], &[text]), d.path(), &[]);
        for (text, rule) in [
            ("see src/present.rs:12", "line_number"),
            ("see a.rs:3-9", "line_number"),
            ("see `present_fn`:4", "line_number"),
            ("第 12 行", "line_number"),
            ("第3-9行", "line_number"),
            ("第 3 ~ 9 行", "line_number"),
            ("run git diff master..HEAD", "two_dot_diff"),
            ("the Diff of `a..b` is empty", "two_dot_diff"),
            ("run `$env:FOO = 1`", "powershell_in_bash"),
            ("run `Get-ChildItem x`", "powershell_in_bash"),
            ("run `cat x 2>$null`", "powershell_in_bash"),
            ("run `rm x -ErrorAction SilentlyContinue`", "powershell_in_bash"),
        ] {
            let r = one(text);
            assert_eq!(rules(&r.warnings), vec![rule], "{text}");
            assert_eq!(r.warnings[0].location.as_deref(), Some("constraints[0]"));
            assert!(r.errors.is_empty(), "{text}");
        }
        for text in [
            "at 10:30",
            "host:8888",
            "第 3 節",
            "a.zip:3",
            "git diff ../x",
            "git diff a...b",
            "range 0..10",
            "git diff /a..b",
            "in PowerShell run `Get-ChildItem x`",
            "pwsh: `$env:FOO`",
            "run $env:FOO outside backticks",
            "`Get-childitem`",
        ] {
            let r = one(text);
            assert!(r.warnings.is_empty() && r.errors.is_empty(), "{text}: {:?}", rules(&r.warnings));
        }
        // once per field: the first example and how many there were
        let r = one("a.rs:1, b.rs:2 and 第 3 行");
        assert_eq!(rules(&r.warnings), vec!["line_number"]);
        assert!(r.warnings[0].message.contains("a.rs:1"), "{}", r.warnings[0].message);
        assert!(r.warnings[0].message.contains("3 in this field"), "{}", r.warnings[0].message);
        // past 100 characters or on the next line the diff no longer applies
        let r = one(&format!("git diff {} a..b", "x".repeat(100)));
        assert!(r.warnings.is_empty());
        let r = one("git diff\na..b");
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn lint_size_limits() {
        let d = root();
        let sized = |n: usize| {
            let empty = spec("g", &["AC1: x"], &[""]);
            let s = spec("g", &["AC1: x"], &[&"x".repeat(n - empty.len())]);
            assert_eq!(s.len(), n);
            s
        };
        let r = lint(&sized(20480), d.path(), &[]);
        assert!(r.errors.is_empty() && r.warnings.is_empty());
        let r = lint(&sized(20481), d.path(), &[]);
        assert!(r.warnings.is_empty());
        assert_eq!(rules(&r.errors), vec!["large"]);
        assert_eq!(r.errors[0].location, None);
        let r = lint(&sized(40960), d.path(), &[]);
        assert!(r.warnings.is_empty());
        assert_eq!(rules(&r.errors), vec!["large"]);
        let r = lint(&sized(40961), d.path(), &[]);
        assert_eq!(rules(&r.errors), vec!["too_large"]);
        assert_eq!(r.errors[0].location, None);
        assert!(r.errors[0].message.contains("40961"));
        assert!(r.warnings.is_empty());
        let junk = format!("{{{}", "x".repeat(40960));
        let r = lint(&junk, d.path(), &[]);
        assert_eq!(rules(&r.errors), vec!["too_large", "invalid_spec"]);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn lint_unknown_check_is_silent_when_the_catalogue_is_empty() {
        let d = root();
        let raw = json!({"goal": "g", "acceptance_criteria": ["AC1: x"], "checks": ["ghost"]}).to_string();
        assert!(lint(&raw, d.path(), &[]).errors.is_empty());
        let r = lint(&raw, d.path(), &["test".to_string()]);
        assert_eq!(rules(&r.errors), vec!["unknown_check"]);
    }
}
