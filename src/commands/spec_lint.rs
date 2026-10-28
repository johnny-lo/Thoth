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

/// Over this many bytes a spec is refused: the task should be split.
pub const TOO_LARGE_BYTES: usize = 40960;
/// Over this many bytes (up to `TOO_LARGE_BYTES`) a spec is only warned about.
pub const LARGE_BYTES: usize = 20480;

const WRITE_WORDS_ZH: &[&str] = &["改", "加", "寫入", "寫進", "新增", "建立", "刪", "移除", "搬", "覆蓋", "編輯"];
const WRITE_WORDS_EN: &[&str] = &[
    "write", "edit", "modify", "change", "update", "create", "add", "delete", "remove", "move",
    "rename", "replace",
];
const NEGATIONS_ZH: &[&str] = &["不", "別", "勿", "禁", "免", "無", "沒"];
const NEGATIONS_EN: &[&str] = &["not", "never", "no", "don't", "without"];
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
        warnings.push(Finding::new(
            "large",
            None,
            format!("the spec is {bytes} bytes, over {LARGE_BYTES}; consider splitting the task"),
        ));
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

    let mut fields = vec![Field::new("goal".into(), &spec.goal)];
    for (i, ac) in spec.acceptance_criteria.iter().enumerate() {
        fields.push(Field::new(format!("acceptance_criteria[{i}]"), ac));
    }
    for (i, c) in spec.constraints.iter().enumerate() {
        fields.push(Field::new(format!("constraints[{i}]"), c));
    }
    errors.extend(fields.iter().filter_map(claude_write));
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

    /// The sentence pieces, split at `。`, `；`, `;`, a newline and `. `.
    fn segments(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let (mut start, mut i) = (0, 0);
        while i < self.chars.len() {
            let width = match self.chars[i] {
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

/// `.claude` as a folder name: `~/.claude/x` and `.claude/skills` count,
/// `.claude-plugin`, `.claude.json` and `CLAUDE.md` do not.
fn names_claude_dir(chars: &[char]) -> bool {
    let pat: Vec<char> = ".claude".chars().collect();
    (0..chars.len()).any(|i| {
        chars[i..].starts_with(&pat)
            && (i == 0 || !chars[i - 1].is_ascii_alphanumeric())
            && chars
                .get(i + pat.len())
                .is_none_or(|&c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    })
}

fn claude_write(f: &Field) -> Option<Finding> {
    for seg in f.segments() {
        if !names_claude_dir(&f.chars[seg.clone()]) {
            continue;
        }
        let plain = f.unquoted(seg.clone());
        let writes = WRITE_WORDS_ZH.iter().any(|w| plain.contains(w))
            || WRITE_WORDS_EN.iter().any(|w| has_word(&plain, w));
        let negated = NEGATIONS_ZH.iter().any(|w| plain.contains(w))
            || NEGATIONS_EN.iter().any(|w| has_word(&plain, w));
        if writes && !negated {
            return Some(Finding::new(
                "claude_write",
                f.location(),
                format!(
                    "asks to write under a .claude folder (\"{}\"); a worker must not change .claude, so do that step yourself or leave it out",
                    f.text(seg).trim()
                ),
            ));
        }
    }
    None
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

/// Every run of path characters with a `/` in it that could be a path of
/// this repository: not after `$~%\:`, not absolute or relative to `.`/`..`,
/// no `//`, `...` or `..` part.
fn path_refs(f: &Field) -> Vec<PathRef> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < f.chars.len() {
        if !is_path_char(f.chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < f.chars.len() && is_path_char(f.chars[i]) {
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

/// Whether the 12 characters before the path say it is to be created.
fn marked_new(f: &Field, r: &PathRef) -> bool {
    let before: String = f.chars[r.start.saturating_sub(12)..r.start].iter().collect();
    let lower = before.to_ascii_lowercase();
    before.contains('新') || lower.contains("new") || lower.contains("create")
}

fn missing_paths(fields: &[Field], root: &Path) -> Vec<Finding> {
    let mut new_paths = BTreeSet::new();
    let mut named = Vec::new();
    for f in fields {
        for r in path_refs(f) {
            if marked_new(f, &r) {
                new_paths.insert(r.path.clone());
            }
            named.push((f, r.path));
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
        let target = under(root, &path);
        let there = if path.ends_with('/') { target.is_dir() } else { target.exists() };
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
            if r.path.ends_with('/') || !target.is_file() {
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

fn two_dot_diff(f: &Field) -> Option<Finding> {
    let lower: Vec<char> = f.chars.iter().map(char::to_ascii_lowercase).collect();
    let diff: Vec<char> = "diff".chars().collect();
    let mut hits: BTreeMap<usize, String> = BTreeMap::new();
    for i in 0..lower.len() {
        if !lower[i..].starts_with(&diff) {
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
            let token = f.text(s..j);
            if two_dot_range(&token) {
                hits.insert(s, token);
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
        assert!(r.errors.is_empty());
        assert_eq!(rules(&r.warnings), vec!["large"]);
        assert_eq!(r.warnings[0].location, None);
        let r = lint(&sized(40960), d.path(), &[]);
        assert!(r.errors.is_empty());
        assert_eq!(rules(&r.warnings), vec!["large"]);
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
