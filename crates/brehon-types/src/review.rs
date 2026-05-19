//! Review system types for scoring, feedback, and policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ReviewId(pub String);

impl ReviewId {
    /// Create a new `ReviewId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReviewId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Review score (1-10).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReviewScore(pub u8);

impl ReviewScore {
    /// Create a new score. Panics if not in 1-10 range.
    pub fn new(score: u8) -> Self {
        assert!((1..=10).contains(&score), "Score must be 1-10");
        Self(score)
    }

    /// Return the raw numeric score.
    pub fn as_u8(&self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for ReviewScore {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if (1..=10).contains(&value) {
            Ok(ReviewScore(value))
        } else {
            Err("Score must be between 1 and 10")
        }
    }
}

/// Reviewer verdict.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ReviewVerdict {
    /// Approve the change.
    Approve,
    /// Request changes before approval.
    ChangesRequested,
    /// Reject entirely (fundamental issues).
    Reject,
}

/// Review status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ReviewStatus {
    /// Review requested, waiting for panel.
    Pending,
    /// Review in progress.
    InProgress,
    /// Review completed.
    Completed,
}

/// Request for a review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewRequest {
    /// Unique review identifier.
    pub review_id: ReviewId,
    /// Task being reviewed.
    pub task_id: String,
    /// Branch to review.
    pub branch: String,
    /// Base branch (usually main).
    pub base_branch: String,
    /// Priority lane.
    pub priority: super::task::Priority,
    /// When review was requested.
    pub requested_at: DateTime<Utc>,
}

/// Review policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewPolicy {
    /// Minimum average score required for approval.
    pub min_average_score: u8,
    /// Minimum individual score required (no score below this).
    pub min_individual_score: u8,
    /// Score threshold for blocking (requires changes).
    pub blocking_score: u8,
    /// Minimum number of approvals required.
    pub min_approvals: u8,
    /// Whether blocking feedback must be resolved.
    pub require_blocking_feedback_resolution: bool,
    /// Maximum review rounds before escalation.
    pub max_review_rounds: u8,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            min_average_score: 7,
            min_individual_score: 6,
            blocking_score: 5,
            min_approvals: 2,
            require_blocking_feedback_resolution: true,
            max_review_rounds: 3,
        }
    }
}

/// Severity of an inline comment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum CommentSeverity {
    /// Must be fixed before approval.
    Blocking,
    /// Improves quality but not required.
    Suggestion,
    /// Minor improvement (style, etc).
    Nitpick,
}

/// Inline comment with file/line reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InlineComment {
    /// File path.
    pub file: String,
    /// Line number (1-indexed).
    pub line: u32,
    /// Comment content.
    pub content: String,
    /// Severity level.
    pub severity: CommentSeverity,
}

/// A finding from review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFinding {
    /// Finding description.
    pub description: String,
    /// File and line reference (if applicable).
    pub location: Option<InlineComment>,
    /// Suggested fix (if applicable).
    pub suggestion: Option<String>,
    /// Severity level.
    pub severity: CommentSeverity,
}

/// Consolidated feedback from multiple reviewers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsolidatedFeedback {
    /// Blocking issues (must be addressed).
    pub blocking: Vec<ReviewFinding>,
    /// Suggestions (improvements but not required).
    pub suggestions: Vec<ReviewFinding>,
    /// Nitpicks (minor issues).
    pub nitpicks: Vec<ReviewFinding>,
    /// Dissenting opinions (conflicting feedback).
    pub dissent: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_score_validation() {
        assert!(ReviewScore::try_from(0).is_err());
        assert!(ReviewScore::try_from(11).is_err());
        assert!(ReviewScore::try_from(5).is_ok());
        assert_eq!(ReviewScore::try_from(10).unwrap().as_u8(), 10);
    }

    #[test]
    fn review_policy_default() {
        let policy = ReviewPolicy::default();
        assert_eq!(policy.min_average_score, 7);
        assert_eq!(policy.min_individual_score, 6);
        assert_eq!(policy.blocking_score, 5);
        assert_eq!(policy.min_approvals, 2);
    }

    #[test]
    fn review_score_ordering() {
        let s1 = ReviewScore::new(5);
        let s2 = ReviewScore::new(8);
        assert!(s1 < s2);
    }

    #[test]
    fn consolidated_feedback_serialization() {
        let feedback = ConsolidatedFeedback {
            blocking: vec![ReviewFinding {
                description: "Error handling missing".into(),
                location: Some(InlineComment {
                    file: "src/main.rs".into(),
                    line: 42,
                    content: "Add error handling".into(),
                    severity: CommentSeverity::Blocking,
                }),
                suggestion: Some("Use ? operator".into()),
                severity: CommentSeverity::Blocking,
            }],
            suggestions: vec![],
            nitpicks: vec![],
            dissent: vec![],
        };
        let json = serde_json::to_string(&feedback).unwrap();
        let parsed: ConsolidatedFeedback = serde_json::from_str(&json).unwrap();
        assert_eq!(feedback, parsed);
    }
}
