//! RD-1: secrets are masked at the moment content leaves this machine (H12),
//! never where it is stored: blobs, the database and the logs stay as they
//! were. Two kinds of secret are recognised (H13): seven common key formats,
//! found by hand-written scanners (no regex crate), and exact values - the
//! values of this process's environment variables whose names contain KEY,
//! TOKEN, SECRET or PASSWORD, and the keys Thoth keeps in
//! `$THOTH_HOME/secrets.json`. Each one is replaced by `[redacted:KIND]`,
//! where KIND names the kind of secret and nothing about its value.
//!
//! A `Masker` holds the exact values in memory only. It has no Debug,
//! Display or Serialize, and no message, log line or event built here ever
//! carries one of them.

use crate::commands::secrets;
use crate::commands::settings::{Kind, REGISTRY};
use crate::db::state::{self, NewEvent};
use crate::model::event_type;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::ops::Range;
use std::path::Path;

/// Exact values shorter than this many characters (after trimming) are not
/// matched (Q3): a short value would mask ordinary words.
const MIN_VALUE_CHARS: usize = 8;

/// The words a variable name must contain, in ASCII upper case, for its
/// value to be matched.
const SECRET_NAME_WORDS: [&str; 4] = ["KEY", "TOKEN", "SECRET", "PASSWORD"];

const MARKER_OPEN: &str = "[redacted:";

/// The longest kind a marker may carry, in bytes.
const MARKER_MAX_KIND_BYTES: usize = 200;

/// One exact value and its kind.
#[derive(Clone)]
struct Entry {
    value: String,
    kind: String,
}

/// What to match exactly: every value, its JSON-escaped form and any
/// variants, each pointing at its entry's kind.
#[derive(Clone)]
pub(crate) struct Masker {
    entries: Vec<Entry>,
    needles: Vec<(String, usize)>,
}

/// How many secrets of each kind were masked. Kinds and counts only, never a
/// value, so it may be printed and stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counts(BTreeMap<String, u64>);

impl Counts {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Adds `other`'s counts to these.
    pub(crate) fn merge(&mut self, other: &Counts) {
        for (kind, n) in &other.0 {
            *self.0.entry(kind.clone()).or_insert(0) += n;
        }
    }

    /// A JSON object of kind to count, keys in name order.
    pub(crate) fn to_json(&self) -> Value {
        Value::Object(self.0.iter().map(|(k, n)| (k.clone(), Value::from(*n))).collect())
    }

    fn add(&mut self, kind: &str) {
        *self.0.entry(kind.to_string()).or_insert(0) += 1;
    }
}

/// One range of text to mask, in bytes, and the kind its marker names.
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: String,
}

impl Masker {
    fn from_entries(entries: Vec<Entry>) -> Masker {
        let mut m = Masker { entries, needles: Vec::new() };
        for i in 0..m.entries.len() {
            let value = m.entries[i].value.clone();
            m.add_needle(value.clone(), i);
            if let Ok(quoted) = serde_json::to_string(&value) {
                let escaped = quoted[1..quoted.len() - 1].to_string();
                if escaped != value {
                    m.add_needle(escaped, i);
                }
            }
        }
        m
    }

    fn add_needle(&mut self, needle: String, entry: usize) {
        if !needle.is_empty() && !self.needles.iter().any(|(n, _)| *n == needle) {
            self.needles.push((needle, entry));
        }
    }

    /// The rule itself: never fails, prints nothing and creates nothing.
    /// `home`'s secrets.json is read even inside a run (Q1), only to mask its
    /// values; a missing or broken file counts as no values.
    pub(crate) fn load_with(home: Option<&Path>, vars: Vec<(OsString, OsString)>) -> Masker {
        let mut entries: Vec<Entry> = Vec::new();
        let mut push = |raw: &str, kind: String| {
            let value = raw.trim();
            if value.chars().count() < MIN_VALUE_CHARS
                || value.chars().all(|c| c.is_ascii_digit())
                || entries.iter().any(|e| e.value == value)
            {
                return;
            }
            entries.push(Entry { value: value.to_string(), kind });
        };
        if let Some(home) = home {
            let mut map = secrets::load_secrets(home).map;
            for s in REGISTRY.iter().filter(|s| s.kind == Kind::Secret) {
                if let Some(Value::String(v)) = map.remove(&secrets::secret_stored_name(s)) {
                    push(&v, format!("thoth:{}", s.key));
                }
            }
            let mut rest: Vec<(String, Value)> = map.into_iter().collect();
            rest.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, v) in rest {
                if let Value::String(v) = v {
                    push(&v, "thoth:secrets.json".to_string());
                }
            }
        }
        let mut named: Vec<(String, String)> = vars
            .into_iter()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| {
                let upper = k.to_ascii_uppercase();
                SECRET_NAME_WORDS.iter().any(|w| upper.contains(w))
            })
            .collect();
        named.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in named {
            push(&value, format!("env:{name}"));
        }
        Masker::from_entries(entries)
    }

    /// `load_with` this process's environment.
    pub(crate) fn load(home: Option<&Path>) -> Masker {
        Masker::load_with(home, std::env::vars_os().collect())
    }

    /// `load` with the state directory `probe_state_dir` names, for callers
    /// that have no `Paths` at hand.
    #[cfg(not(test))]
    pub(crate) fn current() -> Masker {
        Masker::load(crate::config::probe_state_dir().ok().map(|p| p.home).as_deref())
    }

    /// Unit tests never read a file of the real state directory.
    #[cfg(test)]
    pub(crate) fn current() -> Masker {
        Masker::load(None)
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Masker {
        Masker::from_entries(
            pairs
                .iter()
                .map(|(v, k)| Entry { value: v.to_string(), kind: k.to_string() })
                .collect(),
        )
    }

    /// Also matches `f(value)` for every value, under the same kind.
    pub(crate) fn with_variant(mut self, f: fn(&str) -> String) -> Masker {
        for i in 0..self.entries.len() {
            let variant = f(&self.entries[i].value);
            if variant != self.entries[i].value {
                self.add_needle(variant, i);
            }
        }
        self
    }

    /// The names of this process's environment variables a worker must not
    /// get (RD-2, H15): those whose value `load` would match exactly, those
    /// whose value holds a key format, and (RD-4) those whose whole value,
    /// trimmed as `load` trims, is one of this masker's exact values, whatever
    /// the name. The format check uses no exact values, and a value is only
    /// compared whole, so a variable such as PATH is not listed only because
    /// it holds another variable's value. Names only, sorted, without
    /// repeats; a name or value that is not UTF-8 is skipped.
    pub(crate) fn secret_env_names(&self) -> Vec<String> {
        let formats = Masker::load_with(None, Vec::new());
        let mut names: Vec<String> = std::env::vars_os()
            .filter_map(|(k, v)| {
                let (name, value) = (k.to_str()?.to_string(), v.to_str()?.to_string());
                let named = !Masker::load_with(None, vec![(k, v)]).entries.is_empty();
                let alias = self.entries.iter().any(|e| e.value == value.trim());
                (named || alias || !formats.find(&value).is_empty()).then_some(name)
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every range of `text` to mask, sorted by start and not overlapping.
    /// Nothing inside an existing marker is matched again, so masking twice
    /// gives what masking once did.
    pub(crate) fn find(&self, text: &str) -> Vec<Span> {
        let bytes = text.as_bytes();
        let mut in_marker = vec![false; bytes.len()];
        for r in marker_spans(text) {
            in_marker[r].iter_mut().for_each(|b| *b = true);
        }
        // (start, end, kind, exact) before merging
        let mut found: Vec<(usize, usize, String, bool)> = Vec::new();
        let mut exact = vec![false; bytes.len()];
        for (needle, entry) in &self.needles {
            for (start, _) in text.match_indices(needle.as_str()) {
                let end = start + needle.len();
                if in_marker[start..end].iter().any(|b| *b) {
                    continue;
                }
                exact[start..end].iter_mut().for_each(|b| *b = true);
                found.push((start, end, self.entries[*entry].kind.clone(), true));
            }
        }
        for (i, _) in in_marker.iter().enumerate().filter(|(_, m)| !**m) {
            if let Some((end, kind)) = format_at(bytes, i, &exact) {
                found.push((i, end, kind.to_string(), false));
            }
        }
        // Earliest start first; at the same start the longer, then the exact
        // value: the first of a group of overlapping ranges names the kind.
        found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(b.3.cmp(&a.3)));
        let mut spans: Vec<Span> = Vec::new();
        for (start, end, kind, _) in found {
            match spans.last_mut() {
                Some(last) if start < last.end => last.end = last.end.max(end),
                _ => spans.push(Span { start, end, kind }),
            }
        }
        spans
    }

    /// `text` with every range `find` returns replaced by its marker; the
    /// rest is unchanged.
    pub(crate) fn mask_text(&self, text: &str) -> (String, Counts) {
        let spans = self.find(text);
        let mut counts = Counts::default();
        if spans.is_empty() {
            return (text.to_string(), counts);
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for s in &spans {
            out.push_str(&text[at..s.start]);
            out.push_str(MARKER_OPEN);
            out.push_str(&s.kind);
            out.push(']');
            counts.add(&s.kind);
            at = s.end;
        }
        out.push_str(&text[at..]);
        (out, counts)
    }

    /// Masks every string inside `v`, object keys included (RD-4); numbers,
    /// booleans and nulls are left alone. A masked key keeps its value. When
    /// two keys of one object mask to the same name, the second and later
    /// ones, in the order of their original names, get `#2`, `#3`, ...: the
    /// first number that names no other key of the object. An object with no
    /// key to mask is left exactly as it was.
    pub(crate) fn mask_value(&self, v: &mut Value) -> Counts {
        let mut counts = Counts::default();
        match v {
            Value::String(s) => {
                let (masked, c) = self.mask_text(s);
                if !c.is_empty() {
                    *s = masked;
                    counts = c;
                }
            }
            Value::Array(items) => {
                for item in items {
                    counts.merge(&self.mask_value(item));
                }
            }
            Value::Object(map) => {
                for item in map.values_mut() {
                    counts.merge(&self.mask_value(item));
                }
                if map.keys().any(|k| !self.find(k).is_empty()) {
                    counts.merge(&self.mask_keys(map));
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
        counts
    }

    /// `mask_value`'s renaming of `map`'s keys.
    fn mask_keys(&self, map: &mut serde_json::Map<String, Value>) -> Counts {
        let mut counts = Counts::default();
        let mut renamed: Vec<(String, Value)> = Vec::with_capacity(map.len());
        for (key, value) in std::mem::take(map) {
            let (name, c) = self.mask_text(&key);
            counts.merge(&c);
            renamed.push((name, value));
        }
        let names: std::collections::BTreeSet<String> = renamed.iter().map(|(n, _)| n.clone()).collect();
        for (name, value) in renamed {
            // a suffixed name may not be another key's own name either
            let mut unique = name.clone();
            let mut n = 1;
            while map.contains_key(&unique) || (n > 1 && names.contains(&unique)) {
                n += 1;
                unique = format!("{name}#{n}");
            }
            map.insert(unique, value);
        }
        counts
    }
}

/// The byte range of every marker in `text`, in order: `[redacted:`, then 1
/// to 200 bytes without whitespace, `[` or `]`, then `]`.
pub(crate) fn marker_spans(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(MARKER_OPEN) {
        let start = from + found;
        let body = start + MARKER_OPEN.len();
        let mut end = None;
        for (offset, c) in text[body..].char_indices() {
            if c == ']' {
                if (1..=MARKER_MAX_KIND_BYTES).contains(&offset) {
                    end = Some(body + offset + 1);
                }
                break;
            }
            if c.is_whitespace() || c == '[' || offset + c.len_utf8() > MARKER_MAX_KIND_BYTES {
                break;
            }
        }
        match end {
            Some(end) => {
                out.push(start..end);
                from = end;
            }
            None => from = start + 1,
        }
    }
    out
}

/// ASCII letters, digits, `_` and `-`: the characters a key is made of.
fn is_t(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Whether `at` is the start of a line: the start of the text, or right
/// after a line feed or a JSON-escaped one (`\n`).
fn line_start(bytes: &[u8], at: usize) -> bool {
    at == 0 || bytes[at - 1] == b'\n' || (at >= 2 && &bytes[at - 2..at] == b"\\n")
}

/// Whether a format may start at `i`: the byte before it is not a `word`
/// byte, or is inside an exact match, or is the letter of a JSON escape
/// (`\n`, `\t`, ...), or is a unified diff's `-` or `+` at the start of a
/// line.
fn starts_word(bytes: &[u8], i: usize, exact: &[bool], word: fn(u8) -> bool) -> bool {
    if i == 0 {
        return true;
    }
    let p = i - 1;
    if exact[p] || !word(bytes[p]) {
        return true;
    }
    if matches!(bytes[p], b'-' | b'+') && line_start(bytes, p) {
        return true;
    }
    matches!(bytes[p], b'n' | b'r' | b't' | b'b' | b'f') && p >= 1 && bytes[p - 1] == b'\\'
}

/// How many bytes from `from` on satisfy `pred`.
fn run_len(bytes: &[u8], from: usize, pred: fn(u8) -> bool) -> usize {
    bytes.get(from..).unwrap_or(&[]).iter().take_while(|b| pred(**b)).count()
}

/// `run_len` for a token that starts at `start`, stopping where an exact
/// match that begins after `start` begins: a key format never runs on into
/// an exact value written right after it.
fn token_len(bytes: &[u8], start: usize, from: usize, pred: fn(u8) -> bool, exact: &[bool]) -> usize {
    (from..bytes.len())
        .take_while(|&k| pred(bytes[k]) && !(k > start && exact[k] && !exact[k - 1]))
        .count()
}

fn has_digit_and_letter(run: &[u8]) -> bool {
    run.iter().any(u8::is_ascii_digit) && run.iter().any(u8::is_ascii_alphabetic)
}

/// Whether the byte at `i` ends a fixed-length token: there is none, it is
/// inside an exact match, or it is not a `word` byte.
fn ends_word(bytes: &[u8], i: usize, exact: &[bool], word: fn(u8) -> bool) -> bool {
    i >= bytes.len() || exact[i] || !word(bytes[i])
}

/// The github token prefixes followed by at least 36 letters and digits.
const GITHUB_PREFIXES: [&[u8]; 5] = [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"];

/// The first format that matches at `i`: where it ends and its kind.
fn format_at(bytes: &[u8], i: usize, exact: &[bool]) -> Option<(usize, &'static str)> {
    let rest = &bytes[i..];
    let first = *rest.first()?;
    if !matches!(first, b's' | b'A' | b'g' | b'-') {
        return None;
    }
    if rest.starts_with(b"-----BEGIN ") {
        // Any number of extra dashes may come first: a diff that deletes a
        // key file puts one before every line.
        let dashes_from = i - bytes[..i].iter().rev().take_while(|b| **b == b'-').count();
        return starts_word(bytes, dashes_from, exact, is_t)
            .then(|| private_key_end(bytes, i))
            .flatten()
            .map(|end| (end, "private-key"));
    }
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        let alnum = |b: u8| b.is_ascii_alphanumeric();
        let body = i + 4;
        let n = run_len(bytes, body, |b| b.is_ascii_uppercase() || b.is_ascii_digit()).min(16);
        return (starts_word(bytes, i, exact, alnum) && n == 16 && ends_word(bytes, body + 16, exact, alnum))
            .then_some((body + 16, "aws-access-key-id"));
    }
    if !starts_word(bytes, i, exact, is_t) {
        return None;
    }
    if rest.starts_with(b"sk-ant-") {
        let body = i + 7;
        let n = token_len(bytes, i, body, is_t, exact);
        return (n >= 20 && has_digit_and_letter(&bytes[body..body + n])).then_some((body + n, "anthropic-key"));
    }
    if rest.starts_with(b"sk-") {
        let body = i + 3;
        let n = token_len(bytes, i, body, is_t, exact);
        return (n >= 20 && has_digit_and_letter(&bytes[body..body + n])).then_some((body + n, "openai-key"));
    }
    if rest.starts_with(b"AIza") {
        let body = i + 4;
        let n = run_len(bytes, body, is_t).min(35);
        return (n == 35 && ends_word(bytes, body + 35, exact, is_t)).then_some((body + 35, "google-api-key"));
    }
    if rest.starts_with(b"github_pat_") {
        let body = i + 11;
        let n = token_len(bytes, i, body, |b| b.is_ascii_alphanumeric() || b == b'_', exact);
        return (n >= 22).then_some((body + n, "github-token"));
    }
    if GITHUB_PREFIXES.iter().any(|p| rest.starts_with(p)) {
        let body = i + 4;
        let n = token_len(bytes, i, body, |b| b.is_ascii_alphanumeric(), exact);
        return (n >= 36).then_some((body + n, "github-token"));
    }
    if rest.starts_with(b"glpat-") {
        let body = i + 6;
        let n = token_len(bytes, i, body, is_t, exact);
        return (n >= 20).then_some((body + n, "gitlab-token"));
    }
    None
}

fn find_from(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// Where a private key block that begins at `i` ends: after the `-----` that
/// closes its `-----END ` line, or at the end of the text when there is no
/// such line. `None` when the BEGIN label does not name a private key.
fn private_key_end(bytes: &[u8], i: usize) -> Option<usize> {
    let label_start = i + "-----BEGIN ".len();
    let line_end = find_from(bytes, label_start, b"\n").unwrap_or(bytes.len());
    let label_end = find_from(bytes, label_start, b"-----").filter(|e| *e < line_end)?;
    let label = &bytes[label_start..label_end];
    if !(label.ends_with(b"PRIVATE KEY") || label.ends_with(b"PRIVATE KEY BLOCK")) {
        return None;
    }
    let after_begin = label_end + 5;
    let end = find_from(bytes, after_begin, b"-----END ")
        .and_then(|e| find_from(bytes, e + "-----END ".len(), b"-----"))
        .map(|close| close + 5)
        .unwrap_or(bytes.len());
    Some(end)
}

/// The `secret_redacted` event for one hand-off, `None` when nothing was
/// masked. Its detail is the exit, the kinds with their counts and the
/// evidence codes involved: never the masked content, its length or place.
pub(crate) fn event(
    task_id: &str,
    run_id: Option<&str>,
    exit: &str,
    counts: &Counts,
    evidence: &[String],
) -> Option<NewEvent> {
    if counts.is_empty() {
        return None;
    }
    Some(NewEvent {
        task_id: Some(task_id.to_string()),
        run_id: run_id.map(str::to_string),
        event_type: event_type::SECRET_REDACTED.to_string(),
        detail: serde_json::json!({
            "exit": exit,
            "kinds": counts.to_json(),
            "evidence": evidence,
        }),
    })
}

/// Appends `event`'s row; writes nothing when nothing was masked.
pub(crate) fn record(
    conn: &Connection,
    task_id: &str,
    run_id: Option<&str>,
    exit: &str,
    counts: &Counts,
    evidence: &[String],
) -> Result<()> {
    if let Some(e) = event(task_id, run_id, exit, counts, evidence) {
        state::append_event(conn, &e)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A github-format test token, built so no full token is in the source.
    fn gh() -> String {
        format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345")
    }

    fn none() -> Masker {
        Masker::from_pairs(&[])
    }

    fn masked(text: &str) -> String {
        none().mask_text(text).0
    }

    fn counts(pairs: &[(&str, u64)]) -> Counts {
        Counts(pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect())
    }

    #[test]
    fn formats_respect_prefixes_boundaries_and_lengths() {
        let t20 = "dummy0123456789dummy";
        let alnum36 = "dummyDUMMY0123456789dummyDUMMY012345";
        // (prefix, body at the threshold, kind); the body less its last
        // character must not match
        let cases: Vec<(String, String, &str)> = vec![
            (["sk", "-ant-"].concat(), t20.to_string(), "anthropic-key"),
            (["s", "k-"].concat(), t20.to_string(), "openai-key"),
            (["AI", "za"].concat(), format!("{t20}dummy-dummy_dum"), "google-api-key"),
            (["gh", "p_"].concat(), alnum36.to_string(), "github-token"),
            (["gh", "o_"].concat(), alnum36.to_string(), "github-token"),
            (["github", "_pat_"].concat(), "dummy_0123456789_dummy".to_string(), "github-token"),
            (["gl", "pat-"].concat(), t20.to_string(), "gitlab-token"),
            (["AK", "IA"].concat(), "DUMMY0123456789D".to_string(), "aws-access-key-id"),
            (["AS", "IA"].concat(), "DUMMY0123456789D".to_string(), "aws-access-key-id"),
        ];
        for (prefix, body, kind) in &cases {
            let token = format!("{prefix}{body}");
            assert_eq!(masked(&format!("a {token} b")), format!("a [redacted:{kind}] b"), "{kind} at the threshold");
            let short = format!("a {prefix}{} b", &body[..body.len() - 1]);
            assert_eq!(masked(&short), short, "{kind} one short");
        }
        let unchanged = |text: String| assert_eq!(masked(&text), text);
        unchanged("task-1-attempt-2".to_string());
        unchanged(format!("ta{}{t20}", ["s", "k-"].concat()));
        unchanged(format!("{}{t20}dummy-dummy_dumX", ["AI", "za"].concat()));
        unchanged(format!("{}DUMMY0123456789DX", ["AK", "IA"].concat()));
        unchanged(format!("{}{}", ["sk", "-"].concat(), "dummydummydummydummydummy"));
        unchanged(["-----BEGIN ", "PUBLIC KEY-----\nAAAAdummy\n-----END PUBLIC KEY-----"].concat());
        assert_eq!(masked(&format!("x\\n{}", gh())), "x\\n[redacted:github-token]");
        assert_eq!(masked(&format!("a-{}", gh())), format!("a-{}", gh()), "a T character before it");
    }

    #[test]
    fn mask_text_replaces_each_secret_with_its_kind_only() {
        let m = Masker::from_pairs(&[("dummy-exact-value-1", "env:RD_UNIT_X")]);
        let openai = format!("{}{}", ["s", "k-"].concat(), "dummy0123456789dummy99");
        let text = format!("one {} two {openai}\nthree dummy-exact-value-1 four", gh());
        let (out, c) = m.mask_text(&text);
        assert_eq!(
            out,
            "one [redacted:github-token] two [redacted:openai-key]\nthree [redacted:env:RD_UNIT_X] four"
        );
        assert_eq!(c, counts(&[("env:RD_UNIT_X", 1), ("github-token", 1), ("openai-key", 1)]));
    }

    #[test]
    fn exact_values_match_their_json_escaped_form_too() {
        let value = "dummy\"quote\\slash";
        let m = Masker::from_pairs(&[(value, "test")]);
        let escaped = serde_json::to_string(value).unwrap();
        let escaped = &escaped[1..escaped.len() - 1];
        assert_ne!(escaped, value);
        let (out, c) = m.mask_text(&format!("raw {value} json {escaped} end"));
        assert_eq!(out, "raw [redacted:test] json [redacted:test] end");
        assert_eq!(c, counts(&[("test", 2)]));
    }

    #[test]
    fn load_reads_named_env_vars_and_skips_short_or_numeric_values() {
        let vars: Vec<(OsString, OsString)> = [
            ("RD_UNIT_TOKEN", "dummy-7"),
            ("RD_UNIT_KEY", "12345678901"),
            ("RD_UNIT_SECRET", "dummy-unit-secret-1"),
            ("RD_UNIT_PLAIN", "dummy-unit-plain-1"),
        ]
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
        assert_eq!("dummy-7".len(), 7);
        let m = Masker::load_with(None, vars);
        let text = "a dummy-7 b 12345678901 c dummy-unit-secret-1 d dummy-unit-plain-1 e";
        let (out, c) = m.mask_text(text);
        assert_eq!(
            out,
            "a dummy-7 b 12345678901 c [redacted:env:RD_UNIT_SECRET] d dummy-unit-plain-1 e"
        );
        assert_eq!(c, counts(&[("env:RD_UNIT_SECRET", 1)]));
    }

    #[test]
    fn load_reads_secrets_json_even_in_a_run() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            secrets::path_in(dir.path()),
            r#"{"aistupidlevel_api_key":"dummy-unit-stored-1"}"#,
        )
        .unwrap();
        let saved = std::env::var_os("THOTH_RUN_ID");
        std::env::set_var("THOTH_RUN_ID", "01RUNDUMMY");
        let m = Masker::load_with(Some(dir.path()), Vec::new());
        match saved {
            Some(v) => std::env::set_var("THOTH_RUN_ID", v),
            None => std::env::remove_var("THOTH_RUN_ID"),
        }
        let (out, c) = m.mask_text("key=dummy-unit-stored-1;");
        assert_eq!(out, "key=[redacted:thoth:aistupidlevel-api-key];");
        assert_eq!(c, counts(&[("thoth:aistupidlevel-api-key", 1)]));
    }

    #[test]
    fn overlapping_spans_merge_and_masking_twice_changes_nothing() {
        let token = gh();
        // the exact value covers the token's first part
        let value = format!("dummy-v-{}", &token[..10]);
        let m = Masker::from_pairs(&[(value.as_str(), "test")]);
        let text = format!("a dummy-v-{token} b");
        let (out, c) = m.mask_text(&text);
        assert_eq!(out, "a [redacted:test] b");
        assert_eq!(c, counts(&[("test", 1)]));

        // the exact value right before a token: both masked the first time
        let m = Masker::from_pairs(&[("dummy-adjacent-1", "test")]);
        let text = format!("x dummy-adjacent-1{token} y");
        let once = m.mask_text(&text).0;
        assert_eq!(once, "x [redacted:test][redacted:github-token] y");
        assert!(!once.contains(&token[4..]), "{once}");
        assert_eq!(m.mask_text(&once).0, once);
        assert!(m.mask_text(&once).1.is_empty());

        // a value equal to the marker's word leaves the markers whole
        let m = Masker::from_pairs(&[("redacted", "test")]);
        let text = format!("{token} and redacted");
        let once = m.mask_text(&text).0;
        assert_eq!(once, "[redacted:github-token] and [redacted:test]");
        assert_eq!(m.mask_text(&once).0, once);

        // something that only starts like a marker does not shield a token
        let loose = format!("[redacted: see {token} ]");
        assert_eq!(masked(&loose), "[redacted: see [redacted:github-token] ]");
        let cut = format!("[redacted:github-t x {token} ]");
        assert_eq!(masked(&cut), "[redacted:github-t x [redacted:github-token] ]");
    }

    #[test]
    fn private_key_block_without_an_end_masks_to_the_end() {
        let begin = ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat();
        let end = ["-----END ", "RSA PRIVATE KEY-----"].concat();
        let open = format!("before\n{begin}\nAAAAdummy\nBBBBdummy");
        let (out, c) = none().mask_text(&open);
        assert_eq!(out, "before\n[redacted:private-key]");
        assert_eq!(c, counts(&[("private-key", 1)]));
        assert_eq!(masked(&format!("{open}\n{end}\nafter")), "before\n[redacted:private-key]\nafter");
        assert_eq!(masked(&format!("{open}\n-----END RSA")), "before\n[redacted:private-key]");
    }

    #[test]
    fn mask_value_changes_string_leaves_only() {
        let m = Masker::from_pairs(&[("dummy-value-12345", "test")]);
        let mut v = json!({"s": "a dummy-value-12345 b", "n": 12345, "t": true, "z": null,
            "list": ["dummy-value-12345", 2, {"k": "dummy-value-12345"}]});
        let c = m.mask_value(&mut v);
        assert_eq!(
            v,
            json!({"s": "a [redacted:test] b", "n": 12345, "t": true, "z": null,
                "list": ["[redacted:test]", 2, {"k": "[redacted:test]"}]})
        );
        assert_eq!(c, counts(&[("test", 3)]));
    }

    #[test]
    fn mask_value_masks_object_keys_and_keeps_their_values() {
        let token = gh();
        let m = Masker::from_pairs(&[("dummy-key-value-12345", "env:RD4_UNIT_KEY")]);
        let mut v = json!({
            "plain": 1,
            "dummy-key-value-12345": "a",
            "meta": {(format!("x {token}")): true, "ok": "dummy-key-value-12345"},
            "list": [{(token.clone()): [1, 2]}, {"clean": null}],
        });
        let c = m.mask_value(&mut v);
        assert_eq!(
            v,
            json!({
                "plain": 1,
                "[redacted:env:RD4_UNIT_KEY]": "a",
                "meta": {"x [redacted:github-token]": true, "ok": "[redacted:env:RD4_UNIT_KEY]"},
                "list": [{"[redacted:github-token]": [1, 2]}, {"clean": null}],
            })
        );
        assert_eq!(c, counts(&[("env:RD4_UNIT_KEY", 2), ("github-token", 2)]));
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains(&token[4..]), "{text}");
        assert!(!text.contains("dummy-key-value"), "{text}");

        // nothing to mask: the same document
        let clean = json!({"b": {"c": [1, {"d": "e"}]}, "a": "plain"});
        let mut same = clean.clone();
        assert!(m.mask_value(&mut same).is_empty());
        assert_eq!(serde_json::to_string(&same).unwrap(), serde_json::to_string(&clean).unwrap());
    }

    #[test]
    fn keys_that_mask_alike_get_numbered_suffixes_in_key_order() {
        let other = format!("{}{}", ["gh", "o_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        let m = none();
        // the gho_ key sorts before the ghp_ key; a literal `#2` key is kept
        let mut v = json!({
            (gh()): "second",
            (other.clone()): "first",
            "[redacted:github-token]#2": "literal",
        });
        let c = m.mask_value(&mut v);
        assert_eq!(
            v,
            json!({
                "[redacted:github-token]": "first",
                "[redacted:github-token]#2": "literal",
                "[redacted:github-token]#3": "second",
            })
        );
        assert_eq!(c, counts(&[("github-token", 2)]));
        // masking again changes nothing
        let again = v.clone();
        assert!(m.mask_value(&mut v).is_empty());
        assert_eq!(v, again);
    }

    #[test]
    fn secret_env_names_lists_aliases_of_known_values_only_when_whole() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let known = "dummyRd4UnitKnownValue1";
        let vars: Vec<(&str, String)> = vec![
            ("RD4_UNIT_ALIAS", known.to_string()),
            ("RD4_UNIT_ALIAS_PADDED", format!(" {known}\n")),
            ("RD4_UNIT_PATHLIKE", format!("x;{known}")),
            ("RD4_UNIT_OTHER", "dummyRd4UnitOtherValue2".to_string()),
        ];
        let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        let names = Masker::from_pairs(&[(known, "thoth:secrets.json")]).secret_env_names();
        let without = Masker::from_pairs(&[]).secret_env_names();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let has = |n: &str| names.iter().any(|x| x == n);
        assert!(has("RD4_UNIT_ALIAS"));
        assert!(has("RD4_UNIT_ALIAS_PADDED"));
        assert!(!has("RD4_UNIT_PATHLIKE"));
        assert!(!has("RD4_UNIT_OTHER"));
        assert!(!without.iter().any(|x| x.starts_with("RD4_UNIT_")), "listed without a known value");
    }

    fn html_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    #[test]
    fn with_variant_masks_the_html_escaped_form() {
        let value = "dummy<&>\"v1";
        let text = format!("{} {value}", html_escape(value));
        let plain = Masker::from_pairs(&[(value, "test")]);
        assert_eq!(plain.mask_text(&text).0, format!("{} [redacted:test]", html_escape(value)));
        let (out, c) = plain.with_variant(html_escape).mask_text(&text);
        assert_eq!(out, "[redacted:test] [redacted:test]");
        assert_eq!(c, counts(&[("test", 2)]));
    }

    #[test]
    fn event_omits_empty_counts_and_contains_only_metadata() {
        assert!(event("T1", None, "evidence_show", &Counts::default(), &[]).is_none());
        let m = Masker::from_pairs(&[("dummy-event-value-1", "env:RD_UNIT_EVENT")]);
        let (_, c) = m.mask_text(&format!("dummy-event-value-1 {}", gh()));
        let e = event("T1", Some("R1"), "review_input", &c, &["E002".to_string()]).unwrap();
        assert_eq!(e.task_id.as_deref(), Some("T1"));
        assert_eq!(e.run_id.as_deref(), Some("R1"));
        assert_eq!(e.event_type, "secret_redacted");
        assert_eq!(
            e.detail,
            json!({"exit": "review_input", "kinds": {"env:RD_UNIT_EVENT": 1, "github-token": 1}, "evidence": ["E002"]})
        );
        let text = serde_json::to_string(&e.detail).unwrap();
        assert!(!text.contains("dummy-event-value-1"));
        assert!(!text.contains(&gh()[4..]));
    }

    #[test]
    fn secret_env_names_lists_secret_named_and_formatted_vars_only() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let gitlab = format!("{}{}", ["gl", "pat-"].concat(), "dummy0123456789dummy");
        let vars: Vec<(&str, String)> = vec![
            ("RD2_TEST_TOKEN", "dummy-rd2-env-1".to_string()),
            ("RD2_PLAIN_NAME", gitlab.clone()),
            ("RD2_PLAIN", "hello".to_string()),
            ("RD2_PATHLIKE", "x;dummy-rd2-env-1".to_string()),
            ("RD2_DUP_TOKEN", gitlab.clone()),
            ("RD2_DUP_COPY", gitlab),
        ];
        let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        let names = Masker::load(None).secret_env_names();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let has = |n: &str| names.iter().any(|x| x == n);
        for listed in ["RD2_TEST_TOKEN", "RD2_PLAIN_NAME", "RD2_DUP_TOKEN", "RD2_DUP_COPY"] {
            assert!(has(listed), "{listed} is missing");
        }
        for left in ["RD2_PLAIN", "RD2_PATHLIKE"] {
            assert!(!has(left), "{left} is listed");
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert!(sorted == names, "the names are not sorted and unique");
    }

    #[test]
    fn diff_line_markers_are_a_boundary() {
        let token = gh();
        assert_eq!(masked(&format!("-{token}")), "-[redacted:github-token]");
        assert_eq!(masked(&format!("+{token}")), "+[redacted:github-token]");
        assert_eq!(masked(&format!("ctx\n-{token}\n+{token}\n")), "ctx\n-[redacted:github-token]\n+[redacted:github-token]\n");
        assert_eq!(masked(&format!("x\\n-{token}")), "x\\n-[redacted:github-token]");
        let openai = format!("{}{}", ["s", "k-"].concat(), "dummy0123456789dummy99");
        assert_eq!(masked(&format!("\n-{openai}")), "\n-[redacted:openai-key]");
        // a `-` in the middle of a line keeps RD-1a's rule
        assert_eq!(masked(&format!("a -{token}")), format!("a -{token}"));
        assert_eq!(masked(&format!("a-{token}")), format!("a-{token}"));
        let unchanged = |text: String| assert_eq!(masked(&text), text);
        unchanged("task-1-attempt-2".to_string());
        unchanged("-task-1-attempt-2".to_string());
        unchanged(format!("ta{openai}"));
        unchanged(format!("\n-ta{openai}"));
    }

    #[test]
    fn a_deleted_key_file_in_a_diff_is_masked_whole() {
        let lines = [
            ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat(),
            "MIIEdummyLINE1".to_string(),
            "AAAAdummyLINE2".to_string(),
            ["-----END ", "RSA PRIVATE KEY-----"].concat(),
        ];
        let deleted: String = lines.iter().map(|l| format!("-{l}\n")).collect();
        let diff = format!(
            "diff --git a/id_rsa b/id_rsa\ndeleted file mode 100644\n--- a/id_rsa\n+++ /dev/null\n@@ -1,4 +0,0 @@\n{deleted}"
        );
        let (out, c) = none().mask_text(&diff);
        for gone in ["MIIEdummyLINE1", "AAAAdummyLINE2", "BEGIN ", "END "] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(out.contains("@@ -1,4 +0,0 @@\n-[redacted:private-key]\n"), "{out}");
        assert_eq!(c, counts(&[("private-key", 1)]));
    }

    #[test]
    fn a_key_format_stops_at_an_exact_value_right_after_it() {
        let token = gh();
        let m = Masker::from_pairs(&[("dummyExact12345", "env:RD_UNIT_AFTER")]);
        let (out, c) = m.mask_text(&format!("a {token}dummyExact12345 b"));
        assert_eq!(out, "a [redacted:github-token][redacted:env:RD_UNIT_AFTER] b");
        assert!(!out.contains("dummyExact"), "{out}");
        assert_eq!(c, counts(&[("env:RD_UNIT_AFTER", 1), ("github-token", 1)]));
        let openai = format!("{}{}", ["s", "k-"].concat(), "dummy0123456789dummy99");
        let (out, c) = m.mask_text(&format!("{openai}dummyExact12345"));
        assert_eq!(out, "[redacted:openai-key][redacted:env:RD_UNIT_AFTER]");
        assert_eq!(c, counts(&[("env:RD_UNIT_AFTER", 1), ("openai-key", 1)]));
    }

    #[test]
    fn the_secret_masking_docs_are_not_masked_themselves() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let usage = std::fs::read_to_string(root.join("docs").join("phase2").join("usage.md"))
            .expect("docs/phase2/usage.md");
        // The contract is in the public tree, so it must be read.
        let adapter = std::fs::read_to_string(root.join("docs").join("adapter-usage.md"))
            .expect("docs/adapter-usage.md");
        let texts = vec![usage, adapter];
        for text in &texts {
            let (out, c) = none().mask_text(text);
            assert!(c.is_empty(), "{:?}", c.to_json());
            assert!(out == *text);
        }
    }

    #[test]
    fn marker_spans_finds_only_well_formed_markers() {
        let text = "[redacted:a] [redacted:] [redacted:b c] [redacted:[redacted:d]";
        let spans = marker_spans(text);
        let found: Vec<&str> = spans.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, ["[redacted:a]", "[redacted:d]"]);
        let long = format!("[redacted:{}]", "k".repeat(201));
        assert!(marker_spans(&long).is_empty());
        let max = format!("[redacted:{}]", "k".repeat(200));
        let spans = marker_spans(&max);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0], 0..max.len());
    }
}
