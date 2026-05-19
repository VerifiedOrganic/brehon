//! Tests for the review proof recorder (P5.5).

use std::sync::Arc;

use brehon_ports::{EventStore, ProofStore};
use brehon_store_fjall::FjallEventStore;
use brehon_types::{ReviewVerdict, TaskId};
use tempfile::TempDir;

use super::proof::ReviewProofRecorder;
use super::state::{ConsolidatedReport, StoredFinding, StoredSubmission};

fn build_recorder(dir: &TempDir) -> (ReviewProofRecorder, Arc<dyn ProofStore + Send + Sync>) {
    let store = Arc::new(FjallEventStore::new(dir.path().join("fjall")).unwrap());
    let event_store: Arc<dyn EventStore + Send + Sync> = store.clone();
    let proof_store: Arc<dyn ProofStore + Send + Sync> = store.clone();
    (
        ReviewProofRecorder::empty()
            .with_event_store(event_store)
            .with_proof_store(proof_store.clone()),
        proof_store,
    )
}

fn finding(severity: &str, description: &str, suggestion: Option<&str>) -> StoredFinding {
    StoredFinding {
        description: description.to_string(),
        file: None,
        line: None,
        severity: severity.to_string(),
        suggestion: suggestion.map(str::to_string),
    }
}

fn submission(reviewer: &str, review_id: &str, score: u8, verdict: &str) -> StoredSubmission {
    StoredSubmission {
        review_id: review_id.to_string(),
        reviewer: reviewer.to_string(),
        round: 1,
        score,
        verdict: verdict.to_string(),
        summary: format!("{reviewer} review"),
        findings: vec![finding(
            "blocking",
            "needs better tests",
            Some("add unit tests"),
        )],
        submitted_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn report(outcome: &str, review_id: &str) -> ConsolidatedReport {
    ConsolidatedReport {
        review_id: review_id.to_string(),
        task_id: "T-proof-rev".to_string(),
        round: 1,
        outcome: outcome.to_string(),
        scores: serde_json::json!({}),
        average_score: 7.0,
        min_score: 6,
        approval_count: 1,
        threshold_result: "needs_revision".to_string(),
        threshold_reason: "blocking findings outstanding".to_string(),
        blocking: vec![finding(
            "blocking",
            "needs better tests",
            Some("add unit tests"),
        )],
        suggestions: vec![finding("suggestion", "consider renaming foo", None)],
        nitpicks: Vec::new(),
        dissent: Vec::new(),
        evaluated_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[tokio::test]
async fn records_proof_review_per_reviewer_submission() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, proof_store) = build_recorder(&dir);
    let review_id = "REV-proof-rev";
    let submissions = vec![
        submission("reviewer-a", review_id, 7, "needs_revision"),
        submission("reviewer-b", review_id, 8, "approved"),
    ];
    let report = report("changes_requested", review_id);

    let outcome = recorder
        .record_consolidation("T-proof-rev", review_id, &report, &submissions)
        .await;
    assert_eq!(outcome.status, "recorded");
    assert!(outcome.events_recorded >= submissions.len(), "{outcome:?}");
    assert!(outcome.summary.is_some());

    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-proof-rev"))
        .await
        .unwrap()
        .expect("proof bundle should be projected");
    assert!(bundle.review_ids.iter().any(|id| id.as_str() == review_id));
    assert_eq!(bundle.review_scores.len(), 2);

    let reviewer_ids: Vec<String> = bundle
        .review_scores
        .iter()
        .filter_map(|review| review.reviewer_id.clone())
        .collect();
    assert!(reviewer_ids.contains(&"reviewer-a".to_string()));
    assert!(reviewer_ids.contains(&"reviewer-b".to_string()));

    let verdicts: Vec<ReviewVerdict> = bundle
        .review_scores
        .iter()
        .filter_map(|review| review.verdict)
        .collect();
    assert!(verdicts.contains(&ReviewVerdict::ChangesRequested));
    assert!(verdicts.contains(&ReviewVerdict::Approve));

    assert!(bundle
        .review_scores
        .iter()
        .flat_map(|review| review.findings.iter())
        .any(|finding| finding.contains("needs better tests")));
    assert!(bundle
        .review_scores
        .iter()
        .flat_map(|review| review.followups.iter())
        .any(|followup| followup.contains("add unit tests")));
}

#[tokio::test]
async fn missing_followups_surface_as_incomplete_for_blocking_findings() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, _store) = build_recorder(&dir);
    let review_id = "REV-no-followup";
    let submissions = vec![submission("reviewer-a", review_id, 5, "needs_revision")];
    let mut report = report("changes_requested", review_id);
    // Strip suggestion text so the recorder cannot derive a follow-up.
    for finding in report.blocking.iter_mut() {
        finding.suggestion = None;
    }

    let outcome = recorder
        .record_consolidation("T-proof-no-followup", review_id, &report, &submissions)
        .await;
    assert_eq!(outcome.status, "incomplete");
    assert!(outcome
        .warnings
        .iter()
        .any(|warn| warn.contains("Blocking findings recorded without any reviewer follow-ups")));
}

#[tokio::test]
async fn missing_stores_surface_proof_unavailable() {
    let recorder = ReviewProofRecorder::empty();
    let submissions = vec![submission("reviewer-a", "REV-x", 9, "approved")];
    let outcome = recorder
        .record_consolidation("T-x", "REV-x", &report("approved", "REV-x"), &submissions)
        .await;
    assert_eq!(outcome.status, "unavailable");
    assert!(outcome
        .warnings
        .iter()
        .any(|warn| warn.contains("review proof evidence was not recorded")));
}
