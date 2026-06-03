//! Review session lifecycle.
//!
//! Manages the full flow:
//! - Spawn panel
//! - Collect scores
//! - Consolidate feedback
//! - Notify worker
//! - Clear sessions

use std::sync::Arc;

use thiserror::Error;

use brehon_ports::EventStore;
use brehon_types::{Event, EventKind, ReviewPolicy};

use crate::consolidation::FeedbackConsolidator;
use crate::panel::ReviewPanel;
use crate::scoring::{ScoreCollector, ThresholdEvaluator, ThresholdResult};

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("Max review rounds exceeded")]
    MaxRoundsExceeded,

    #[error("Panel error: {0}")]
    PanelError(String),

    #[error("Scoring error: {0}")]
    ScoringError(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("No reviewers available")]
    NoReviewers,

    #[error("Session spawn failed: {0}")]
    SpawnFailed(String),

    #[error("Review timeout")]
    Timeout,
}

/// Outcome of a review round.
#[derive(Debug, Clone)]
pub enum ReviewOutcome {
    /// Review approved.
    Approved,
    /// Changes requested.
    ChangesRequested {
        feedback: brehon_types::ConsolidatedFeedback,
    },
    /// Rejected (fundamental issues).
    Rejected {
        feedback: brehon_types::ConsolidatedFeedback,
    },
    /// Max rounds exceeded, needs escalation.
    Escalate {
        reason: String,
        feedback: brehon_types::ConsolidatedFeedback,
    },
}

/// Manages the full review lifecycle.
pub struct ReviewLifecycle {
    policy: ReviewPolicy,
    store: Arc<dyn EventStore>,
}

impl ReviewLifecycle {
    pub fn new(policy: ReviewPolicy, store: Arc<dyn EventStore>) -> Self {
        Self { policy, store }
    }

    pub fn policy(&self) -> &ReviewPolicy {
        &self.policy
    }

    /// Process a single review round.
    pub async fn process_round(
        &self,
        panel: &ReviewPanel,
        collector: &ScoreCollector,
        _round: u32,
    ) -> Result<ReviewOutcome, LifecycleError> {
        let submissions = panel.submissions();

        if submissions.is_empty() {
            return Err(LifecycleError::NoReviewers);
        }

        let evaluator = ThresholdEvaluator::new(self.policy.clone());
        let result = evaluator.evaluate(collector);

        match result {
            ThresholdResult::Approved => Ok(ReviewOutcome::Approved),
            ThresholdResult::Rejected => {
                let consolidator = FeedbackConsolidator::new();
                let feedback =
                    consolidator.consolidate(&submissions.values().cloned().collect::<Vec<_>>());
                Ok(ReviewOutcome::Rejected { feedback })
            }
            ThresholdResult::ChangesRequested => {
                let consolidator = FeedbackConsolidator::new();
                let feedback =
                    consolidator.consolidate(&submissions.values().cloned().collect::<Vec<_>>());
                Ok(ReviewOutcome::ChangesRequested { feedback })
            }
            ThresholdResult::NeedMoreReviewers => Err(LifecycleError::NoReviewers),
        }
    }

    /// Check if max rounds exceeded.
    pub fn check_max_rounds(&self, current_round: u32) -> bool {
        current_round >= self.policy.max_review_rounds as u32
    }

    /// Create escalation outcome when max rounds exceeded.
    pub async fn escalate(
        &self,
        panel: &ReviewPanel,
        _collector: &ScoreCollector,
        reason: &str,
    ) -> Result<ReviewOutcome, LifecycleError> {
        let submissions: Vec<_> = panel.submissions().values().cloned().collect();
        let consolidator = FeedbackConsolidator::new();
        let feedback = consolidator.consolidate(&submissions);

        self.emit_escalation_event(reason).await?;

        Ok(ReviewOutcome::Escalate {
            reason: reason.to_string(),
            feedback,
        })
    }

    async fn emit_escalation_event(&self, reason: &str) -> Result<(), LifecycleError> {
        let event = Event {
            kind: EventKind::EscalationTriggered {
                reason: "Max review rounds exceeded".to_string(),
                context: format!("Review stuck: {}", reason),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: "review-coordinator".to_string(),
        };

        self.store
            .append(event)
            .await
            .map_err(|e| LifecycleError::StorageError(e.to_string()))?;

        Ok(())
    }

    /// Emit approval event.
    pub async fn emit_approval(&self, review_id: &str) -> Result<(), LifecycleError> {
        let event = Event {
            kind: EventKind::ReviewApproved {
                review_id: review_id.to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: review_id.to_string(),
        };

        self.store
            .append(event)
            .await
            .map_err(|e| LifecycleError::StorageError(e.to_string()))?;

        Ok(())
    }

    /// Emit changes requested event.
    pub async fn emit_changes_requested(&self, review_id: &str) -> Result<(), LifecycleError> {
        let event = Event {
            kind: EventKind::ReviewChangesRequested {
                review_id: review_id.to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: review_id.to_string(),
        };

        self.store
            .append(event)
            .await
            .map_err(|e| LifecycleError::StorageError(e.to_string()))?;

        Ok(())
    }

    /// Emit rejection event.
    pub async fn emit_rejection(&self, review_id: &str) -> Result<(), LifecycleError> {
        let event = Event {
            kind: EventKind::ReviewRejected {
                review_id: review_id.to_string(),
            },
            timestamp: chrono::Utc::now(),
            aggregate_id: review_id.to_string(),
        };

        self.store
            .append(event)
            .await
            .map_err(|e| LifecycleError::StorageError(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::InMemoryEventStore;

    fn default_policy() -> ReviewPolicy {
        ReviewPolicy {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        }
    }

    #[test]
    fn check_max_rounds() {
        let store = Arc::new(InMemoryEventStore::new());
        let lifecycle = ReviewLifecycle::new(default_policy(), store);

        assert!(!lifecycle.check_max_rounds(1));
        assert!(!lifecycle.check_max_rounds(2));
        assert!(lifecycle.check_max_rounds(3));
        assert!(lifecycle.check_max_rounds(4));
    }

    #[tokio::test]
    async fn emit_approval_event() {
        let store = Arc::new(InMemoryEventStore::new());
        let lifecycle = ReviewLifecycle::new(default_policy(), store.clone());

        lifecycle.emit_approval("R001").await.unwrap();

        let events = store.all_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, EventKind::ReviewApproved { .. }));
    }

    #[tokio::test]
    async fn emit_escalation_event() {
        let store = Arc::new(InMemoryEventStore::new());
        let lifecycle = ReviewLifecycle::new(default_policy(), store.clone());

        lifecycle.emit_escalation_event("deadlock").await.unwrap();

        let events = store.all_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].kind,
            EventKind::EscalationTriggered { .. }
        ));
    }
}
