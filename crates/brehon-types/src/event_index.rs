//! Helpers for indexing events by their aggregate dimensions.

use crate::event::EventKind;

impl EventKind {
    /// Return the task id associated with this event, when the event belongs to a task.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Self::TaskCreated { task_id }
            | Self::TaskAssigned { task_id, .. }
            | Self::TaskCompleted { task_id }
            | Self::ReviewRequested { task_id, .. }
            | Self::MergePrepared { task_id, .. }
            | Self::MergeCommitted { task_id }
            | Self::MergeAborted { task_id, .. }
            | Self::WorkerReassigned { task_id, .. } => Some(task_id.as_str()),
            Self::RunCreated { task_id, .. }
            | Self::RunClaimed { task_id, .. }
            | Self::RunClaimRenewed { task_id, .. }
            | Self::RunStarted { task_id, .. }
            | Self::RunActivityObserved { task_id, .. }
            | Self::RunReleased { task_id, .. }
            | Self::RunRetryQueued { task_id, .. }
            | Self::RunCompleted { task_id, .. }
            | Self::RunFailed { task_id, .. }
            | Self::RunAbandoned { task_id, .. }
            | Self::StaleRunMutationRejected { task_id, .. }
            | Self::ProofBundleCreated { task_id, .. }
            | Self::ProofCommandRecorded { task_id, .. }
            | Self::ProofCheckRecorded { task_id, .. }
            | Self::ProofReviewLinked { task_id, .. }
            | Self::ProofIntegrationRecorded { task_id, .. }
            | Self::ProofDecisionRecorded { task_id, .. }
            | Self::ProofBlockerRecorded { task_id, .. }
            | Self::ProofBundleFinalized { task_id, .. } => Some(task_id.as_str()),
            Self::FeedbackTriggerDetected { task_id, .. }
            | Self::FeedbackBriefBuilt { task_id, .. }
            | Self::FeedbackTurnStarted { task_id, .. }
            | Self::FeedbackOutcomeReceived { task_id, .. }
            | Self::FeedbackOutcomeValidated { task_id, .. }
            | Self::FeedbackOutcomeRejected { task_id, .. }
            | Self::FeedbackDecisionRecorded { task_id, .. }
            | Self::FeedbackApplied { task_id, .. }
            | Self::FeedbackFailed { task_id, .. } => task_id.as_ref().map(|id| id.as_str()),
            _ => None,
        }
    }

    /// Return the review id associated with this event, when present.
    pub fn review_id(&self) -> Option<&str> {
        match self {
            Self::ReviewRequested { review_id, .. }
            | Self::ReviewScoreReceived { review_id, .. }
            | Self::ReviewApproved { review_id }
            | Self::ReviewRejected { review_id }
            | Self::ReviewChangesRequested { review_id } => Some(review_id.as_str()),
            Self::ProofReviewLinked { review, .. } => Some(review.review_id.as_str()),
            _ => None,
        }
    }

    /// Return the feedback trigger id associated with this event, when present.
    pub fn feedback_trigger_id(&self) -> Option<&str> {
        match self {
            Self::FeedbackTriggerDetected { trigger_id, .. }
            | Self::FeedbackBriefBuilt { trigger_id, .. }
            | Self::FeedbackTurnStarted { trigger_id, .. }
            | Self::FeedbackOutcomeReceived { trigger_id, .. }
            | Self::FeedbackOutcomeValidated { trigger_id, .. }
            | Self::FeedbackOutcomeRejected { trigger_id, .. }
            | Self::FeedbackDecisionRecorded { trigger_id, .. }
            | Self::FeedbackApplied { trigger_id, .. }
            | Self::FeedbackFailed { trigger_id, .. } => Some(trigger_id.as_str()),
            _ => None,
        }
    }

    /// Return the feedback turn id associated with this event, when present.
    pub fn feedback_turn_id(&self) -> Option<&str> {
        match self {
            Self::FeedbackBriefBuilt { turn_id, .. }
            | Self::FeedbackTurnStarted { turn_id, .. }
            | Self::FeedbackOutcomeReceived { turn_id, .. }
            | Self::FeedbackOutcomeValidated { turn_id, .. }
            | Self::FeedbackOutcomeRejected { turn_id, .. }
            | Self::FeedbackDecisionRecorded { turn_id, .. }
            | Self::FeedbackApplied { turn_id, .. }
            | Self::FeedbackFailed { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        }
    }

    /// Return the agent id associated with this event, when present.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::AgentSpawned { agent_id, .. }
            | Self::AgentDied { agent_id, .. }
            | Self::TaskAssigned { agent_id, .. } => Some(agent_id.as_str()),
            _ => None,
        }
    }
}
