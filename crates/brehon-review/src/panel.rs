//! Review panel management.
//!
//! Handles spawning reviewers, maintaining panel affinity across rounds,
//! and tearing down sessions when reviews complete.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use brehon_ports::AgentGateway;
use brehon_types::{AgentId, ReviewId, ReviewScore, ReviewVerdict, SessionId, SessionSpec};

#[derive(Debug, Error)]
pub enum PanelError {
    #[error("Failed to spawn reviewer: {0}")]
    SpawnFailed(String),

    #[error("Failed to kill session: {0}")]
    KillFailed(String),

    #[error("Panel already exists for review {0}")]
    PanelExists(String),

    #[error("Panel not found: {0}")]
    NotFound(String),

    #[error("Reviewer not available")]
    ReviewerNotAvailable,

    #[error("Gateway error: {0}")]
    Gateway(String),
}

/// Individual reviewer's submission.
#[derive(Debug, Clone)]
pub struct ReviewerSubmission {
    pub reviewer_id: String,
    pub session_id: SessionId,
    pub score: ReviewScore,
    pub verdict: ReviewVerdict,
    pub findings: Vec<brehon_types::ReviewFinding>,
}

/// Tracks panel affinity - same reviewers across all rounds for a task.
#[derive(Debug, Clone)]
pub struct PanelAffinity {
    pub review_id: ReviewId,
    pub task_id: String,
    pub reviewer_ids: Vec<String>,
    pub round: u32,
    pub sessions: HashMap<String, SessionId>,
    pub submissions: HashMap<String, ReviewerSubmission>,
}

impl PanelAffinity {
    pub fn new(review_id: ReviewId, task_id: String, reviewer_ids: Vec<String>) -> Self {
        Self {
            review_id,
            task_id,
            reviewer_ids,
            round: 1,
            sessions: HashMap::new(),
            submissions: HashMap::new(),
        }
    }

    pub fn is_complete(&self, required_reviewers: usize) -> bool {
        self.submissions.len() >= required_reviewers.min(self.reviewer_ids.len())
    }

    pub fn advance_round(&mut self) {
        self.round += 1;
        self.submissions.clear();
    }

    pub fn has_more_rounds(&self, max_rounds: u8) -> bool {
        self.round < max_rounds as u32
    }

    pub fn get_missing_reviewers(&self) -> Vec<&String> {
        self.reviewer_ids
            .iter()
            .filter(|id| !self.submissions.contains_key(*id))
            .collect()
    }
}

/// Manages a panel of reviewers for a review.
///
/// Responsible for:
/// - Spawning reviewer sessions via AgentGateway
/// - Tracking panel affinity (same reviewers across rounds)
/// - Collecting submissions
/// - Tearing down sessions when complete
pub struct ReviewPanel {
    affinity: PanelAffinity,
    gateway: Arc<dyn AgentGateway>,
    worktree_path: String,
    #[allow(dead_code)]
    branch: String,
}

impl ReviewPanel {
    pub fn new(
        affinity: PanelAffinity,
        gateway: Arc<dyn AgentGateway>,
        worktree_path: String,
        branch: String,
    ) -> Self {
        Self {
            affinity,
            gateway,
            worktree_path,
            branch,
        }
    }

    pub fn affinity(&self) -> &PanelAffinity {
        &self.affinity
    }

    pub fn affinity_mut(&mut self) -> &mut PanelAffinity {
        &mut self.affinity
    }

    /// Spawn all reviewer sessions for this panel.
    pub async fn spawn_sessions(&mut self) -> Result<(), PanelError> {
        for reviewer_id in &self.affinity.reviewer_ids {
            if self.affinity.sessions.contains_key(reviewer_id) {
                continue;
            }

            let spec = SessionSpec::new(
                AgentId::new(reviewer_id),
                "reviewer".to_string(),
                self.worktree_path.clone(),
            );

            let session_id = self
                .gateway
                .spawn(spec)
                .await
                .map_err(|e| PanelError::SpawnFailed(e.to_string()))?;

            self.affinity
                .sessions
                .insert(reviewer_id.clone(), session_id);
        }

        Ok(())
    }

    /// Submit a review from a reviewer.
    pub fn submit(&mut self, submission: ReviewerSubmission) {
        self.affinity
            .submissions
            .insert(submission.reviewer_id.clone(), submission);
    }

    /// Get all submissions for the current round.
    pub fn submissions(&self) -> &HashMap<String, ReviewerSubmission> {
        &self.affinity.submissions
    }

    /// Get sessions for all reviewers.
    pub fn sessions(&self) -> &HashMap<String, SessionId> {
        &self.affinity.sessions
    }

    /// Kill all reviewer sessions.
    pub async fn kill_all_sessions(&self) -> Result<(), PanelError> {
        let mut errors = Vec::new();

        for (reviewer_id, session_id) in &self.affinity.sessions {
            if let Err(e) = self.gateway.kill_session(session_id).await {
                errors.push(format!("{} ({}): {}", reviewer_id, session_id, e));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(PanelError::KillFailed(format!(
                "failed to kill {} reviewer session(s): {}",
                errors.len(),
                errors.join(", ")
            )))
        }
    }

    /// Kill a specific reviewer's session.
    pub async fn kill_session(&self, reviewer_id: &str) -> Result<(), PanelError> {
        let session_id = self
            .affinity
            .sessions
            .get(reviewer_id)
            .ok_or_else(|| PanelError::NotFound(reviewer_id.to_string()))?;

        self.gateway
            .kill_session(session_id)
            .await
            .map_err(|e| PanelError::KillFailed(e.to_string()))
    }

    /// Check if all reviewers have submitted for this round.
    pub fn is_round_complete(&self) -> bool {
        self.affinity
            .reviewer_ids
            .iter()
            .all(|id| self.affinity.submissions.contains_key(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_test_harness::MockGateway;
    use std::sync::Arc;

    #[test]
    fn panel_affinity_creation() {
        let affinity = PanelAffinity::new(
            ReviewId::new("R001"),
            "T001".to_string(),
            vec!["reviewer-1".to_string(), "reviewer-2".to_string()],
        );

        assert_eq!(affinity.round, 1);
        assert_eq!(affinity.reviewer_ids.len(), 2);
        assert!(affinity.sessions.is_empty());
        assert!(affinity.submissions.is_empty());
    }

    #[test]
    fn panel_affinity_is_complete() {
        let mut affinity = PanelAffinity::new(
            ReviewId::new("R001"),
            "T001".to_string(),
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()],
        );

        assert!(!affinity.is_complete(3));

        affinity.submissions.insert(
            "r1".to_string(),
            ReviewerSubmission {
                reviewer_id: "r1".to_string(),
                session_id: SessionId::new("s1"),
                score: ReviewScore::new(8),
                verdict: ReviewVerdict::Approve,
                findings: vec![],
            },
        );

        assert!(!affinity.is_complete(3));

        affinity.submissions.insert(
            "r2".to_string(),
            ReviewerSubmission {
                reviewer_id: "r2".to_string(),
                session_id: SessionId::new("s2"),
                score: ReviewScore::new(7),
                verdict: ReviewVerdict::Approve,
                findings: vec![],
            },
        );

        affinity.submissions.insert(
            "r3".to_string(),
            ReviewerSubmission {
                reviewer_id: "r3".to_string(),
                session_id: SessionId::new("s3"),
                score: ReviewScore::new(9),
                verdict: ReviewVerdict::Approve,
                findings: vec![],
            },
        );

        assert!(affinity.is_complete(3));
        assert!(affinity.is_complete(2));
    }

    #[test]
    fn panel_affinity_advance_round() {
        let mut affinity = PanelAffinity::new(
            ReviewId::new("R001"),
            "T001".to_string(),
            vec!["r1".to_string()],
        );

        affinity.submissions.insert(
            "r1".to_string(),
            ReviewerSubmission {
                reviewer_id: "r1".to_string(),
                session_id: SessionId::new("s1"),
                score: ReviewScore::new(5),
                verdict: ReviewVerdict::ChangesRequested,
                findings: vec![],
            },
        );

        assert_eq!(affinity.round, 1);
        assert!(!affinity.submissions.is_empty());

        affinity.advance_round();

        assert_eq!(affinity.round, 2);
        assert!(affinity.submissions.is_empty());
    }

    #[test]
    fn panel_affinity_has_more_rounds() {
        let affinity = PanelAffinity::new(
            ReviewId::new("R001"),
            "T001".to_string(),
            vec!["r1".to_string()],
        );

        assert!(affinity.has_more_rounds(3));
        assert!(!affinity.has_more_rounds(1));
    }

    #[test]
    fn get_missing_reviewers() {
        let mut affinity = PanelAffinity::new(
            ReviewId::new("R001"),
            "T001".to_string(),
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()],
        );

        let missing = affinity.get_missing_reviewers();
        assert_eq!(missing.len(), 3);

        affinity.submissions.insert(
            "r1".to_string(),
            ReviewerSubmission {
                reviewer_id: "r1".to_string(),
                session_id: SessionId::new("s1"),
                score: ReviewScore::new(8),
                verdict: ReviewVerdict::Approve,
                findings: vec![],
            },
        );

        let missing = affinity.get_missing_reviewers();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&&"r2".to_string()));
        assert!(missing.contains(&&"r3".to_string()));
    }

    #[tokio::test]
    async fn kill_all_sessions_reports_failures() {
        let mut affinity = PanelAffinity::new(
            ReviewId::new("R-kill-fail"),
            "task".to_string(),
            vec!["reviewer-a".to_string()],
        );
        affinity
            .sessions
            .insert("reviewer-a".to_string(), SessionId::new("missing"));

        let gateway = Arc::new(MockGateway::new());
        let panel = ReviewPanel::new(
            affinity,
            gateway,
            "worktree".to_string(),
            "branch".to_string(),
        );

        let err = panel
            .kill_all_sessions()
            .await
            .expect_err("kill_all_sessions should fail for unknown session");

        let msg = err.to_string();
        assert!(msg.contains("failed to kill 1 reviewer session(s)"));
        assert!(msg.contains("reviewer-a (missing)"));
    }
}
