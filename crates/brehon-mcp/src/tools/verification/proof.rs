//! Review evidence recording into proof bundles.
//!
//! Mirrors the worker proof recorder in `task_actions::proof`: optional
//! event/proof stores are attached via the verification tool, and review
//! consolidation calls `record_consolidation` after `write_consolidated`
//! to durably link review evidence (review id, reviewer ids, scores,
//! verdict, findings, follow-ups) to the proof bundle for the task.

use std::sync::Arc;

use chrono::Utc;

use brehon_ports::{EventStore, ProofStore};
use brehon_types::{
    Event, EventKind, ProofBundle, ProofBundleId, ProofReview, ReviewId, ReviewScore,
    ReviewVerdict, TaskId,
};

use super::state::{ConsolidatedReport, StoredFinding, StoredSubmission};
use crate::tools::proof_summary::{write_proof_cache, ProofSummary};

/// Optional review proof recorder attached to the verification tool.
#[derive(Clone, Default)]
pub(super) struct ReviewProofRecorder {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
    proof_store: Option<Arc<dyn ProofStore + Send + Sync>>,
}

/// Outcome of recording review evidence into a proof bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReviewProofOutcome {
    /// `recorded`, `unavailable`, `error`, or `incomplete`.
    pub status: &'static str,
    /// Bundle id, if the bundle exists or was created.
    pub proof_bundle_id: Option<String>,
    /// Number of events appended for this consolidation.
    pub events_recorded: usize,
    /// Visible warnings (missing evidence, store unavailable, etc.).
    pub warnings: Vec<String>,
    /// Compact summary suitable for embedding back into MCP responses.
    pub summary: Option<ProofSummary>,
}

impl ReviewProofOutcome {
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
                "Review proof evidence recording failed: {}",
                message.into()
            )],
            summary: None,
        }
    }

    /// Attach this outcome to the MCP tool response payload under
    /// `proof`/`proof_status`/`proof_warning` so reviewers see what was
    /// (or wasn't) recorded.
    pub fn attach_to_result(&self, result: &mut serde_json::Value) {
        result["proof_status"] = serde_json::Value::String(self.status.to_string());
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
            result["proof_warning"] = serde_json::Value::String(warning.clone());
        }
    }
}

impl ReviewProofRecorder {
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

    /// Record review consolidation evidence into the proof bundle for the
    /// given task. Emits one `ProofReviewLinked` event per submission so a
    /// reviewer that submitted a verdict is durably attributed.
    pub(super) async fn record_consolidation(
        &self,
        task_id: &str,
        review_id: &str,
        report: &ConsolidatedReport,
        submissions: &[StoredSubmission],
    ) -> ReviewProofOutcome {
        let Some(event_store) = self.event_store.as_ref() else {
            return ReviewProofOutcome::unavailable(
                "No event store is attached; review proof evidence was not recorded.",
            );
        };
        let Some(proof_store) = self.proof_store.as_ref() else {
            return ReviewProofOutcome::unavailable(
                "No proof store is attached; review proof evidence was not recorded.",
            );
        };

        let proof_bundle_id = proof_bundle_id_for_task(task_id);
        let task_id_typed = TaskId::new(task_id);
        let mut events_recorded = 0usize;
        let mut warnings = Vec::new();

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
                    return ReviewProofOutcome::error(proof_bundle_id, err);
                }
                events_recorded += 1;
            }
            Err(err) => {
                return ReviewProofOutcome::error(proof_bundle_id, err.to_string());
            }
        }

        // Pre-seed reviewer→submission map to emit one event per submitter.
        let submitter_set: Vec<&StoredSubmission> = submissions
            .iter()
            .filter(|submission| submission.review_id == review_id)
            .collect();

        if submitter_set.is_empty() {
            // No reviewer submissions to attribute; still emit a panel-wide
            // review record so the bundle reflects the outcome.
            let now = Utc::now();
            let review = ProofReview {
                review_id: ReviewId::new(review_id),
                reviewer_id: None,
                score: None,
                verdict: parse_outcome_verdict(report.outcome.as_str()),
                findings: collect_finding_summaries(report),
                followups: collect_followup_summaries(report),
                reviewed_at: now,
            };
            let event = Event {
                kind: EventKind::ProofReviewLinked {
                    proof_bundle_id: proof_bundle_id.clone(),
                    task_id: task_id_typed.clone(),
                    review,
                    linked_at: now,
                },
                timestamp: now,
                aggregate_id: task_id_typed.as_str().to_string(),
            };
            if let Err(err) = append_and_project(event_store, proof_store, event).await {
                return ReviewProofOutcome::error(proof_bundle_id, err);
            }
            events_recorded += 1;
            warnings.push(
                "Review consolidation linked without reviewer-level attribution.".to_string(),
            );
        } else {
            for submission in &submitter_set {
                let now = Utc::now();
                let review = ProofReview {
                    review_id: ReviewId::new(review_id),
                    reviewer_id: Some(submission.reviewer.clone()),
                    score: ReviewScore::try_from(submission.score).ok(),
                    verdict: parse_verdict_str(submission.verdict.as_str())
                        .or_else(|| parse_outcome_verdict(report.outcome.as_str())),
                    findings: submission
                        .findings
                        .iter()
                        .map(stored_finding_summary)
                        .collect(),
                    followups: collect_followup_summaries_for_reviewer(
                        report,
                        &submission.reviewer,
                    ),
                    reviewed_at: now,
                };
                let event = Event {
                    kind: EventKind::ProofReviewLinked {
                        proof_bundle_id: proof_bundle_id.clone(),
                        task_id: task_id_typed.clone(),
                        review,
                        linked_at: now,
                    },
                    timestamp: now,
                    aggregate_id: task_id_typed.as_str().to_string(),
                };
                if let Err(err) = append_and_project(event_store, proof_store, event).await {
                    return ReviewProofOutcome::error(proof_bundle_id, err);
                }
                events_recorded += 1;
            }
        }

        // Mark consolidation incomplete if there are blocking findings but
        // outcome did not capture follow-ups for them.
        if !report.blocking.is_empty()
            && report.outcome != "approved"
            && collect_followup_summaries(report).is_empty()
        {
            warnings
                .push("Blocking findings recorded without any reviewer follow-ups.".to_string());
        }

        // Pull the projected bundle back so callers can surface its summary.
        let bundle = proof_store
            .proof_bundle_for_task(&task_id_typed)
            .await
            .ok()
            .flatten();
        let summary = bundle.as_ref().map(ProofSummary::from_bundle);
        if let Some(summary) = &summary {
            write_proof_cache(task_id, summary);
        }
        let status = if !warnings.is_empty() {
            "incomplete"
        } else {
            "recorded"
        };
        ReviewProofOutcome {
            status,
            proof_bundle_id: Some(proof_bundle_id.to_string()),
            events_recorded,
            warnings,
            summary,
        }
    }

    /// Fetch the current proof bundle for the task, if a proof store is
    /// attached. Returns `None` when no store is wired or no bundle exists.
    pub(super) async fn proof_bundle_for_task(&self, task_id: &str) -> Option<ProofBundle> {
        let store = self.proof_store.as_ref()?;
        store
            .proof_bundle_for_task(&TaskId::new(task_id))
            .await
            .ok()
            .flatten()
    }

    /// Returns true when a proof projection store has been attached. Used by
    /// callers that want to render an explicit "no proof recorded" surface
    /// when wiring is present but a bundle has not been produced yet.
    pub(super) fn is_attached(&self) -> bool {
        self.proof_store.is_some()
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

fn parse_verdict_str(verdict: &str) -> Option<ReviewVerdict> {
    match verdict.to_ascii_lowercase().as_str() {
        "approved" | "approve" => Some(ReviewVerdict::Approve),
        "needs_revision" | "changes_requested" | "request_changes" => {
            Some(ReviewVerdict::ChangesRequested)
        }
        "rejected" | "reject" => Some(ReviewVerdict::Reject),
        _ => None,
    }
}

fn parse_outcome_verdict(outcome: &str) -> Option<ReviewVerdict> {
    match outcome {
        "approved" => Some(ReviewVerdict::Approve),
        "changes_requested" => Some(ReviewVerdict::ChangesRequested),
        "rejected" => Some(ReviewVerdict::Reject),
        _ => None,
    }
}

fn stored_finding_summary(finding: &StoredFinding) -> String {
    let location = match (&finding.file, finding.line) {
        (Some(file), Some(line)) => format!(" ({file}:{line})"),
        (Some(file), None) => format!(" ({file})"),
        _ => String::new(),
    };
    let severity = if finding.severity.is_empty() {
        "finding"
    } else {
        finding.severity.as_str()
    };
    format!("[{severity}]{location} {}", finding.description.trim())
}

fn collect_finding_summaries(report: &ConsolidatedReport) -> Vec<String> {
    report
        .blocking
        .iter()
        .chain(report.suggestions.iter())
        .chain(report.nitpicks.iter())
        .map(stored_finding_summary)
        .collect()
}

fn collect_followup_summaries(report: &ConsolidatedReport) -> Vec<String> {
    report
        .blocking
        .iter()
        .filter_map(|finding| {
            finding
                .suggestion
                .as_ref()
                .map(|suggestion| suggestion.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .collect()
}

fn collect_followup_summaries_for_reviewer(
    report: &ConsolidatedReport,
    _reviewer: &str,
) -> Vec<String> {
    // The consolidated report does not retain per-reviewer attribution for
    // suggestion text. Surface report-wide blocking follow-ups so each
    // reviewer event still records a follow-up trail.
    collect_followup_summaries(report)
}
