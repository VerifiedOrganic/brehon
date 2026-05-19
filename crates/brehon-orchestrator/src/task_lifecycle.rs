//! Task lifecycle state machine.
//!
//! Valid transitions:
//! - \[*\] → Pending (task created)
//! - Pending → Assigned (supervisor assigns)
//! - Assigned → InProgress (worker starts)
//! - InProgress → InReview (worker marks complete)
//! - InReview → ChangesRequested (reviewers request changes)
//! - ChangesRequested → InProgress (worker iterates)
//! - InReview → Approved (score threshold met)
//! - Approved → Merged (supervisor merges to main)
//! - Merged → \[*\] (terminal)
//! - InProgress → Blocked (dependency unmet / stuck)
//! - Blocked → Pending (dependency resolved)
//! - InProgress → Pending (worker dies / reassigned)
//! - InReview → Pending (review invalidated, e.g., stale)

use brehon_types::TaskStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transition {
    Create,
    Assign,
    Start,
    Complete,
    RequestChanges,
    Iterate,
    Approve,
    Merge,
    Block,
    Unblock,
    Reassign,
    Invalidate,
}

impl Transition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transition::Create => "create",
            Transition::Assign => "assign",
            Transition::Start => "start",
            Transition::Complete => "complete",
            Transition::RequestChanges => "request_changes",
            Transition::Iterate => "iterate",
            Transition::Approve => "approve",
            Transition::Merge => "merge",
            Transition::Block => "block",
            Transition::Unblock => "unblock",
            Transition::Reassign => "reassign",
            Transition::Invalidate => "invalidate",
        }
    }
}

pub struct TaskLifecycle;

impl TaskLifecycle {
    pub fn valid_transitions_from(status: TaskStatus) -> Vec<Transition> {
        match status {
            TaskStatus::Pending => vec![Transition::Assign],
            TaskStatus::Assigned => vec![Transition::Start, Transition::Reassign],
            TaskStatus::InProgress => vec![
                Transition::Complete,
                Transition::Block,
                Transition::Reassign,
            ],
            TaskStatus::InReview => vec![
                Transition::RequestChanges,
                Transition::Approve,
                Transition::Invalidate,
            ],
            TaskStatus::ChangesRequested => vec![Transition::Iterate],
            TaskStatus::Approved => vec![Transition::Merge],
            TaskStatus::Merged => vec![],
            TaskStatus::Blocked => vec![Transition::Unblock],
        }
    }

    pub fn can_transition(from: TaskStatus, transition: Transition) -> bool {
        Self::valid_transitions_from(from).contains(&transition)
    }

    pub fn apply(status: TaskStatus, transition: Transition) -> crate::Result<TaskStatus> {
        if !Self::can_transition(status, transition) {
            return Err(crate::error::OrchestratorError::InvalidTransition(format!(
                "Cannot apply {:?} transition to task in {:?} state",
                transition, status
            )));
        }

        let new_status = match (status, transition) {
            (TaskStatus::Pending, Transition::Assign) => TaskStatus::Assigned,
            (TaskStatus::Assigned, Transition::Start) => TaskStatus::InProgress,
            (TaskStatus::Assigned, Transition::Reassign) => TaskStatus::Pending,
            (TaskStatus::InProgress, Transition::Complete) => TaskStatus::InReview,
            (TaskStatus::InProgress, Transition::Block) => TaskStatus::Blocked,
            (TaskStatus::InProgress, Transition::Reassign) => TaskStatus::Pending,
            (TaskStatus::InReview, Transition::RequestChanges) => TaskStatus::ChangesRequested,
            (TaskStatus::InReview, Transition::Approve) => TaskStatus::Approved,
            (TaskStatus::InReview, Transition::Invalidate) => TaskStatus::Pending,
            (TaskStatus::ChangesRequested, Transition::Iterate) => TaskStatus::InProgress,
            (TaskStatus::Approved, Transition::Merge) => TaskStatus::Merged,
            (TaskStatus::Blocked, Transition::Unblock) => TaskStatus::Pending,
            _ => {
                return Err(crate::error::OrchestratorError::InvalidTransition(format!(
                    "Invalid transition {:?} from {:?}",
                    transition, status
                )));
            }
        };

        Ok(new_status)
    }

    pub fn is_terminal(status: TaskStatus) -> bool {
        matches!(status, TaskStatus::Merged)
    }

    pub fn is_active(status: TaskStatus) -> bool {
        matches!(
            status,
            TaskStatus::Assigned
                | TaskStatus::InProgress
                | TaskStatus::InReview
                | TaskStatus::ChangesRequested
                | TaskStatus::Approved
        )
    }

    pub fn is_blocked(status: TaskStatus) -> bool {
        matches!(status, TaskStatus::Blocked)
    }

    pub fn is_pending(status: TaskStatus) -> bool {
        matches!(status, TaskStatus::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions_from_pending() {
        let transitions = TaskLifecycle::valid_transitions_from(TaskStatus::Pending);
        assert_eq!(transitions, vec![Transition::Assign]);
    }

    #[test]
    fn valid_transitions_from_in_progress() {
        let transitions = TaskLifecycle::valid_transitions_from(TaskStatus::InProgress);
        assert_eq!(
            transitions,
            vec![
                Transition::Complete,
                Transition::Block,
                Transition::Reassign
            ]
        );
    }

    #[test]
    fn valid_transitions_from_merged() {
        let transitions = TaskLifecycle::valid_transitions_from(TaskStatus::Merged);
        assert!(transitions.is_empty());
    }

    #[test]
    fn can_transition_valid() {
        assert!(TaskLifecycle::can_transition(
            TaskStatus::Pending,
            Transition::Assign
        ));
        assert!(TaskLifecycle::can_transition(
            TaskStatus::InProgress,
            Transition::Complete
        ));
    }

    #[test]
    fn cannot_transition_invalid() {
        assert!(!TaskLifecycle::can_transition(
            TaskStatus::Pending,
            Transition::Merge
        ));
        assert!(!TaskLifecycle::can_transition(
            TaskStatus::Merged,
            Transition::Assign
        ));
    }

    #[test]
    fn apply_valid_transition() {
        let result = TaskLifecycle::apply(TaskStatus::Pending, Transition::Assign);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), TaskStatus::Assigned);

        let result = TaskLifecycle::apply(TaskStatus::InProgress, Transition::Complete);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), TaskStatus::InReview);
    }

    #[test]
    fn apply_invalid_transition() {
        let result = TaskLifecycle::apply(TaskStatus::Pending, Transition::Merge);
        assert!(result.is_err());

        let result = TaskLifecycle::apply(TaskStatus::Merged, Transition::Assign);
        assert!(result.is_err());
    }

    #[test]
    fn full_lifecycle() {
        let mut status = TaskStatus::Pending;

        status = TaskLifecycle::apply(status, Transition::Assign).unwrap();
        assert_eq!(status, TaskStatus::Assigned);

        status = TaskLifecycle::apply(status, Transition::Start).unwrap();
        assert_eq!(status, TaskStatus::InProgress);

        status = TaskLifecycle::apply(status, Transition::Complete).unwrap();
        assert_eq!(status, TaskStatus::InReview);

        status = TaskLifecycle::apply(status, Transition::Approve).unwrap();
        assert_eq!(status, TaskStatus::Approved);

        status = TaskLifecycle::apply(status, Transition::Merge).unwrap();
        assert_eq!(status, TaskStatus::Merged);

        assert!(TaskLifecycle::is_terminal(status));
    }

    #[test]
    fn block_unblock_cycle() {
        let mut status = TaskStatus::InProgress;

        status = TaskLifecycle::apply(status, Transition::Block).unwrap();
        assert_eq!(status, TaskStatus::Blocked);
        assert!(TaskLifecycle::is_blocked(status));

        status = TaskLifecycle::apply(status, Transition::Unblock).unwrap();
        assert_eq!(status, TaskStatus::Pending);
        assert!(TaskLifecycle::is_pending(status));
    }

    #[test]
    fn review_changes_cycle() {
        let mut status = TaskStatus::InProgress;

        status = TaskLifecycle::apply(status, Transition::Complete).unwrap();
        assert_eq!(status, TaskStatus::InReview);

        status = TaskLifecycle::apply(status, Transition::RequestChanges).unwrap();
        assert_eq!(status, TaskStatus::ChangesRequested);

        status = TaskLifecycle::apply(status, Transition::Iterate).unwrap();
        assert_eq!(status, TaskStatus::InProgress);
    }

    #[test]
    fn invalidate_review() {
        let status = TaskStatus::InReview;
        let new_status = TaskLifecycle::apply(status, Transition::Invalidate).unwrap();
        assert_eq!(new_status, TaskStatus::Pending);
    }

    #[test]
    fn reassign_from_assigned() {
        let status = TaskStatus::Assigned;
        let new_status = TaskLifecycle::apply(status, Transition::Reassign).unwrap();
        assert_eq!(new_status, TaskStatus::Pending);
    }

    #[test]
    fn reassign_from_in_progress() {
        let status = TaskStatus::InProgress;
        let new_status = TaskLifecycle::apply(status, Transition::Reassign).unwrap();
        assert_eq!(new_status, TaskStatus::Pending);
    }

    #[test]
    fn is_active_status() {
        assert!(TaskLifecycle::is_active(TaskStatus::Assigned));
        assert!(TaskLifecycle::is_active(TaskStatus::InProgress));
        assert!(TaskLifecycle::is_active(TaskStatus::InReview));
        assert!(TaskLifecycle::is_active(TaskStatus::ChangesRequested));
        assert!(TaskLifecycle::is_active(TaskStatus::Approved));

        assert!(!TaskLifecycle::is_active(TaskStatus::Pending));
        assert!(!TaskLifecycle::is_active(TaskStatus::Merged));
        assert!(!TaskLifecycle::is_active(TaskStatus::Blocked));
    }
}
