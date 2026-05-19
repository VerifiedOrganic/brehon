use super::panel::PanelReviewerReplacement;

/// Actions taken during periodic review maintenance (timeout or panel reassignment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMaintenanceAction {
    RecoveredOrphanedGate {
        task_id: String,
        from_status: String,
        to_status: String,
    },
    ReleasedDeadWorkerAssignment {
        task_id: String,
        status: String,
        previous_assignee: String,
    },
    ReleasedStaleReviewState {
        task_id: String,
        review_id: String,
        review_status: String,
        task_status: String,
    },
    AutoRequestedReview {
        task_id: String,
        review_id: String,
        panel_id: String,
    },
    TimedOut {
        task_id: String,
        review_id: String,
        outcome: String,
    },
    ReassignedPanel {
        task_id: String,
        review_id: String,
        panel_id: String,
        replacements: Vec<PanelReviewerReplacement>,
    },
}

impl ReviewMaintenanceAction {
    /// Return a human-readable description of the maintenance action taken.
    pub fn message(&self) -> String {
        match self {
            Self::RecoveredOrphanedGate {
                task_id,
                from_status,
                to_status,
            } => format!(
                "Recovered orphaned review gate for {task_id}; {from_status} -> {to_status}"
            ),
            Self::ReleasedDeadWorkerAssignment {
                task_id,
                status,
                previous_assignee,
            } => format!(
                "Released dead worker assignment for {task_id}; {status} no longer assigned to {previous_assignee}"
            ),
            Self::ReleasedStaleReviewState {
                task_id,
                review_id,
                review_status,
                task_status,
            } => format!(
                "Released stale review {review_id} for {task_id}; review was {review_status} but task is {task_status}"
            ),
            Self::AutoRequestedReview {
                task_id,
                review_id,
                panel_id,
            } => format!(
                "Auto-requested review {review_id} for {task_id} on panel {panel_id}"
            ),
            Self::TimedOut {
                task_id,
                review_id,
                outcome,
            } => format!("Review {review_id} for {task_id} timed out; outcome is now {outcome}"),
            Self::ReassignedPanel {
                task_id,
                review_id,
                panel_id,
                replacements,
            } => {
                let details = replacements
                    .iter()
                    .map(|replacement| {
                        format!("{} -> {}", replacement.removed, replacement.replaced_with)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Review {review_id} for {task_id} re-seated on panel {panel_id}; {details}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct PanelReassignmentResult {
    pub(super) review_id: String,
    pub(super) panel_id: String,
    pub(super) panel: Vec<String>,
    pub(super) replacements: Vec<PanelReviewerReplacement>,
    pub(super) prompts_sent_to: Vec<String>,
    pub(super) submissions_already_received: Vec<String>,
}
