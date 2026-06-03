//! Integration evidence recording into proof bundles.
//!
//! Mirrors the worker and review proof recorders. Threaded into both the
//! integrate and abort-integration action handlers so successful merges and
//! aborted attempts both leave a durable proof record (source branch,
//! target branch, commit, conflicts, abort reason).
//!
//! Recording is best-effort: missing stores surface as visible warnings on
//! the response and never block the integration transition.

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;

use brehon_ports::{EventStore, ProofStore};
use brehon_types::{Event, EventKind, ProofBundleId, ProofIntegration, TaskId};

use crate::tools::proof_summary::{write_proof_cache, ProofSummary};

/// Optional integration proof recorder. Default state has neither store
/// attached, in which case all recording paths short-circuit with an
/// `unavailable` outcome.
#[derive(Clone, Default)]
pub(super) struct IntegrationProofRecorder {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
    proof_store: Option<Arc<dyn ProofStore + Send + Sync>>,
}

/// Compact descriptor for a successful integration result.
#[derive(Debug, Clone)]
pub(super) struct IntegrationSuccessProof<'a> {
    pub task_id: &'a str,
    pub status: &'a str,
    pub source_branch: Option<&'a str>,
    pub target_branch: Option<&'a str>,
    pub worktree_path: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub summary: Option<String>,
}

/// Compact descriptor for an aborted integration.
#[derive(Debug, Clone)]
pub(super) struct IntegrationAbortProof<'a> {
    pub task_id: &'a str,
    pub source_branch: Option<&'a str>,
    pub target_branch: Option<&'a str>,
    pub worktree_path: Option<&'a str>,
    pub reason: &'a str,
    pub conflicts: Vec<String>,
}

/// Outcome of recording integration evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IntegrationProofOutcome {
    pub status: &'static str,
    pub proof_bundle_id: Option<String>,
    pub events_recorded: usize,
    pub warnings: Vec<String>,
    pub summary: Option<ProofSummary>,
}

impl IntegrationProofOutcome {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: "unavailable",
            proof_bundle_id: None,
            events_recorded: 0,
            warnings: vec![message.into()],
            summary: None,
        }
    }

    fn error(proof_bundle_id: ProofBundleId, message: impl Into<String>) -> Self {
        Self {
            status: "error",
            proof_bundle_id: Some(proof_bundle_id.to_string()),
            events_recorded: 0,
            warnings: vec![format!(
                "Integration proof evidence recording failed: {}",
                message.into()
            )],
            summary: None,
        }
    }

    /// Attach this outcome to an MCP tool response payload.
    pub(super) fn attach_to_result(&self, result: &mut Value) {
        result["proof_status"] = Value::String(self.status.to_string());
        let mut payload = serde_json::json!({
            "status": self.status,
            "proof_bundle_id": self.proof_bundle_id,
            "events_recorded": self.events_recorded,
            "warnings": self.warnings,
        });
        if let Some(summary) = &self.summary {
            if let Ok(value) = serde_json::to_value(summary) {
                payload["summary"] = value;
            }
        }
        result["proof"] = payload;
        if let Some(warning) = self.warnings.first() {
            result["proof_warning"] = Value::String(warning.clone());
        }
    }
}

impl IntegrationProofRecorder {
    pub(super) const fn empty() -> Self {
        Self {
            event_store: None,
            proof_store: None,
        }
    }

    pub(super) fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }

    pub(super) fn with_proof_store(mut self, store: Arc<dyn ProofStore + Send + Sync>) -> Self {
        self.proof_store = Some(store);
        self
    }

    /// Record a successful integration into the proof bundle for the task.
    pub(super) async fn record_success(
        &self,
        proof: IntegrationSuccessProof<'_>,
    ) -> IntegrationProofOutcome {
        let integration = ProofIntegration {
            status: proof.status.to_string(),
            branch: proof.source_branch.map(str::to_string),
            base_branch: proof.target_branch.map(str::to_string),
            worktree_path: proof.worktree_path.map(str::to_string),
            commit: proof.commit.map(str::to_string),
            summary: proof.summary,
            conflicts: Vec::new(),
            integrated_at: Utc::now(),
        };
        let mut warnings = Vec::new();
        if proof.commit.is_none_or(str::is_empty) {
            warnings.push(
                "Successful integration proof did not record a merged commit reference."
                    .to_string(),
            );
        }
        if proof.source_branch.is_none_or(str::is_empty) {
            warnings.push("Integration proof did not record a source branch.".to_string());
        }
        if proof.target_branch.is_none_or(str::is_empty) {
            warnings.push("Integration proof did not record a target branch.".to_string());
        }
        self.record_integration(proof.task_id, integration, warnings)
            .await
    }

    /// Record an aborted integration into the proof bundle for the task.
    pub(super) async fn record_abort(
        &self,
        proof: IntegrationAbortProof<'_>,
    ) -> IntegrationProofOutcome {
        let integration = ProofIntegration {
            status: "aborted".to_string(),
            branch: proof.source_branch.map(str::to_string),
            base_branch: proof.target_branch.map(str::to_string),
            worktree_path: proof.worktree_path.map(str::to_string),
            commit: None,
            summary: Some(format!("Integration aborted: {}", proof.reason)),
            conflicts: proof.conflicts.clone(),
            integrated_at: Utc::now(),
        };
        let mut warnings = Vec::new();
        if proof.reason.trim().is_empty() {
            warnings.push("Integration was aborted without a recorded reason.".to_string());
        }
        self.record_integration(proof.task_id, integration, warnings)
            .await
    }

    async fn record_integration(
        &self,
        task_id: &str,
        integration: ProofIntegration,
        mut warnings: Vec<String>,
    ) -> IntegrationProofOutcome {
        let Some(event_store) = self.event_store.as_ref() else {
            return IntegrationProofOutcome::unavailable(
                "No event store is attached; integration proof evidence was not recorded.",
            );
        };
        let Some(proof_store) = self.proof_store.as_ref() else {
            return IntegrationProofOutcome::unavailable(
                "No proof store is attached; integration proof evidence was not recorded.",
            );
        };

        let proof_bundle_id = proof_bundle_id_for_task(task_id);
        let task_id_typed = TaskId::new(task_id);
        let mut events_recorded = 0usize;

        match proof_store.proof_bundle_for_task(&task_id_typed).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                let now = Utc::now();
                let event = Event {
                    kind: EventKind::ProofBundleCreated {
                        proof_bundle_id: proof_bundle_id.clone(),
                        task_id: task_id_typed.clone(),
                        run_ids: Vec::new(),
                        created_at: now,
                    },
                    timestamp: now,
                    aggregate_id: task_id_typed.as_str().to_string(),
                };
                if let Err(err) = append_and_project(event_store, proof_store, event).await {
                    return IntegrationProofOutcome::error(proof_bundle_id, err);
                }
                events_recorded += 1;
            }
            Err(err) => return IntegrationProofOutcome::error(proof_bundle_id, err.to_string()),
        }

        let now = Utc::now();
        let event = Event {
            kind: EventKind::ProofIntegrationRecorded {
                proof_bundle_id: proof_bundle_id.clone(),
                task_id: task_id_typed.clone(),
                integration,
                recorded_at: now,
            },
            timestamp: now,
            aggregate_id: task_id_typed.as_str().to_string(),
        };
        if let Err(err) = append_and_project(event_store, proof_store, event).await {
            return IntegrationProofOutcome::error(proof_bundle_id, err);
        }
        events_recorded += 1;

        let bundle = proof_store
            .proof_bundle_for_task(&task_id_typed)
            .await
            .ok()
            .flatten();
        let summary = bundle.as_ref().map(ProofSummary::from_bundle);
        if let Some(summary) = &summary {
            write_proof_cache(task_id, summary);
        }
        let status = if warnings.is_empty() {
            "recorded"
        } else {
            "incomplete"
        };
        if status == "incomplete" {
            warnings.sort();
            warnings.dedup();
        }
        IntegrationProofOutcome {
            status,
            proof_bundle_id: Some(proof_bundle_id.to_string()),
            events_recorded,
            warnings,
            summary,
        }
    }
}

fn proof_bundle_id_for_task(task_id: &str) -> ProofBundleId {
    ProofBundleId::new(format!("proof-{task_id}"))
}

async fn append_and_project(
    event_store: &Arc<dyn EventStore + Send + Sync>,
    proof_store: &Arc<dyn ProofStore + Send + Sync>,
    event: Event,
) -> Result<(), String> {
    let event_id = event_store
        .append(event.clone())
        .await
        .map_err(|err| err.to_string())?;
    proof_store
        .apply_proof_event(&event, event_id)
        .await
        .map_err(|err| err.to_string())?;
    Ok(())
}
