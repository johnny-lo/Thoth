use crate::db::state::{self, NewEvent, Run};
use crate::ids;
use crate::model::event_type;
use crate::result_json;
use rusqlite::Connection;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Ok { summary: String, claim_count: usize },
    Invalid { reason: String },
    /// The run did not reach a state where ingest is even attempted.
    Skipped,
}

/// v6 §2.5 rule 2: these two claim kinds are worthless without a citation.
/// `completion` and `behavior` are exempt - they are summaries of the worker's
/// own judgement, which is what a `claim` is for (v6 §2.2 rule 2).
pub const CITATION_REQUIRED_KINDS: [&str; 2] = ["test_result", "doc_reference"];

/// Records the `result_invalid` event and returns the outcome.
///
/// `detail` carries the rule number and whatever identified the offending claim,
/// which is what brief §7 asks for ("events.result_invalid 記 reason 與
/// claim_id"); `reason` is merged in so the two never drift.
fn reject(
    conn: &Connection,
    run: &Run,
    reason: String,
    mut detail: serde_json::Value,
) -> IngestOutcome {
    if let Some(obj) = detail.as_object_mut() {
        obj.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.clone()),
        );
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::RESULT_INVALID.to_string(),
            detail,
        },
    );
    IngestOutcome::Invalid { reason }
}

/// The one implementation of the worker transport contract (v6 §10.3): validate,
/// resolve citations, merge claims, then rebuild their evidence links. Used by
/// the runner and by `thoth submit`. Renamed from `ingest_text` when the
/// citation rules arrived; the `(kind, text)` merge inside it is unchanged from
/// commit 6153a9b and is what keeps `claim_id` stable across a retried submit.
///
/// Implements v6 §2.5's automatic UNVERIFIED conditions, in brief §7's order,
/// and all of them **before** any write: an invalid document must leave the
/// previous claim set and its links exactly as they were.
///
/// 1. the document fails schema validation
/// 2. a `test_result` / `doc_reference` claim cites nothing
/// 3. a cited code is not evidence of this task
/// 4. a cited evidence's blob is missing or no longer hashes to its digest
/// 5. the task is High Risk and a claim cites no `control_plane` evidence
///    (`research` claims excepted)
/// 6. the run's diff modified or deleted a protected file (or added a file
///    named like a check word) that no `protected_change` claim declares
/// 7. the task is a research task, the status is `completed`, and no claim is
///    of kind `research`
///
/// `task_risk` is `tasks.risk`, not `spec.risk`: the task row is what
/// `task update-spec` maintains and what the Supervisor can see.
pub fn ingest_with_evidence(
    conn: &Connection,
    run: &Run,
    task_risk: &str,
    json_text: &str,
) -> IngestOutcome {
    // Rule 1.
    let parsed = match result_json::parse_result(json_text, &run.task_id) {
        Ok(p) => p,
        Err(e) => {
            return reject(
                conn,
                run,
                e.to_string(),
                serde_json::json!({ "rule": 1, "bytes": json_text.len() }),
            )
        }
    };

    // Rules 2-5. Every citation is resolved to a real row here, so the write
    // phase below has nothing left to decide.
    let high_risk = task_risk == crate::model::Risk::High.as_str();
    let mut resolved: Vec<Vec<state::Evidence>> = Vec::new();
    for (i, c) in parsed.claims.iter().enumerate() {
        let n = i + 1;
        if CITATION_REQUIRED_KINDS.contains(&c.kind.as_str()) && c.evidence.is_empty() {
            return reject(
                conn,
                run,
                format!("claim {n} of kind '{}' cites no evidence", c.kind),
                serde_json::json!({
                    "rule": 2, "claim_index": i, "claim_kind": c.kind, "claim_text": c.text,
                }),
            );
        }

        let mut rows: Vec<state::Evidence> = Vec::new();
        for raw_code in &c.evidence {
            // A code is always `E` plus digits, so a worker that wrote ` e001 `
            // meant `E001`. Normalising here keeps the three citation readers
            // symmetric: `evidence show` upper-cases its argument and
            // `review_json::cited_codes` upper-cases the reviewer's.
            let code = raw_code.trim().to_uppercase();
            // L5: a claim that cites the same code twice gets one link, not two -
            // `cited_codes` de-duplicates on the reviewer's side, so the worker's
            // side does too. Otherwise `evidence_refs` and `review show`'s claim
            // table would report `["E001","E001"]` for a document that said
            // nothing twice.
            if rows.iter().any(|r| r.code == code) {
                continue;
            }
            let row = match state::get_evidence_by_code(conn, &run.task_id, &code) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return reject(
                        conn,
                        run,
                        format!(
                            "claim {n} cites {code}, which is not evidence of task {}",
                            run.task_id
                        ),
                        serde_json::json!({ "rule": 3, "claim_index": i, "code": code }),
                    )
                }
                Err(e) => {
                    return reject(
                        conn,
                        run,
                        format!("reading evidence {code} failed: {e}"),
                        serde_json::json!({ "rule": 3, "claim_index": i, "code": code }),
                    )
                }
            };
            if let (Some(path), Some(sha)) = (row.blob_path.as_deref(), row.sha256.as_deref()) {
                if !crate::blobs::verify(Path::new(path), sha) {
                    return reject(
                        conn,
                        run,
                        format!(
                            "claim {n} cites {code}, whose blob is missing or no longer matches \
                             its sha256"
                        ),
                        serde_json::json!({
                            "rule": 4, "claim_index": i, "code": code,
                            "blob_path": path, "sha256": sha,
                        }),
                    );
                }
            }
            rows.push(row);
        }

        // A `research` claim stands on its `sources`, which no command ever
        // fetched, so control_plane evidence cannot back it; its check is the
        // source audit (RES-1b), not rule 5.
        if high_risk
            && c.kind != result_json::RESEARCH_CLAIM_KIND
            && !rows
                .iter()
                .any(|r| r.collected_by == crate::evidence::COLLECTED_BY_CONTROL_PLANE)
        {
            return reject(
                conn,
                run,
                format!(
                    "task risk is high, so claim {n} must cite at least one control_plane \
                     evidence"
                ),
                serde_json::json!({
                    "rule": 5, "claim_index": i, "claim_kind": c.kind,
                    "cited": c.evidence, "risk": task_risk,
                }),
            );
        }
        resolved.push(rows);
    }

    // Rule 6 (P7-2). The control plane worked out, from the diff, which
    // protected files this run modified or deleted (`protected_changes` on the
    // run's newest diff evidence). Each one must be declared by a
    // `protected_change` claim whose text starts with the path. The claim
    // cites nothing - the diff evidence does not exist yet while the worker is
    // writing result.json - and whether the change weakened a check is left to
    // the reviewer. A run with no diff evidence has nothing to compare against;
    // the list is read leniently (`changes_in_detail`), so one written before
    // the key existed has nothing either.
    let undeclared = match crate::protected::latest_diff(conn, run) {
        Ok(latest) => latest
            .map(|d| crate::protected::changes_in_detail(&d.detail))
            .unwrap_or_default()
            .into_iter()
            .find(|ch| {
                !parsed.claims.iter().any(|c| {
                    c.kind == crate::protected::CLAIM_KIND
                        && crate::protected::claim_declares(&c.text, &ch.path)
                })
            }),
        Err(e) => {
            return reject(
                conn,
                run,
                format!("reading the run's diff evidence failed: {e}"),
                serde_json::json!({ "rule": 6 }),
            )
        }
    };
    if let Some(ch) = undeclared {
        return reject(
            conn,
            run,
            format!(
                "claim missing for protected file {} ({}): add a claim of kind protected_change \
                 whose text starts with the path and explains the change",
                ch.path, ch.status
            ),
            serde_json::json!({
                "rule": 6, "path": ch.path, "status": ch.status, "kind": ch.kind,
            }),
        );
    }

    // Rule 7 (RES-1a). A research task's completed delivery is its sourced
    // claims; without one the source audit would have nothing to check. The
    // run's spec copy keeps `task_type`, and a blocked delivery is not judged.
    if crate::spec::is_research_task(&run.spec)
        && parsed.status == "completed"
        && !parsed
            .claims
            .iter()
            .any(|c| c.kind == result_json::RESEARCH_CLAIM_KIND)
    {
        return reject(
            conn,
            run,
            "a completed delivery of a research task needs at least one claim of kind research \
             with its sources"
                .to_string(),
            serde_json::json!({ "rule": 7 }),
        );
    }

    // Commit 6153a9b's re-ingest merge, kept exactly as it is: an incoming
    // claim that matches an existing (kind, text) keeps that row's `claim_id`
    // and is not rewritten; only a genuinely new one is inserted; and an id
    // nothing matched is deleted with `delete_claim`. Phase 2 adds one thing to
    // it - the run's `claim_evidence` rows are cleared first, before any claim
    // is inserted or deleted, and rebuilt at the end - so a link can never
    // outlive the claim it belonged to (Open question 2). The read, the writes
    // and the event commit atomically, so a crash mid-ingest leaves either the
    // whole batch or none of it.
    // `write_tx` (BEGIN IMMEDIATE): the claims read below decides the writes
    // that follow, so a deferred transaction could fail at its first write with
    // SQLITE_BUSY after another connection's commit instead of waiting; see
    // `evidence::insert_row` for the race that surfaced it.
    let tx = match crate::db::write_tx(conn) {
        Ok(tx) => tx,
        Err(e) => {
            return reject(
                conn,
                run,
                format!("starting ingest transaction failed: {e}"),
                serde_json::json!({ "rule": 0 }),
            )
        }
    };

    let existing = match state::list_claims_for_run(&tx, &run.run_id) {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("reading existing claims failed: {e}");
            // N1 (Task 8 carry-over): `tx` is still open here, and an event
            // appended through `conn` would execute inside that same open
            // transaction and be rolled back with everything else. Drop (roll
            // back) `tx` first, then append the event as its own write.
            drop(tx);
            return reject(conn, run, reason, serde_json::json!({ "rule": 0 }));
        }
    };

    // Clearing the links here, first, is what makes the stale-claim deletes
    // below safe: `delete_claim_evidence_for_run` finds a link by joining
    // through `claims.run_id`, so a claim deleted before its links would leave
    // them unreachable and orphaned.
    if let Err(e) = state::delete_claim_evidence_for_run(&tx, &run.run_id) {
        let reason = format!("clearing existing claim links failed: {e}");
        drop(tx);
        return reject(conn, run, reason, serde_json::json!({ "rule": 0 }));
    }

    // Group existing claims by (kind, text). More than one existing row can
    // share the same (kind, text); each is matched in insertion order so an
    // incoming claim never reuses the same stable id twice.
    let mut by_key: HashMap<(String, String), VecDeque<String>> = HashMap::new();
    for claim in existing {
        by_key
            .entry((claim.kind, claim.text))
            .or_default()
            .push_back(claim.claim_id);
    }

    // The id each incoming claim ended up under, in document order, so the
    // linking pass below knows what to attach its resolved evidence to.
    let mut claim_ids: Vec<String> = Vec::with_capacity(parsed.claims.len());
    for c in &parsed.claims {
        let key = (c.kind.clone(), c.text.clone());
        if let Some(existing_id) = by_key.get_mut(&key).and_then(|q| q.pop_front()) {
            claim_ids.push(existing_id);
            continue;
        }
        let claim_id = ids::new_claim_id();
        if let Err(e) = state::insert_claim(&tx, &claim_id, &run.run_id, &c.kind, &c.text) {
            let reason = format!("writing claim failed: {e}");
            drop(tx);
            return reject(
                conn,
                run,
                reason,
                serde_json::json!({ "rule": 0, "claim_kind": c.kind }),
            );
        }
        claim_ids.push(claim_id);
    }

    // Whatever id is still sitting in `by_key` was never claimed by an incoming
    // claim, so it no longer belongs to this run's current claim set. Its links
    // are already gone.
    for (_, remaining) in by_key {
        for stale_id in remaining {
            if let Err(e) = state::delete_claim(&tx, &stale_id) {
                let reason = format!("clearing a superseded claim failed: {e}");
                drop(tx);
                return reject(conn, run, reason, serde_json::json!({ "rule": 0 }));
            }
        }
    }

    // Rebuild the links. Every code was resolved to a real row above, so the
    // only way an insert fails here is a database-level problem.
    let mut evidence_refs = serde_json::Map::new();
    for ((claim_id, rows), c) in claim_ids
        .iter()
        .zip(resolved.iter())
        .zip(parsed.claims.iter())
    {
        for row in rows {
            if let Err(e) = state::insert_claim_evidence(&tx, claim_id, &row.evidence_id) {
                let reason = format!("writing claim evidence link failed: {e}");
                drop(tx);
                return reject(
                    conn,
                    run,
                    reason,
                    serde_json::json!({
                        "rule": 0,
                        "claim_id": claim_id,
                        "claim_kind": c.kind,
                        "code": row.code,
                    }),
                );
            }
        }
        evidence_refs.insert(
            claim_id.clone(),
            serde_json::Value::Array(
                rows.iter()
                    .map(|r| serde_json::Value::String(r.code.clone()))
                    .collect(),
            ),
        );
    }

    // The raw document is kept so nothing the worker sent is lost - in
    // particular `verification_requests`, which `thoth verify` reads back out
    // of here when the worktree's own copy is gone (Open question 15).
    let raw: serde_json::Value =
        serde_json::from_str(json_text).unwrap_or(serde_json::Value::Null);
    if let Err(e) = state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::RESULT_INGESTED.to_string(),
            detail: serde_json::json!({
                "summary": parsed.summary,
                "claim_count": parsed.claims.len(),
                "evidence_refs": evidence_refs,
                "result_json": raw,
            }),
        },
    ) {
        let reason = format!("recording result_ingested event failed: {e}");
        drop(tx);
        return reject(conn, run, reason, serde_json::json!({ "rule": 0 }));
    }

    if let Err(e) = tx.commit() {
        return reject(
            conn,
            run,
            format!("committing ingest transaction failed: {e}"),
            serde_json::json!({ "rule": 0 }),
        );
    }

    IngestOutcome::Ok {
        summary: parsed.summary,
        claim_count: parsed.claims.len(),
    }
}

pub fn ingest_path(conn: &Connection, run: &Run, task_risk: &str, path: &Path) -> IngestOutcome {
    match std::fs::read_to_string(path) {
        Ok(text) => ingest_with_evidence(conn, run, task_risk, &text),
        Err(e) => {
            let reason = format!("could not read result.json at {}: {e}", path.display());
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::RESULT_INVALID.to_string(),
                    detail: serde_json::json!({ "reason": reason, "rule": 1 }),
                },
            );
            IngestOutcome::Invalid { reason }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::state::{self, Run};
    use rusqlite::Connection;

    const TASK: &str = "01HZTASKIDTASKIDTASKIDTASK";

    fn demo_run_row(run_id: &str) -> Run {
        Run {
            run_id: run_id.into(),
            task_id: TASK.into(),
            attempt: 1,
            role: "implementer".into(),
            provider: "anthropic".into(),
            account: "claude-b".into(),
            model: "sonnet".into(),
            harness_session_id: None,
            config_dir: "C:/cfg".into(),
            allowed_tools: "Bash".into(),
            worktree: "C:/wt".into(),
            base_commit: "abc".into(),
            candidate_commit: None,
            spec: "{}".into(),
            status: "running".into(),
            interrupted_reason: None,
            exit_code: None,
            input_tokens: None,
            output_tokens: None,
            started_at: None,
            ended_at: None,
            runner_pid: None,
            worker_pid: None,
            heartbeat_at: None,
            cancel_requested: 0,
            timeout_seconds: 1800,
            run_dir: "C:/rd".into(),
            error_text: None,
            harness_bin: None,
        }
    }

    fn task_row(task_id: &str, risk: &str) -> state::Task {
        state::Task {
            task_id: task_id.into(),
            title: "t".into(),
            spec: "{}".into(),
            risk: risk.into(),
            status: "running".into(),
            base_commit: "abc".into(),
            created_at: "n".into(),
            updated_at: "n".into(),
        }
    }

    fn insert_task_row(conn: &Connection, risk: &str) {
        state::insert_task(conn, &task_row(TASK, risk)).unwrap();
    }

    fn setup() -> (Connection, Run) {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        insert_task_row(&conn, "normal");
        let run = demo_run_row("01HZRUNIDRUNIDRUNIDRUNID01");
        state::insert_run(&conn, &run).unwrap();
        (conn, run)
    }

    /// Seeds the two evidence rows the rule tests cite: `E001`, collected by
    /// the control plane, with a real blob in a tempdir; and `E002`, collected
    /// by the worker, with no blob at all. The returned `TempDir` must stay
    /// alive for as long as the test needs `E001`'s blob to exist.
    fn seed_evidence(conn: &Connection) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let (sha, bytes, path) =
            crate::blobs::put_bytes_into(dir.path(), b"cargo test: ok").unwrap();
        state::insert_evidence(
            conn,
            &state::Evidence {
                evidence_id: "EV1".into(),
                code: "E001".into(),
                task_id: TASK.into(),
                run_id: None,
                kind: "command".into(),
                collected_by: crate::evidence::COLLECTED_BY_CONTROL_PLANE.into(),
                label: "cargo test".into(),
                sha256: Some(sha),
                bytes: Some(bytes as i64),
                blob_path: Some(path.display().to_string()),
                detail: serde_json::json!({"cmd": "cargo test", "exit_code": 0}).to_string(),
                created_at: "2026-09-16T00:00:00Z".into(),
            },
        )
        .unwrap();
        state::insert_evidence(
            conn,
            &state::Evidence {
                evidence_id: "EV2".into(),
                code: "E002".into(),
                task_id: TASK.into(),
                run_id: None,
                kind: "source_document".into(),
                collected_by: "worker".into(),
                label: "official docs v3.2".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: "{}".into(),
                created_at: "2026-09-16T00:00:01Z".into(),
            },
        )
        .unwrap();
        dir
    }

    fn invalid_reason(outcome: IngestOutcome) -> String {
        match outcome {
            IngestOutcome::Invalid { reason } => reason,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    fn last_invalid_detail(conn: &Connection) -> serde_json::Value {
        let evs = state::events_since(conn, 0, Some(TASK)).unwrap();
        let row = evs
            .iter()
            .rev()
            .find(|e| e.event_type == "result_invalid")
            .expect("a result_invalid event");
        serde_json::from_str(&row.detail).unwrap()
    }

    /// A trigger makes only the `result_ingested` insert fail, so the failure
    /// path is forced: the run must still leave a `result_invalid` event, and
    /// the claims written inside the transaction must be rolled back.
    #[test]
    fn a_failed_result_ingested_write_still_leaves_a_result_invalid_event() {
        let (conn, run) = setup();
        conn.execute_batch(
            "CREATE TRIGGER no_ingested BEFORE INSERT ON events
             WHEN NEW.type = 'result_ingested'
             BEGIN SELECT RAISE(ABORT, 'forced'); END;",
        )
        .unwrap();
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"did it",
                 "claims":[{{"kind":"completion","text":"all AC met"}}],
                 "verification_requests":[]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("result_ingested event failed"), "{reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["reason"], reason.as_str());
        assert!(state::list_claims_for_run(&conn, &run.run_id)
            .unwrap()
            .is_empty());
    }

    /// Phase 1 test, edited (Open question 1): it cites `E001`, which rule 3
    /// now requires to exist, so the fixture seeds it. The new assertions are
    /// the `claim_evidence` link and the `evidence_refs` map. Name unchanged
    /// from HEAD.
    #[test]
    fn a_valid_result_writes_claims_and_a_result_ingested_event() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"did it",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E001"]}},
                           {{"kind":"completion","text":"all AC met"}}],
                 "verification_requests":[{{"cmd":"cargo test","cwd":"."}}]}}"#
        );
        match ingest_with_evidence(&conn, &run, "normal", &json) {
            IngestOutcome::Ok { summary, claim_count } => {
                assert_eq!(summary, "did it");
                assert_eq!(claim_count, 2);
            }
            other => panic!("expected Ok, got {other:?}"),
        }

        let claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(claims.len(), 2);
        let cited = claims
            .iter()
            .find(|c| c.kind == "test_result")
            .expect("the test_result claim");
        assert_eq!(
            state::list_evidence_for_claim(&conn, &cited.claim_id).unwrap(),
            vec!["EV1".to_string()],
            "the citation must become a claim_evidence row"
        );
        let uncited = claims
            .iter()
            .find(|c| c.kind == "completion")
            .expect("the completion claim");
        assert!(state::list_evidence_for_claim(&conn, &uncited.claim_id)
            .unwrap()
            .is_empty());

        let evs = state::events_since(&conn, 0, Some(TASK)).unwrap();
        let ingested: Vec<&state::EventRow> = evs
            .iter()
            .filter(|e| e.event_type == "result_ingested")
            .collect();
        assert_eq!(ingested.len(), 1);
        // the whole document is still kept, so `verify` can read
        // verification_requests back out of the event (Open question 15)
        assert!(ingested[0].detail.contains("verification_requests"));
        assert!(ingested[0].detail.contains("cargo test"));
        // brief §7: the event names which codes each claim ended up citing
        let detail: serde_json::Value = serde_json::from_str(&ingested[0].detail).unwrap();
        let refs = detail["evidence_refs"].as_object().unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[&cited.claim_id], serde_json::json!(["E001"]));
        assert_eq!(refs[&uncited.claim_id], serde_json::json!([]));
    }

    /// Phase 1 test, unchanged: rule 1 is the schema check that was already
    /// there.
    #[test]
    fn an_invalid_result_writes_no_claims_and_a_result_invalid_event() {
        let (conn, run) = setup();
        let json = r#"{"status":"completed","summary":"missing the task id"}"#;
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", json));
        assert!(reason.contains("task_id"), "reason was: {reason}");
        assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());
        // kept from HEAD: exactly one event, not zero and not a duplicate
        let evs = state::events_since(&conn, 0, Some(TASK)).unwrap();
        assert_eq!(
            evs.iter().filter(|e| e.event_type == "result_invalid").count(),
            1
        );
        assert_eq!(last_invalid_detail(&conn)["rule"], 1);
    }

    #[test]
    fn rule2_a_test_result_claim_with_no_citation_is_unverified() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"test_result","text":"green","evidence":[]}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("cites no evidence"), "reason was: {reason}");
        assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 2);
        assert_eq!(detail["claim_kind"], "test_result");

        // a doc_reference is the other citation-required kind
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"doc_reference","text":"per the docs"}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("doc_reference"), "reason was: {reason}");
    }

    #[test]
    fn rule2_leaves_completion_and_behavior_claims_alone() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"completion","text":"all AC met"}},
                           {{"kind":"behavior","text":"the button is blue now"}}]}}"#
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &json),
            IngestOutcome::Ok { claim_count: 2, .. }
        ));
    }

    /// L5: citing the same code twice is not an error, but it must not double the
    /// link. `review_json::cited_codes` de-duplicates on the reviewer's side; this
    /// keeps the two sides symmetric.
    #[test]
    fn a_claim_that_cites_one_code_twice_gets_one_link() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E001","E001","e001"]}}]}}"#
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &json),
            IngestOutcome::Ok { claim_count: 1, .. }
        ));
        let claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(
            state::list_evidence_for_claim(&conn, &claims[0].claim_id).unwrap(),
            vec!["EV1".to_string()]
        );
        let evs = state::events_since(&conn, 0, Some(TASK)).unwrap();
        let ingested = evs
            .iter()
            .rev()
            .find(|e| e.event_type == "result_ingested")
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&ingested.detail).unwrap();
        assert_eq!(
            detail["evidence_refs"][&claims[0].claim_id],
            serde_json::json!(["E001"]),
            "the recorded citation list is de-duplicated too"
        );
    }

    #[test]
    fn rule3_a_code_that_does_not_exist_on_this_task_is_unverified() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E999"]}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("E999"), "reason was: {reason}");
        assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 3);
        assert_eq!(detail["code"], "E999");

        // "not evidence of *this task*" also covers a code that does exist,
        // but on someone else's task: codes are allocated per task
        // (`idx_evidence_task_code` is on `(task_id, code)`), so one run may
        // not borrow another task's citation.
        const OTHER: &str = "01HZOTHERTASKOTHERTASKOTHE";
        state::insert_task(&conn, &task_row(OTHER, "normal")).unwrap();
        state::insert_evidence(
            &conn,
            &state::Evidence {
                evidence_id: "EV9".into(),
                code: "E777".into(),
                task_id: OTHER.into(),
                run_id: None,
                kind: "command".into(),
                collected_by: crate::evidence::COLLECTED_BY_CONTROL_PLANE.into(),
                label: "another task's cargo test".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: "{}".into(),
                created_at: "2026-09-16T00:00:02Z".into(),
            },
        )
        .unwrap();
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E777"]}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("E777"), "reason was: {reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 3);
        assert_eq!(detail["code"], "E777");
        assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());
    }

    #[test]
    fn rule4_a_blob_that_no_longer_matches_its_digest_is_unverified() {
        let (conn, run) = setup();
        let blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E001"]}}]}}"#
        );
        // clean first, so the failure below is clearly about the tampering
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &json),
            IngestOutcome::Ok { .. }
        ));

        let row = state::get_evidence_by_code(&conn, TASK, "E001").unwrap().unwrap();
        let path = std::path::PathBuf::from(row.blob_path.unwrap());
        std::fs::write(&path, b"cargo test: actually it failed").unwrap();
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("sha256"), "reason was: {reason}");
        assert_eq!(last_invalid_detail(&conn)["rule"], 4);

        // Fix round 1: the invariant the doc comment promises - a rejected
        // document leaves the previous claim set *and its links* exactly as
        // they were, because every rule runs before the transaction opens.
        let survivors = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(survivors.len(), 1, "the first ingest's claim must survive");
        assert_eq!(
            state::list_evidence_for_claim(&conn, &survivors[0].claim_id).unwrap(),
            vec!["EV1".to_string()],
            "and so must its link"
        );

        // and a blob that is simply gone is the same failure
        std::fs::remove_file(&path).unwrap();
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("missing"), "reason was: {reason}");
        let survivors = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(survivors.len(), 1);
        assert_eq!(
            state::list_evidence_for_claim(&conn, &survivors[0].claim_id).unwrap(),
            vec!["EV1".to_string()]
        );
        drop(blobs);
    }

    #[test]
    fn rule5_a_high_risk_claim_without_control_plane_evidence_is_unverified() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        insert_task_row(&conn, "high");
        let run = demo_run_row("01HZRUNIDHIGHRISKHIGHRISK1");
        state::insert_run(&conn, &run).unwrap();
        let _blobs = seed_evidence(&conn);

        // E002 is collected_by=worker: v6 §2.4 says a High Risk claim needs at
        // least one control_plane citation.
        let worker_only = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"doc_reference","text":"per the docs","evidence":["E002"]}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "high", &worker_only));
        assert!(reason.contains("control_plane"), "reason was: {reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 5);
        assert_eq!(detail["risk"], "high");

        // Open question 3: a claim citing nothing at all fails the same rule,
        // because the positive form of §2.4 is "at least one".
        let nothing_cited = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"completion","text":"all AC met"}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "high", &nothing_cited));
        assert!(reason.contains("control_plane"), "reason was: {reason}");

        // one control_plane citation is enough, even alongside a worker one
        let ok = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"doc_reference","text":"per the docs","evidence":["E002","E001"]}}]}}"#
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "high", &ok),
            IngestOutcome::Ok { claim_count: 1, .. }
        ));

        // and the same worker-only document is fine at normal risk
        let (conn2, run2) = setup();
        let _blobs2 = seed_evidence(&conn2);
        assert!(matches!(
            ingest_with_evidence(&conn2, &run2, "normal", &worker_only),
            IngestOutcome::Ok { .. }
        ));
    }

    /// A `diff` evidence row of `run_id` under `code`, whose detail lists
    /// `changes` as `(path, status, kind)`.
    fn seed_diff(conn: &Connection, run_id: &str, code: &str, changes: &[(&str, &str, &str)]) {
        let listed: Vec<serde_json::Value> = changes
            .iter()
            .map(|(p, s, k)| serde_json::json!({"path": p, "status": s, "kind": k}))
            .collect();
        state::insert_evidence(
            conn,
            &state::Evidence {
                evidence_id: format!("EV-{run_id}-{code}"),
                code: code.into(),
                task_id: TASK.into(),
                run_id: Some(run_id.into()),
                kind: "diff".into(),
                collected_by: crate::evidence::COLLECTED_BY_CONTROL_PLANE.into(),
                label: "git diff aaaaaaaa..bbbbbbbb".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: serde_json::json!({"protected_changes": listed}).to_string(),
                created_at: "2026-10-01T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn result_with_claims(claims: &str) -> String {
        format!(r#"{{"task_id":"{TASK}","status":"completed","summary":"s","claims":{claims}}}"#)
    }

    /// AC4, the positive case: the diff modified `tests/x.rs` and a
    /// `protected_change` claim starts with that path. The claim cites
    /// nothing, so this is also rule 2's exemption.
    #[test]
    fn rule6_a_declared_protected_change_is_accepted_without_a_citation() {
        let (conn, run) = setup();
        seed_diff(&conn, &run.run_id, "E003", &[("tests/x.rs", "modified", "test")]);
        let json = result_with_claims(
            r#"[{"kind":"protected_change","text":"tests/x.rs: the old assertion contradicted AC2"}]"#,
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &json),
            IngestOutcome::Ok { claim_count: 1, .. }
        ));
        let claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(claims[0].kind, "protected_change");
    }

    /// AC4, the negative cases: a text naming a longer path, a text naming
    /// only the file name, no claim at all, and the right text under another
    /// kind are all rule 6.
    #[test]
    fn rule6_an_undeclared_protected_change_is_unverified() {
        const REASON: &str = "claim missing for protected file tests/x.rs (modified): add a claim \
                              of kind protected_change whose text starts with the path and \
                              explains the change";
        for claims in [
            r#"[{"kind":"protected_change","text":"tests/x.rs2: a different file"}]"#,
            r#"[{"kind":"protected_change","text":"x.rs: the file name alone"}]"#,
            r#"[]"#,
            r#"[{"kind":"completion","text":"tests/x.rs: right text, wrong kind"}]"#,
        ] {
            let (conn, run) = setup();
            seed_diff(&conn, &run.run_id, "E003", &[("tests/x.rs", "modified", "test")]);
            let reason = invalid_reason(ingest_with_evidence(
                &conn,
                &run,
                "normal",
                &result_with_claims(claims),
            ));
            assert_eq!(reason, REASON, "claims were {claims}");
            let detail = last_invalid_detail(&conn);
            assert_eq!(detail["rule"], 6);
            assert_eq!(detail["path"], "tests/x.rs");
            assert_eq!(detail["status"], "modified");
            assert_eq!(detail["kind"], "test");
            assert_eq!(detail["reason"], REASON);
            // rejected before any write
            assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());
        }
    }

    /// AC4: every listed file needs its own claim, and the reason names the
    /// one that is missing with its own status.
    #[test]
    fn rule6_every_protected_change_needs_its_own_claim() {
        let (conn, run) = setup();
        seed_diff(
            &conn,
            &run.run_id,
            "E003",
            &[("Cargo.lock", "deleted", "check_input"), ("tests/x.rs", "modified", "test")],
        );
        let one = result_with_claims(r#"[{"kind":"protected_change","text":"tests/x.rs: reason"}]"#);
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &one));
        assert!(reason.contains("Cargo.lock (deleted)"), "reason was: {reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 6);
        assert_eq!(detail["kind"], "check_input");

        let both = result_with_claims(
            r#"[{"kind":"protected_change","text":"tests/x.rs: reason"},
                {"kind":"protected_change","text":"Cargo.lock no longer needed"}]"#,
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &both),
            IngestOutcome::Ok { claim_count: 2, .. }
        ));
    }

    /// AC9: an added file named like a check word is handled by rule 6 exactly
    /// like the other kinds, and the reason says `added`.
    #[test]
    fn rule6_treats_an_added_check_word_file_like_the_other_kinds() {
        let (conn, run) = setup();
        seed_diff(&conn, &run.run_id, "E003", &[("cargo.bat", "added", "check_word")]);
        let reason = invalid_reason(ingest_with_evidence(
            &conn,
            &run,
            "normal",
            &result_with_claims("[]"),
        ));
        assert!(reason.contains("cargo.bat"), "reason was: {reason}");
        assert!(reason.contains("added"), "reason was: {reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 6);
        assert_eq!(detail["status"], "added");
        assert_eq!(detail["kind"], "check_word");

        let declared = result_with_claims(
            r#"[{"kind":"protected_change","text":"cargo.bat: a wrapper the task asked for"}]"#,
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &declared),
            IngestOutcome::Ok { .. }
        ));
    }

    /// AC4: rule 6 reads the run's newest diff evidence - the largest code
    /// number, not the largest string - and no other run's.
    #[test]
    fn rule6_reads_only_this_runs_newest_diff_evidence() {
        let (conn, run) = setup();
        let nothing = result_with_claims("[]");

        // no diff evidence at all: rule 6 does not apply
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &nothing),
            IngestOutcome::Ok { .. }
        ));

        // another run's diff is not this run's
        let other = demo_run_row("01HZRUNIDOTHEROTHEROTHER1");
        state::insert_run(&conn, &other).unwrap();
        seed_diff(&conn, &other.run_id, "E002", &[("tests/x.rs", "modified", "test")]);
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &nothing),
            IngestOutcome::Ok { .. }
        ));

        // E9 lists a change, the newer E10 lists none: E10 decides
        seed_diff(&conn, &run.run_id, "E9", &[("tests/x.rs", "modified", "test")]);
        assert_eq!(last_rule(&conn, &run, &nothing), Some(6));
        seed_diff(&conn, &run.run_id, "E10", &[]);
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &nothing),
            IngestOutcome::Ok { .. }
        ));
    }

    /// Ingests and returns the rule of the rejection, or `None` when accepted.
    fn last_rule(conn: &Connection, run: &Run, json: &str) -> Option<i64> {
        match ingest_with_evidence(conn, run, "normal", json) {
            IngestOutcome::Invalid { .. } => last_invalid_detail(conn)["rule"].as_i64(),
            _ => None,
        }
    }

    /// AC4: rule 5 still applies to a `protected_change` claim, and is checked
    /// before rule 6.
    #[test]
    fn rule5_still_applies_to_a_protected_change_claim() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        insert_task_row(&conn, "high");
        let run = demo_run_row("01HZRUNIDHIGHRISKHIGHRISK6");
        state::insert_run(&conn, &run).unwrap();
        let _blobs = seed_evidence(&conn);
        seed_diff(&conn, &run.run_id, "E003", &[("tests/x.rs", "modified", "test")]);

        let uncited = result_with_claims(r#"[{"kind":"protected_change","text":"tests/x.rs: reason"}]"#);
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "high", &uncited));
        assert!(reason.contains("control_plane"), "reason was: {reason}");
        assert_eq!(last_invalid_detail(&conn)["rule"], 5);

        let cited = result_with_claims(
            r#"[{"kind":"protected_change","text":"tests/x.rs: reason","evidence":["E001"]}]"#,
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "high", &cited),
            IngestOutcome::Ok { claim_count: 1, .. }
        ));
    }

    const RESEARCH_SPEC: &str = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"task_type":"research"}"#;
    const RESEARCH_CLAIM: &str = r#"{"kind":"research","text":"X is Y","evidence":[],
        "sources":[{"url":"https://example.org/a","quote":"X is Y."}]}"#;

    /// A run of a research task (its spec copy says `task_type` research).
    fn research_run(conn: &Connection, run_id: &str) -> Run {
        let mut run = demo_run_row(run_id);
        run.spec = RESEARCH_SPEC.into();
        state::insert_run(conn, &run).unwrap();
        run
    }

    /// RES-1a AC6: a high task's `research` claim cites no control_plane
    /// evidence and is still accepted; the completion claim beside it is not
    /// exempt.
    #[test]
    fn rule5_does_not_apply_to_a_research_claim_on_a_high_task() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        insert_task_row(&conn, "high");
        let run = research_run(&conn, "01HZRUNIDHIGHRISKRESEARCH1");
        let _blobs = seed_evidence(&conn);

        let research_only = result_with_claims(&format!("[{RESEARCH_CLAIM}]"));
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "high", &research_only),
            IngestOutcome::Ok { claim_count: 1, .. }
        ));

        let with_completion = result_with_claims(&format!(
            r#"[{RESEARCH_CLAIM},{{"kind":"completion","text":"report written"}}]"#
        ));
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "high", &with_completion));
        assert!(reason.contains("claim 2"), "reason was: {reason}");
        assert_eq!(last_invalid_detail(&conn)["rule"], 5);
    }

    /// RES-1a AC7: a research task's completed delivery with no research
    /// claim is rule 7, with the reason in the event detail.
    #[test]
    fn rule7_a_completed_research_delivery_without_a_research_claim_is_unverified() {
        let (conn, _) = setup();
        let run = research_run(&conn, "01HZRUNIDRESEARCHNOCLAIM01");
        let json = result_with_claims(r#"[{"kind":"completion","text":"report written"}]"#);
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("research"), "reason was: {reason}");
        let detail = last_invalid_detail(&conn);
        assert_eq!(detail["rule"], 7);
        assert_eq!(detail["reason"], reason.as_str());
        assert!(state::list_claims_for_run(&conn, &run.run_id).unwrap().is_empty());

        let with_research = result_with_claims(&format!(
            r#"[{{"kind":"completion","text":"report written"}},{RESEARCH_CLAIM}]"#
        ));
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &with_research),
            IngestOutcome::Ok { claim_count: 2, .. }
        ));
    }

    /// RES-1a AC7: rule 7 skips a blocked delivery and every task that is not
    /// research, including the other document types.
    #[test]
    fn rule7_skips_blocked_deliveries_and_other_task_types() {
        let (conn, _) = setup();
        let run = research_run(&conn, "01HZRUNIDRESEARCHBLOCKED01");
        let blocked = format!(
            r#"{{"task_id":"{TASK}","status":"blocked","summary":"no access","claims":[]}}"#
        );
        assert!(matches!(
            ingest_with_evidence(&conn, &run, "normal", &blocked),
            IngestOutcome::Ok { .. }
        ));

        let nothing = result_with_claims("[]");
        for task_type in ["docs", "review", "code"] {
            let (conn, mut run) = setup();
            run.spec = format!(r#"{{"goal":"g","acceptance_criteria":["x"],"task_type":"{task_type}"}}"#);
            assert_eq!(last_rule(&conn, &run, &nothing), None, "{task_type}");
        }
    }

    /// Phase 1 test, edited (Open question 1): its claim was `test_result` with
    /// no citation, which rule 2 now rejects *before* the claim insert this
    /// test exists to fail. `completion` is exempt from rule 2, so the FK
    /// failure is reached again.
    #[test]
    fn n1_a_claim_insert_failure_still_persists_its_result_invalid_event() {
        // No `runs` row for this run_id, so `claims.run_id`'s foreign key
        // rejects `insert_claim` and the ingest transaction rolls back - the
        // result_invalid event must survive that rollback because it is
        // appended after `tx` is dropped (N1).
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        insert_task_row(&conn, "normal");
        let run = demo_run_row("01HZRUNIDNOROWNOROWNOROW01");
        // Deliberately not inserted into `runs`.

        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                 "claims":[{{"kind":"completion","text":"all AC met"}}]}}"#
        );
        let reason = invalid_reason(ingest_with_evidence(&conn, &run, "normal", &json));
        assert!(reason.contains("writing claim failed"), "reason was: {reason}");

        let evs = state::events_since(&conn, 0, Some(TASK)).unwrap();
        assert_eq!(
            evs.iter().filter(|e| e.event_type == "result_invalid").count(),
            1,
            "the result_invalid event must persist despite the transaction's rollback"
        );
    }

    /// Phase 1 test, edited for the new `task_risk` parameter only.
    #[test]
    fn a_missing_file_is_invalid_not_a_panic() {
        let (conn, run) = setup();
        let dir = tempfile::tempdir().unwrap();
        let reason = invalid_reason(ingest_path(
            &conn,
            &run,
            "normal",
            &dir.path().join("nope.json"),
        ));
        assert!(reason.to_lowercase().contains("result.json"), "{reason}");
    }

    /// Phase 1 test, edited for the new `task_risk` parameter only.
    #[test]
    fn ingesting_a_real_file_works() {
        let (conn, run) = setup();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("result.json");
        std::fs::write(
            &p,
            format!(r#"{{"task_id":"{TASK}","status":"completed","summary":"s","claims":[]}}"#),
        )
        .unwrap();
        assert!(matches!(
            ingest_path(&conn, &run, "normal", &p),
            IngestOutcome::Ok { .. }
        ));
    }

    /// Commit `6153a9b`'s test, kept name and kept assertions, extended
    /// (Open questions 1 and 2): `E001` is seeded so the citations resolve,
    /// the changed-text claim in the third ingest now carries a citation
    /// (rule 2 would otherwise reject it before the id churn this test is
    /// about), and the `claim_evidence` side of the merge is asserted
    /// alongside the ids.
    #[test]
    fn m4_ingesting_the_same_result_twice_does_not_double_the_claims() {
        let (conn, run) = setup();
        let _blobs = seed_evidence(&conn);
        let json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"did it",
                 "claims":[{{"kind":"test_result","text":"green","evidence":["E001"]}},
                           {{"kind":"completion","text":"all AC met"}}]}}"#
        );
        let links_of = |conn: &Connection, claim_id: &str| -> Vec<String> {
            let mut out = state::list_evidence_for_claim(conn, claim_id).unwrap();
            out.sort();
            out
        };
        let total_links = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM claim_evidence WHERE claim_id IN
                 (SELECT claim_id FROM claims WHERE run_id = ?1)",
                [&run.run_id],
                |r| r.get(0),
            )
            .unwrap()
        };

        match ingest_with_evidence(&conn, &run, "normal", &json) {
            IngestOutcome::Ok { claim_count, .. } => assert_eq!(claim_count, 2),
            other => panic!("expected Ok, got {other:?}"),
        }
        let first_claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(first_claims.len(), 2);
        assert_eq!(total_links(&conn), 1, "only the test_result claim cites anything");

        // re-ingest (e.g. a retried `submit`) must replace, not double, the claims
        match ingest_with_evidence(&conn, &run, "normal", &json) {
            IngestOutcome::Ok { claim_count, .. } => assert_eq!(claim_count, 2),
            other => panic!("expected Ok, got {other:?}"),
        }
        let second_claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(second_claims.len(), 2);
        assert_eq!(total_links(&conn), 1, "nor double the links");

        // #32 (final review deferred item 32): re-ingesting the exact same
        // claims must keep their claim_ids stable - Phase 2's claim_evidence
        // keys on claim_id, and it must not change just because a claim
        // was re-sent unchanged.
        let first_ids: std::collections::HashSet<&str> =
            first_claims.iter().map(|c| c.claim_id.as_str()).collect();
        let second_ids: std::collections::HashSet<&str> =
            second_claims.iter().map(|c| c.claim_id.as_str()).collect();
        assert_eq!(first_ids, second_ids, "unchanged claims must keep their claim_id");

        // Open question 2, the Phase 2 consequence of that stability: the link
        // set is rebuilt, but under the *same* claim_id.
        let stable_test_result = second_claims
            .iter()
            .find(|c| c.kind == "test_result")
            .expect("the test_result claim");
        assert_eq!(links_of(&conn, &stable_test_result.claim_id), vec!["EV1".to_string()]);
        // and nothing is orphaned
        assert_eq!(orphans_now(&conn), 0);

        // both ingests are still recorded in the event log
        let evs = state::events_since(&conn, 0, Some(TASK)).unwrap();
        assert_eq!(
            evs.iter().filter(|e| e.event_type == "result_ingested").count(),
            2
        );

        // a third ingest changes one claim's text: its old id must disappear
        // and a new one take its place, while the untouched claim keeps its id.
        // The changed claim keeps a citation, because it is still a
        // `test_result` and rule 2 would otherwise reject the document before
        // any of this happened.
        let old_test_result_id = stable_test_result.claim_id.clone();
        let unchanged_completion_id = second_claims
            .iter()
            .find(|c| c.kind == "completion")
            .unwrap()
            .claim_id
            .clone();
        let changed_json = format!(
            r#"{{"task_id":"{TASK}","status":"completed","summary":"did it, again",
                 "claims":[{{"kind":"test_result","text":"green, now with more coverage","evidence":["E001"]}},
                           {{"kind":"completion","text":"all AC met"}}]}}"#
        );
        match ingest_with_evidence(&conn, &run, "normal", &changed_json) {
            IngestOutcome::Ok { claim_count, .. } => assert_eq!(claim_count, 2),
            other => panic!("expected Ok, got {other:?}"),
        }
        let third_claims = state::list_claims_for_run(&conn, &run.run_id).unwrap();
        assert_eq!(third_claims.len(), 2);
        let third_ids: Vec<&str> = third_claims.iter().map(|c| c.claim_id.as_str()).collect();
        assert!(
            !third_ids.contains(&old_test_result_id.as_str()),
            "the changed claim's old id must be gone: {third_ids:?}"
        );
        assert!(
            third_ids.contains(&unchanged_completion_id.as_str()),
            "the unchanged claim must keep its id: {third_ids:?}"
        );
        // the vanished claim took its links with it, and the new one has its own
        assert!(links_of(&conn, &old_test_result_id).is_empty());
        assert_eq!(total_links(&conn), 1);
        assert_eq!(orphans_now(&conn), 0);
    }

    /// Link rows whose claim no longer exists. A helper rather than an inline
    /// query because three tests need it.
    fn orphans_now(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM claim_evidence
             WHERE claim_id NOT IN (SELECT claim_id FROM claims)",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }
}
