use serde::{Deserialize, Serialize};

fn default_cwd() -> String {
    ".".to_string()
}

/// v6 section 10.4 `.agent/result.json`, the worker's only reporting channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub claims: Vec<WorkerClaim>,
    #[serde(default)]
    pub verification_requests: Vec<VerificationRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerClaim {
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// RES-1a: where a `research` claim comes from. Required (non-empty) on a
    /// `research` claim and forbidden (must be empty) on every other kind.
    #[serde(default)]
    pub sources: Vec<ClaimSource>,
}

/// One source of a `research` claim: a page the worker actually opened and a
/// verbatim excerpt of it that supports the claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimSource {
    pub url: String,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRequest {
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

/// v6 section 10.5 comments the claims.kind vocabulary. `protected_change`
/// (P7-2) declares one modified or deleted protected file; see ingest rule 6.
/// `research` (RES-1a) is a sourced factual claim; see `RESEARCH_CLAIM_KIND`.
pub const CLAIM_KINDS: [&str; 6] = [
    "test_result",
    "doc_reference",
    "completion",
    "behavior",
    crate::protected::CLAIM_KIND,
    RESEARCH_CLAIM_KIND,
];
/// The claim kind that carries `sources`; a research task's completed delivery
/// needs at least one (ingest rule 7).
pub const RESEARCH_CLAIM_KIND: &str = "research";
/// URL schemes a research source may use, compared case-insensitively.
const SOURCE_URL_SCHEMES: [&str; 3] = ["http://", "https://", "file://"];
/// The worker may only declare these outcomes; anything else is UNVERIFIED.
pub const RESULT_STATUSES: [&str; 2] = ["completed", "blocked"];

#[derive(Debug, thiserror::Error)]
pub enum ResultError {
    #[error("result.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("result.json is missing required field '{0}'")]
    MissingField(&'static str),
    #[error("result.json task_id mismatch: expected '{expected}', got '{got}'")]
    TaskIdMismatch { expected: String, got: String },
    #[error("result.json status must be completed or blocked, got '{0}'")]
    BadStatus(String),
    #[error("result.json claim kind must be one of test_result/doc_reference/completion/behavior/protected_change/research, got '{0}'")]
    BadClaimKind(String),
    /// `claim` is 1-based, like every other per-claim message in ingest.
    #[error("result.json claim {claim} is kind research but its 'sources' is empty")]
    ResearchWithoutSources { claim: usize },
    #[error("result.json claim {claim} source {source_no}: 'url' must start with http://, https:// or file:// and contain no whitespace, got '{url}'")]
    BadSourceUrl {
        claim: usize,
        source_no: usize,
        url: String,
    },
    #[error("result.json claim {claim} source {source_no}: 'quote' is blank")]
    BlankSourceQuote { claim: usize, source_no: usize },
    #[error("result.json claim {claim} is kind '{kind}' but has 'sources'; only research claims carry sources")]
    SourcesOnNonResearchClaim { claim: usize, kind: String },
}

/// Whether `url` is acceptable as a research source: after trimming, it
/// starts with `http://`, `https://` or `file://` (any case), has at least one
/// character after that, and contains no whitespace or control characters.
pub fn is_valid_source_url(url: &str) -> bool {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let Some(scheme) = SOURCE_URL_SCHEMES.iter().find(|s| lower.starts_with(**s)) else {
        return false;
    };
    url.len() > scheme.len() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn parse_result(json: &str, expected_task_id: &str) -> Result<WorkerResult, ResultError> {
    let r: WorkerResult = serde_json::from_str(json)?;
    if r.task_id.trim().is_empty() {
        return Err(ResultError::MissingField("task_id"));
    }
    if r.task_id != expected_task_id {
        return Err(ResultError::TaskIdMismatch {
            expected: expected_task_id.to_string(),
            got: r.task_id.clone(),
        });
    }
    if r.status.trim().is_empty() {
        return Err(ResultError::MissingField("status"));
    }
    if !RESULT_STATUSES.contains(&r.status.as_str()) {
        return Err(ResultError::BadStatus(r.status.clone()));
    }
    if r.summary.trim().is_empty() {
        return Err(ResultError::MissingField("summary"));
    }
    for c in &r.claims {
        if !CLAIM_KINDS.contains(&c.kind.as_str()) {
            return Err(ResultError::BadClaimKind(c.kind.clone()));
        }
    }
    for (i, c) in r.claims.iter().enumerate() {
        let claim = i + 1;
        if c.kind != RESEARCH_CLAIM_KIND {
            if !c.sources.is_empty() {
                return Err(ResultError::SourcesOnNonResearchClaim {
                    claim,
                    kind: c.kind.clone(),
                });
            }
            continue;
        }
        if c.sources.is_empty() {
            return Err(ResultError::ResearchWithoutSources { claim });
        }
        for (j, s) in c.sources.iter().enumerate() {
            let source_no = j + 1;
            if !is_valid_source_url(&s.url) {
                return Err(ResultError::BadSourceUrl {
                    claim,
                    source_no,
                    url: s.url.clone(),
                });
            }
            if s.quote.trim().is_empty() {
                return Err(ResultError::BlankSourceQuote { claim, source_no });
            }
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "01HZZZTASKIDTASKIDTASKID00";

    fn good() -> String {
        format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"did the thing",
                "claims":[{{"kind":"test_result","text":"cargo test green","evidence":["E001"]}}],
                "verification_requests":[{{"cmd":"cargo test","cwd":"."}}]}}"#
        )
    }

    #[test]
    fn a_well_formed_result_parses() {
        let r = parse_result(&good(), TASK).unwrap();
        assert_eq!(r.task_id, TASK);
        assert_eq!(r.status, "completed");
        assert_eq!(r.summary, "did the thing");
        assert_eq!(r.claims.len(), 1);
        assert_eq!(r.claims[0].kind, "test_result");
        assert_eq!(r.claims[0].evidence, vec!["E001".to_string()]);
        assert_eq!(r.verification_requests[0].cmd, "cargo test");
        assert_eq!(r.verification_requests[0].cwd, ".");
    }

    #[test]
    fn claims_and_verification_requests_are_optional_and_cwd_defaults_to_dot() {
        let j = format!(r#"{{"task_id":"{TASK}","status":"completed","summary":"s"}}"#);
        let r = parse_result(&j, TASK).unwrap();
        assert!(r.claims.is_empty());
        assert!(r.verification_requests.is_empty());

        let j2 = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "verification_requests":[{{"cmd":"x"}}]}}"#
        );
        let r2 = parse_result(&j2, TASK).unwrap();
        assert_eq!(r2.verification_requests[0].cwd, ".");
    }

    #[test]
    fn a_missing_task_id_field_is_rejected() {
        // brief section 4 FAKE_SCENARIO=bad_result drops exactly this field
        let j = r#"{"status":"completed","summary":"s","claims":[]}"#;
        assert!(matches!(
            parse_result(j, TASK),
            Err(ResultError::MissingField("task_id"))
        ));
    }

    #[test]
    fn a_wrong_task_id_is_rejected_and_reports_both_values() {
        // no interpolation here, so a plain literal - `format!` with no
        // arguments would trip clippy::useless_format under -D warnings
        let j = r#"{"task_id":"OTHER","status":"completed","summary":"s"}"#;
        match parse_result(j, TASK) {
            Err(ResultError::TaskIdMismatch { expected, got }) => {
                assert_eq!(expected, TASK);
                assert_eq!(got, "OTHER");
            }
            other => panic!("expected TaskIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_blank_status_is_rejected() {
        // #9 (final review): the branch exists but only had a blank-summary
        // twin; a blank status must be rejected the same way.
        let j = format!(r#"{{"task_id":"{TASK}","status":"   ","summary":"s"}}"#);
        assert!(matches!(
            parse_result(&j, TASK),
            Err(ResultError::MissingField("status"))
        ));
    }

    #[test]
    fn a_blank_summary_is_rejected() {
        let j = format!(r#"{{"task_id":"{TASK}","status":"completed","summary":"  "}}"#);
        assert!(matches!(
            parse_result(&j, TASK),
            Err(ResultError::MissingField("summary"))
        ));
    }

    #[test]
    fn an_unknown_status_is_rejected() {
        let j = format!(r#"{{"task_id":"{TASK}","status":"vibes","summary":"s"}}"#);
        match parse_result(&j, TASK) {
            Err(ResultError::BadStatus(v)) => assert_eq!(v, "vibes"),
            other => panic!("expected BadStatus, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_claim_kind_is_rejected() {
        let j = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"vibes","text":"t"}}]}}"#
        );
        match parse_result(&j, TASK) {
            Err(ResultError::BadClaimKind(v)) => assert_eq!(v, "vibes"),
            other => panic!("expected BadClaimKind, got {other:?}"),
        }
    }

    /// P7-2 / AC4: `protected_change` is a claim kind.
    #[test]
    fn a_protected_change_claim_parses() {
        let j = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"protected_change","text":"tests/x.rs: the old assertion contradicted AC2"}}]}}"#
        );
        let r = parse_result(&j, TASK).unwrap();
        assert_eq!(r.claims[0].kind, "protected_change");
        assert!(r.claims[0].evidence.is_empty());
        assert!(CLAIM_KINDS.contains(&"protected_change"));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(parse_result("nope", TASK), Err(ResultError::Json(_))));
    }

    // RES-1a AC6: research claims and their sources.

    fn with_claims(claims: &str) -> String {
        format!(r#"{{"task_id":"{TASK}","status":"completed","summary":"s","claims":{claims}}}"#)
    }

    #[test]
    fn a_research_claim_with_sources_parses_and_keeps_them() {
        let j = with_claims(
            r#"[{"kind":"completion","text":"done"},
                {"kind":"research","text":"X is Y","evidence":[],
                 "sources":[{"url":"https://example.org/a","quote":"X is Y."},
                            {"url":"file:///C:/Users/x/a.txt","quote":"also"}]}]"#,
        );
        let r = parse_result(&j, TASK).unwrap();
        assert!(r.claims[0].sources.is_empty(), "sources defaults to empty");
        assert_eq!(r.claims[1].kind, "research");
        assert_eq!(
            r.claims[1].sources,
            vec![
                ClaimSource {
                    url: "https://example.org/a".into(),
                    quote: "X is Y.".into()
                },
                ClaimSource {
                    url: "file:///C:/Users/x/a.txt".into(),
                    quote: "also".into()
                },
            ]
        );
    }

    #[test]
    fn a_research_claim_without_sources_is_rejected_and_names_the_claim() {
        for sources in ["", r#","sources":[]"#] {
            let j = with_claims(&format!(
                r#"[{{"kind":"completion","text":"done"}},{{"kind":"research","text":"t"{sources}}}]"#
            ));
            let e = parse_result(&j, TASK).unwrap_err();
            assert!(
                matches!(e, ResultError::ResearchWithoutSources { claim: 2 }),
                "{e:?}"
            );
            assert!(e.to_string().contains("claim 2"), "{e}");
            assert!(e.to_string().contains("sources"), "{e}");
        }
    }

    #[test]
    fn a_research_source_with_a_bad_url_is_rejected_and_names_the_field() {
        let j = with_claims(
            r#"[{"kind":"research","text":"t","sources":[
                {"url":"https://ok.example/a","quote":"q"},{"url":"ftp://x","quote":"q"}]}]"#,
        );
        let e = parse_result(&j, TASK).unwrap_err();
        match &e {
            ResultError::BadSourceUrl {
                claim,
                source_no,
                url,
            } => {
                assert_eq!((*claim, *source_no, url.as_str()), (1, 2, "ftp://x"));
            }
            other => panic!("expected BadSourceUrl, got {other:?}"),
        }
        let text = e.to_string();
        assert!(text.contains("claim 1") && text.contains("'url'"), "{text}");
    }

    #[test]
    fn a_research_source_with_a_blank_quote_is_rejected() {
        let j = with_claims(
            r#"[{"kind":"research","text":"t","sources":[{"url":"https://a.example","quote":"  \n "}]}]"#,
        );
        let e = parse_result(&j, TASK).unwrap_err();
        assert!(
            matches!(
                e,
                ResultError::BlankSourceQuote {
                    claim: 1,
                    source_no: 1
                }
            ),
            "{e:?}"
        );
        assert!(e.to_string().contains("'quote'"), "{e}");
    }

    #[test]
    fn a_source_missing_its_url_or_quote_is_a_json_error() {
        for s in [r#"{"quote":"q"}"#, r#"{"url":"https://a.example"}"#] {
            let j = with_claims(&format!(r#"[{{"kind":"research","text":"t","sources":[{s}]}}]"#));
            assert!(
                matches!(parse_result(&j, TASK), Err(ResultError::Json(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn a_non_research_claim_with_sources_is_rejected() {
        let j = with_claims(
            r#"[{"kind":"completion","text":"done","sources":[{"url":"https://a.example","quote":"q"}]}]"#,
        );
        let e = parse_result(&j, TASK).unwrap_err();
        match &e {
            ResultError::SourcesOnNonResearchClaim { claim, kind } => {
                assert_eq!((*claim, kind.as_str()), (1, "completion"));
            }
            other => panic!("expected SourcesOnNonResearchClaim, got {other:?}"),
        }
        assert!(e.to_string().contains("'sources'"), "{e}");
        // an explicitly empty list on another kind is fine
        let ok = with_claims(r#"[{"kind":"completion","text":"done","sources":[]}]"#);
        assert!(parse_result(&ok, TASK).is_ok());
    }

    #[test]
    fn the_bad_claim_kind_message_lists_research() {
        let j = with_claims(r#"[{"kind":"vibes","text":"t"}]"#);
        let text = parse_result(&j, TASK).unwrap_err().to_string();
        assert!(text.contains("protected_change/research"), "{text}");
        assert!(CLAIM_KINDS.contains(&"research"));
    }

    #[test]
    fn source_urls_accept_http_https_and_file_in_any_case() {
        for ok in [
            "http://example.org",
            "https://example.org/a?b=c#d",
            "HTTPS://EXAMPLE.ORG/",
            "Http://x",
            "file:///C:/Users/x/a.txt",
            "file:///home/x/a.txt",
            "FILE:///tmp/a",
            "  https://example.org/padded  ",
            "https://example.org/%E4%B8%AD",
            "https://例子.測試/路徑",
        ] {
            assert!(is_valid_source_url(ok), "{ok:?} should be valid");
        }
    }

    #[test]
    fn source_urls_reject_other_schemes_bare_schemes_and_whitespace() {
        for bad in [
            "",
            "   ",
            "ftp://x",
            "http://",
            "https://   ",
            "https:// a b",
            "https://a b",
            "https://a\tb",
            "https://a\u{0007}b",
            "example.com/a",
            "//example.com/a",
            "mailto:a@example.org",
            "javascript:alert(1)",
            "httpss://x",
        ] {
            assert!(!is_valid_source_url(bad), "{bad:?} should be invalid");
        }
    }
}
