//! Tests for the integration proof recorder (P5.6).

use std::sync::Arc;

use brehon_ports::{EventStore, ProofStore};
use brehon_store_fjall::FjallEventStore;
use brehon_types::TaskId;
use tempfile::TempDir;

use super::integration_proof::{
    IntegrationAbortProof, IntegrationProofRecorder, IntegrationSuccessProof,
};

fn build_recorder(dir: &TempDir) -> (IntegrationProofRecorder, Arc<dyn ProofStore + Send + Sync>) {
    let store = Arc::new(FjallEventStore::new(dir.path().join("fjall")).unwrap());
    let event_store: Arc<dyn EventStore + Send + Sync> = store.clone();
    let proof_store: Arc<dyn ProofStore + Send + Sync> = store.clone();
    (
        IntegrationProofRecorder::empty()
            .with_event_store(event_store)
            .with_proof_store(proof_store.clone()),
        proof_store,
    )
}

#[tokio::test]
async fn successful_integration_records_merge_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, proof_store) = build_recorder(&dir);

    let outcome = recorder
        .record_success(IntegrationSuccessProof {
            task_id: "T-integ-ok",
            status: "integrated",
            source_branch: Some("worker/T-integ-ok"),
            target_branch: Some("epic/x"),
            worktree_path: Some("/tmp/wt-int"),
            commit: Some("deadbeef1234"),
            summary: Some("Integrated reviewed commit into epic/x.".to_string()),
        })
        .await;

    assert_eq!(outcome.status, "recorded", "{outcome:?}");
    assert!(outcome.events_recorded >= 1);
    assert!(outcome.summary.is_some());

    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-integ-ok"))
        .await
        .unwrap()
        .expect("proof bundle exists");
    let integration = bundle
        .integration_result
        .as_ref()
        .expect("integration evidence recorded");
    assert_eq!(integration.status, "integrated");
    assert_eq!(integration.branch.as_deref(), Some("worker/T-integ-ok"));
    assert_eq!(integration.base_branch.as_deref(), Some("epic/x"));
    assert_eq!(integration.commit.as_deref(), Some("deadbeef1234"));
    assert!(integration.conflicts.is_empty());
}

#[tokio::test]
async fn integration_success_without_commit_marks_proof_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, _store) = build_recorder(&dir);

    let outcome = recorder
        .record_success(IntegrationSuccessProof {
            task_id: "T-integ-no-commit",
            status: "integrated",
            source_branch: Some("worker/x"),
            target_branch: Some("epic/x"),
            worktree_path: None,
            commit: None,
            summary: None,
        })
        .await;

    assert_eq!(outcome.status, "incomplete");
    assert!(outcome
        .warnings
        .iter()
        .any(|warn| warn.contains("did not record a merged commit reference")));
}

#[tokio::test]
async fn aborted_integration_records_reason_and_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, proof_store) = build_recorder(&dir);
    let conflicts = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];

    let outcome = recorder
        .record_abort(IntegrationAbortProof {
            task_id: "T-integ-abort",
            source_branch: Some("worker/T-integ-abort"),
            target_branch: Some("epic/y"),
            worktree_path: Some("/tmp/wt-abort"),
            reason: "supervisor abandoned cherry-pick",
            conflicts: conflicts.clone(),
        })
        .await;

    assert_eq!(outcome.status, "recorded", "{outcome:?}");
    assert!(outcome.summary.is_some());

    let bundle = proof_store
        .proof_bundle_for_task(&TaskId::new("T-integ-abort"))
        .await
        .unwrap()
        .expect("proof bundle exists");
    let integration = bundle
        .integration_result
        .as_ref()
        .expect("aborted integration recorded");
    assert_eq!(integration.status, "aborted");
    assert!(integration
        .summary
        .as_deref()
        .unwrap_or("")
        .contains("supervisor abandoned cherry-pick"));
    assert_eq!(integration.conflicts, conflicts);
    // Conflicts also bubble up to the bundle-level list.
    for conflict in &conflicts {
        assert!(bundle.conflicts.contains(conflict));
    }
}

#[tokio::test]
async fn aborted_integration_without_reason_is_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let (recorder, _store) = build_recorder(&dir);

    let outcome = recorder
        .record_abort(IntegrationAbortProof {
            task_id: "T-integ-noreason",
            source_branch: None,
            target_branch: Some("epic/z"),
            worktree_path: None,
            reason: "   ",
            conflicts: Vec::new(),
        })
        .await;

    assert_eq!(outcome.status, "incomplete");
    assert!(outcome
        .warnings
        .iter()
        .any(|warn| warn.contains("without a recorded reason")));
}

#[tokio::test]
async fn missing_stores_surface_proof_unavailable() {
    let recorder = IntegrationProofRecorder::empty();
    let outcome = recorder
        .record_success(IntegrationSuccessProof {
            task_id: "T-x",
            status: "integrated",
            source_branch: Some("a"),
            target_branch: Some("b"),
            worktree_path: None,
            commit: Some("c"),
            summary: None,
        })
        .await;
    assert_eq!(outcome.status, "unavailable");
    assert!(outcome
        .warnings
        .iter()
        .any(|warn| warn.contains("integration proof evidence was not recorded")));
}
