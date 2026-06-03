//! AI decision types for supervisor judgment.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Request for an AI decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionRequest {
    /// Unique request identifier.
    pub request_id: String,
    /// Kind of decision needed.
    pub kind: DecisionKind,
    /// Context for the decision.
    pub context: String,
    /// Relevant event IDs.
    pub event_ids: Vec<String>,
    /// Available options (if applicable).
    pub options: Vec<String>,
    /// When request was created.
    pub created_at: DateTime<Utc>,
}

/// Kind of decision the AI can make.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DecisionKind {
    /// Plan execution order for tasks.
    PlanExecution,
    /// Choose worker for a task.
    AssignWorker,
    /// Provide guidance to stuck worker.
    StuckGuidance,
    /// Handle review deadlock.
    ReviewDeadlock,
    /// Handle merge conflict resolution.
    MergeConflict,
    /// Sanity check / heartbeat.
    HeartbeatCheck,
    /// Choose next action.
    NextAction,
    /// Handle error state.
    ErrorHandler,
}

/// AI decision response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecisionResponse {
    /// Original request ID.
    pub request_id: String,
    /// Decision made.
    pub decision: String,
    /// Reasoning for the decision.
    pub reasoning: String,
    /// Confidence level.
    pub confidence: DecisionConfidence,
    /// Suggested next actions.
    pub next_actions: Vec<String>,
    /// Tokens used for this decision.
    pub tokens_used: u64,
    /// When decision was made.
    pub decided_at: DateTime<Utc>,
}

/// Confidence level for a decision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DecisionConfidence {
    /// Low confidence - may need human review.
    Low,
    /// Medium confidence - reasonable certainty.
    Medium,
    /// High confidence - very certain.
    High,
}

impl DecisionConfidence {
    /// Convert to a floating-point value (Low=0.33, Medium=0.66, High=1.0).
    pub fn as_f32(&self) -> f32 {
        match self {
            DecisionConfidence::Low => 0.33,
            DecisionConfidence::Medium => 0.66,
            DecisionConfidence::High => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_kind_roundtrip() {
        let kinds = vec![
            DecisionKind::PlanExecution,
            DecisionKind::AssignWorker,
            DecisionKind::StuckGuidance,
            DecisionKind::ReviewDeadlock,
            DecisionKind::MergeConflict,
            DecisionKind::HeartbeatCheck,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: DecisionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn decision_request() {
        let request = DecisionRequest {
            request_id: "req-1".into(),
            kind: DecisionKind::AssignWorker,
            context: "Task T001 needs assignment".into(),
            event_ids: vec!["e1".into(), "e2".into()],
            options: vec!["agent-1".into(), "agent-2".into()],
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let parsed: DecisionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(request, parsed);
    }

    #[test]
    fn decision_confidence_ordering() {
        assert!(DecisionConfidence::Low < DecisionConfidence::Medium);
        assert!(DecisionConfidence::Medium < DecisionConfidence::High);
    }
}
