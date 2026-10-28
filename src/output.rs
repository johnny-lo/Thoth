use crate::redact::Masker;

/// What `emit` prints, masked (RD-1): JSON string leaves, or the human text.
/// With nothing to mask it is exactly what was printed before masking.
pub(crate) fn render(json_mode: bool, value: &serde_json::Value, human: &str, masker: &Masker) -> String {
    if json_mode {
        let mut masked = value.clone();
        masker.mask_value(&mut masked);
        match serde_json::to_string(&masked) {
            Ok(s) => s,
            Err(e) => format!("{{\"error\":\"serialize failed: {e}\"}}"),
        }
    } else {
        masker.mask_text(human).0
    }
}

/// Print either a JSON document or a human line, never both.
pub fn emit(json_mode: bool, value: &serde_json::Value, human: &str) {
    println!("{}", render(json_mode, value, human, &Masker::current()));
}

/// What a line thoth writes to stderr becomes (RD-4): masked like stdout and
/// `error:`, markers only and no `secret_redacted`. With nothing to mask it
/// is `text` as it was.
pub fn render_err(text: &str, masker: &Masker) -> String {
    masker.mask_text(text).0
}

/// The one way thoth writes a line of its own to stderr, other than
/// `emit_err` (RD-4): `masked_eprintln!` (main.rs) formats into this.
pub fn eprintln_masked(text: &str) {
    eprintln!("{}", render_err(text, &Masker::current()));
}

/// `text` masked, without a line feed added, for a prompt or text that
/// carries its own; a failed write is ignored, as clap and the prompt did.
pub fn eprint_masked(text: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = write!(err, "{}", render_err(text, &Masker::current()));
    let _ = err.flush();
}

/// `eprintln_masked` that ignores a failed write, for code that must not
/// panic when stderr is gone.
pub fn try_eprintln_masked(text: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{}", render_err(text, &Masker::current()));
}

/// Print an error to stderr in the requested shape.
pub fn emit_err(json_mode: bool, message: &str) {
    let message = Masker::current().mask_text(message).0;
    if json_mode {
        let v = serde_json::json!({ "ok": false, "error": message });
        eprintln!("{}", serde_json::to_string(&v).unwrap_or_default());
    } else {
        eprintln!("error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_masks_json_string_leaves_and_human_text() {
        let m = Masker::from_pairs(&[("dummy-output-value-1", "env:RD_OUT_X")]);
        let value = json!({"a": "x dummy-output-value-1 y", "n": 42, "list": ["dummy-output-value-1"]});
        let out = render(true, &value, "", &m);
        assert!(!out.contains("dummy-output-value-1"));
        assert!(out.contains("[redacted:"));
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        let keys: Vec<&String> = back.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["a", "list", "n"]);
        assert_eq!(back["n"], 42);

        let human = "say dummy-output-value-1 now";
        let out = render(false, &value, human, &m);
        assert!(!out.contains("dummy-output-value-1"));
        assert!(out.contains("[redacted:"));

        let none = Masker::from_pairs(&[]);
        let clean = json!({"a": "plain", "n": 1});
        assert_eq!(render(true, &clean, "", &none), serde_json::to_string(&clean).unwrap());
        assert_eq!(render(false, &clean, "plain human", &none), "plain human");
    }

    #[test]
    fn render_err_leaves_only_markers_and_keeps_clean_lines() {
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        let m = Masker::from_pairs(&[("dummy-err-value-1", "env:RD4_ERR_X")]);
        let line = format!("warning: automatic score refresh failed: HTTP 401 for {token} (dummy-err-value-1)");
        let out = render_err(&line, &m);
        assert_eq!(
            out,
            "warning: automatic score refresh failed: HTTP 401 for [redacted:github-token] ([redacted:env:RD4_ERR_X])"
        );
        assert!(!out.contains(&token[4..20]));
        let clean = "warning: arena refresh failed: timed out; keeping the cached table";
        assert_eq!(render_err(clean, &m), clean);
        assert_eq!(render_err(clean, &Masker::from_pairs(&[])), clean);
    }

    /// RD-4: thoth writes to stderr only through this module, so every line
    /// is masked. The fakes in src/bin are not thoth; windows.rs only reads
    /// the stderr handle to pass it on.
    #[test]
    fn no_raw_stderr_write_is_left_in_src() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![src.clone()];
        let mut found: Vec<String> = Vec::new();
        while let Some(p) = files.pop() {
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "bin") {
                    continue;
                }
                for e in std::fs::read_dir(&p).unwrap() {
                    files.push(e.unwrap().path());
                }
                continue;
            }
            let rel = p.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
            if !rel.ends_with(".rs") || rel == "output.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, line) in text.lines().enumerate() {
                for word in ["eprintln!", "eprint!", "stderr()"] {
                    let raw = line.match_indices(word).any(|(at, _)| {
                        let before = line[..at].chars().next_back();
                        !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    });
                    let handle = rel == "process/windows.rs" && line.contains("stderr().as_raw_handle()");
                    if raw && !handle && !line.trim_start().starts_with("//") {
                        found.push(format!("src/{rel}:{}: {}", i + 1, line.trim()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "raw stderr writes:\n{}", found.join("\n"));
    }
}
