//! Task-related types for the task board and task lifecycle.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl TaskId {
    /// Create a new `TaskId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Task status with state machine transitions.
///
/// Valid transitions:
/// - Pending → Assigned (supervisor assigns)
/// - Assigned → InProgress (worker starts)
/// - InProgress → InReview (worker marks complete)
/// - InReview → ChangesRequested (reviewers request changes)
/// - ChangesRequested → InProgress (worker iterates)
/// - InReview → Approved (score threshold met)
/// - Approved → Merged (supervisor terminal action for merge-mode tasks)
/// - Approved → Closed (supervisor terminal action for close-mode tasks)
/// - InProgress → Blocked (dependency unmet / stuck)
/// - Blocked → Pending (dependency resolved)
/// - InProgress → Pending (worker dies / reassigned)
/// - InReview → Pending (review invalidated, e.g., stale)
///
/// Terminal states: Merged, Closed
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    /// Task waiting to be assigned.
    Pending,
    /// Task assigned to a worker, not yet started.
    Assigned,
    /// Worker actively working on task.
    InProgress,
    /// Task complete, under review.
    InReview,
    /// Reviewers requested changes, worker needs to iterate.
    ChangesRequested,
    /// Review passed, ready to merge.
    Approved,
    /// Merged to main, task complete.
    Merged,
    /// Task blocked on dependency or external issue.
    Blocked,
}

/// Task priority level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    /// Low priority.
    Low,
    /// Medium priority (default).
    Medium,
    /// High priority.
    High,
    /// Critical priority.
    Critical,
}

/// What terminal action approval should lead to for this task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskCompletionMode {
    /// Approved work should be merged.
    #[default]
    Merge,
    /// Approved work should be closed without a merge.
    Close,
}

impl TaskCompletionMode {
    /// Return the canonical lowercase string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Close => "close",
        }
    }
}

/// Parse a completion mode string to a typed value.
pub fn parse_task_completion_mode(mode: &str) -> Option<TaskCompletionMode> {
    match mode.trim() {
        "merge" | "Merge" => Some(TaskCompletionMode::Merge),
        "close" | "Close" => Some(TaskCompletionMode::Close),
        _ => None,
    }
}

/// Infer a sensible completion mode from task text when one is not explicit.
pub fn infer_task_completion_mode(title: &str, description: &str) -> TaskCompletionMode {
    let haystack = format!("{title}\n{description}").to_ascii_lowercase();
    let close_signals = [
        "audit",
        "audit-only",
        "audit only",
        "no code",
        "no-code",
        "no code changes",
        "no-code changes",
        "research",
        "investigation",
        "analysis",
        "design review",
        "design doc",
        "documentation only",
        "docs only",
    ];

    if close_signals.iter().any(|signal| haystack.contains(signal)) {
        TaskCompletionMode::Close
    } else {
        TaskCompletionMode::Merge
    }
}

/// Normalize a task status string to canonical snake_case form.
///
/// Accepts both snake_case and PascalCase variants (e.g., "in_progress" and "InProgress").
/// Returns None for unrecognized status values.
///
/// This is the single source of truth for status normalization across the system.
pub fn normalize_task_status(status: &str) -> Option<&'static str> {
    match status.trim() {
        "pending" | "Pending" => Some("pending"),
        "assigned" | "Assigned" => Some("assigned"),
        "in_progress" | "InProgress" => Some("in_progress"),
        "review_ready" | "ReviewReady" => Some("review_ready"),
        "complete" | "Complete" | "completed" | "Completed" => Some("review_ready"),
        "in_review" | "InReview" => Some("in_review"),
        "changes_requested" | "ChangesRequested" => Some("changes_requested"),
        "approved" | "Approved" => Some("approved"),
        "merged" | "Merged" => Some("merged"),
        "blocked" | "Blocked" => Some("blocked"),
        "closed" | "Closed" => Some("closed"),
        _ => None,
    }
}

/// Check if a task status is terminal (cannot transition to other states).
///
/// Terminal states: merged, closed
pub fn is_terminal_task_status(status: &str) -> bool {
    matches!(normalize_task_status(status), Some("merged" | "closed"))
}

/// A task on the board.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    /// Unique task identifier.
    pub id: TaskId,
    /// Task title.
    pub title: String,
    /// Detailed task description.
    pub description: String,
    /// Current status.
    pub status: TaskStatus,
    /// Priority level.
    pub priority: Priority,
    /// Agent assigned to this task (if any).
    pub assignee: Option<String>,
    /// Task dependencies (must complete before this one).
    pub dependencies: Vec<TaskId>,
    /// When task was created.
    pub created_at: DateTime<Utc>,
    /// When task was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Kind of note attached to a task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TaskNoteKind {
    /// General note.
    Note,
    /// Blocker identified.
    Blocker,
    /// Progress update.
    Progress,
    /// Outcome/result summary.
    Outcome,
}

/// A note attached to a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNote {
    /// Who wrote this note.
    pub author: String,
    /// Kind of note.
    pub kind: TaskNoteKind,
    /// Note content.
    pub content: String,
    /// When note was created.
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_display() {
        let id = TaskId::new("TASK-123");
        assert_eq!(format!("{}", id), "TASK-123");
    }

    #[test]
    fn task_status_roundtrip() {
        let status = TaskStatus::InProgress;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""InProgress""#);
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TaskStatus::InProgress);
    }

    #[test]
    fn normalize_legacy_completed_status_as_review_ready() {
        assert_eq!(normalize_task_status("completed"), Some("review_ready"));
        assert_eq!(normalize_task_status("Completed"), Some("review_ready"));
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Medium);
        assert!(Priority::Medium > Priority::Low);
    }

    #[test]
    fn task_construction() {
        let task = Task {
            id: TaskId::new("T001"),
            title: "Implement auth".into(),
            description: "Add JWT validation".into(),
            status: TaskStatus::Pending,
            priority: Priority::High,
            assignee: None,
            dependencies: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task, parsed);
    }

    #[test]
    fn parse_task_completion_mode_test() {
        assert_eq!(
            parse_task_completion_mode("merge"),
            Some(TaskCompletionMode::Merge)
        );
        assert_eq!(
            parse_task_completion_mode("close"),
            Some(TaskCompletionMode::Close)
        );
        assert_eq!(parse_task_completion_mode("unknown"), None);
    }

    #[test]
    fn infer_task_completion_mode_prefers_close_for_audit_work() {
        assert_eq!(
            infer_task_completion_mode(
                "Audit review lifecycle",
                "Audit-only task with no code changes"
            ),
            TaskCompletionMode::Close
        );
        assert_eq!(
            infer_task_completion_mode("Implement login", "Code changes in src/auth.rs"),
            TaskCompletionMode::Merge
        );
    }
}
