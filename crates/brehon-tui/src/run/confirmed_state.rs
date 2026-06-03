//! Confirmed-state summaries for the dashboard.

use chrono::Utc;

use super::types::{DashboardData, TaskInfo};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttentionCounts {
    pub failed_runs: usize,
    pub retry_queued: usize,
    pub stale_claims: usize,
    pub review_blockers: usize,
    pub integration_conflicts: usize,
    pub feedback_escalations: usize,
    pub permission_blockers: usize,
}

impl AttentionCounts {
    pub(crate) fn from_dashboard(dashboard: &DashboardData) -> Self {
        let now = Utc::now();
        let mut counts = Self::default();
        for task in &dashboard.tasks {
            if let Some(run) = task.run.as_ref() {
                counts.failed_runs += usize::from(run.is_failed() || run.retry_exhausted());
                counts.retry_queued += usize::from(run.is_retry_queued());
                counts.stale_claims += usize::from(run.claim_is_stale_at(now));
            }
            counts.review_blockers += usize::from(task_has_review_blocker(task));
            counts.integration_conflicts += usize::from(task_has_integration_conflict(task));
            counts.feedback_escalations += usize::from(task_has_feedback_escalation(task));
            counts.permission_blockers += usize::from(task_has_permission_blocker(task));
        }
        counts
    }

    fn is_clear(&self) -> bool {
        self.failed_runs == 0
            && self.retry_queued == 0
            && self.stale_claims == 0
            && self.review_blockers == 0
            && self.integration_conflicts == 0
            && self.feedback_escalations == 0
            && self.permission_blockers == 0
    }
}

pub(crate) fn attention_lane_label(dashboard: &DashboardData) -> String {
    AttentionCounts::from_dashboard(dashboard).to_label()
}

impl AttentionCounts {
    fn to_label(&self) -> String {
        if self.is_clear() {
            return String::new();
        }
        format!(
            "attention fail:{} retry:{} stale:{} review:{} conflict:{} feedback:{} perm:{}",
            self.failed_runs,
            self.retry_queued,
            self.stale_claims,
            self.review_blockers,
            self.integration_conflicts,
            self.feedback_escalations,
            self.permission_blockers
        )
    }
}

pub(crate) fn runtime_command_confirmation_suffix(status: &str) -> &'static str {
    match status {
        "pending" => " confirmation=pending",
        "accepted" => " confirmation=runtime-accepted",
        "applied" => " confirmation=runtime-applied",
        "deferred" => " confirmation=durable-retry",
        _ => "",
    }
}

fn task_has_review_blocker(task: &TaskInfo) -> bool {
    !task.review_feedback_blocking.is_empty()
        || task
            .review_feedback_outcome
            .as_deref()
            .is_some_and(|outcome| outcome == "changes_requested" || outcome == "blocked")
}

fn task_has_integration_conflict(task: &TaskInfo) -> bool {
    task.integration_conflict_owner.as_deref() == Some("supervisor")
        || task
            .proof
            .as_ref()
            .is_some_and(|proof| !proof.integration_conflicts.is_empty())
}

fn task_has_feedback_escalation(task: &TaskInfo) -> bool {
    task.feedback.as_ref().is_some_and(|feedback| {
        feedback.drain_active || feedback.safe_mode_active || !feedback.escalations.is_empty()
    })
}

fn task_has_permission_blocker(task: &TaskInfo) -> bool {
    let scalar_text = [
        task.blockers.as_deref(),
        task.activity.as_deref(),
        task.notes.as_deref(),
    ];
    scalar_text
        .into_iter()
        .flatten()
        .any(looks_like_permission_blocker)
        || task
            .review_feedback_blocking
            .iter()
            .any(|text| looks_like_permission_blocker(text))
        || task.proof.as_ref().is_some_and(|proof| {
            proof
                .open_blockers
                .iter()
                .chain(proof.missing.iter())
                .any(|text| looks_like_permission_blocker(text))
        })
        || task.feedback.as_ref().is_some_and(|feedback| {
            feedback
                .active_triggers
                .iter()
                .any(|trigger| looks_like_permission_blocker(&trigger.summary))
                || feedback
                    .escalations
                    .iter()
                    .any(|escalation| looks_like_permission_blocker(&escalation.reason))
        })
}

fn looks_like_permission_blocker(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["permission", "approval", "credential", "auth", "denied"]
        .iter()
        .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::types::TaskRunInfo;
    use brehon_types::{FeedbackEscalationSummary, FeedbackTaskSummary, ProofSummary};

    fn task(id: &str) -> TaskInfo {
        TaskInfo {
            id: id.to_string(),
            title: id.to_string(),
            status: "in_progress".to_string(),
            assignee: None,
            task_type: "task".to_string(),
            parent_id: None,
            description: String::new(),
            priority: None,
            percent: None,
            tokens_used: 0,
            completion_mode: None,
            merge_target: None,
            integration_status: None,
            integration_branch: None,
            integration_worktree: None,
            activity: None,
            notes: None,
            blockers: None,
            dependencies: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
            closed_at: None,
            closed_by: None,
            merged_commit: None,
            merged_branch: None,
            latest_commit: None,
            run: None,
            review_id: None,
            review_status: None,
            review_round: None,
            review_panel_id: None,
            review_panel_members: Vec::new(),
            review_panel_lease_state: None,
            review_feedback_outcome: None,
            review_feedback_threshold_reason: None,
            review_feedback_evaluated_at: None,
            review_feedback_blocking: Vec::new(),
            review_feedback_suggestions: Vec::new(),
            review_feedback_nitpicks: Vec::new(),
            review_feedback_dissent: Vec::new(),
            integration_conflict_owner: None,
            integration_conflict_source: None,
            integration_conflict_merge_target: None,
            integration_conflict_reviewed_commit: None,
            integration_conflict_previous_worker: None,
            integration_conflict_conflicting_files: Vec::new(),
            acceptance_criteria: Vec::new(),
            file_hints: Vec::new(),
            constraints: Vec::new(),
            test_requirements: Vec::new(),
            plan_steps: Vec::new(),
            implementation_notes: None,
            research_context: Vec::new(),
            proof: None,
            feedback: None,
        }
    }

    fn run(status: &str) -> TaskRunInfo {
        TaskRunInfo {
            run_id: Some("RUN-1".to_string()),
            task_id: Some("T-1".to_string()),
            role: Some("worker".to_string()),
            status: status.to_string(),
            owner: None,
            session: None,
            attempt: Some(1),
            max_attempts: Some(2),
            last_activity_at: None,
            lease_expires_at: None,
            retry_at: None,
            retry_reason: None,
            failure_reason: None,
            updated_at: None,
            state_source: Some("durable projection".to_string()),
            continuation_turns: None,
            retry_exhausted: false,
            pending_confirmation: false,
            stale: false,
        }
    }

    #[test]
    fn run_state_attention_counts_all_operator_lanes() {
        let mut failed = task("T-failed");
        failed.run = Some(run("failed"));
        let mut retry = task("T-retry");
        retry.run = Some(run("retry_queued"));
        let mut stale = task("T-stale");
        let mut stale_run = run("running");
        stale_run.stale = true;
        stale.run = Some(stale_run);
        let mut review = task("T-review");
        review.review_feedback_blocking = vec!["fix data loss".to_string()];
        let mut conflict = task("T-conflict");
        conflict.integration_conflict_owner = Some("supervisor".to_string());
        let mut feedback = task("T-feedback");
        feedback.feedback = Some(FeedbackTaskSummary {
            escalations: vec![FeedbackEscalationSummary {
                trigger_id: "fb-1".to_string(),
                reason: "retry exhausted".to_string(),
                raised_at: "2026-05-16T00:00:00Z".to_string(),
            }],
            ..Default::default()
        });
        let mut permission = task("T-permission");
        let mut proof = ProofSummary::absent();
        proof.open_blockers = vec!["needs approval token".to_string()];
        permission.proof = Some(proof);

        let counts = AttentionCounts::from_dashboard(&DashboardData {
            tasks: vec![failed, retry, stale, review, conflict, feedback, permission],
            ..Default::default()
        });

        assert_eq!(counts.failed_runs, 1);
        assert_eq!(counts.retry_queued, 1);
        assert_eq!(counts.stale_claims, 1);
        assert_eq!(counts.review_blockers, 1);
        assert_eq!(counts.integration_conflicts, 1);
        assert_eq!(counts.feedback_escalations, 1);
        assert_eq!(counts.permission_blockers, 1);
        assert!(counts.to_label().contains("perm:1"));
    }

    #[test]
    fn pending_runtime_command_uses_confirmation_suffix() {
        assert_eq!(
            runtime_command_confirmation_suffix("pending"),
            " confirmation=pending"
        );
        assert_eq!(
            runtime_command_confirmation_suffix("applied"),
            " confirmation=runtime-applied"
        );
    }
}
