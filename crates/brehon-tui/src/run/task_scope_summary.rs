//! Compact task-scope summary for the dashboard task panel.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::chrome::TEXT_MUTED;

use super::rendering::truncate_to;
use super::task_detail::{compute_display_status, task_status_color};
use super::types::{DashboardData, TaskInfo};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TaskScopeSummary {
    pub required: usize,
    pub completed: usize,
    pub open: usize,
    pub active: usize,
    pub review: usize,
    pub blocked: usize,
    pub followups: usize,
}

impl TaskScopeSummary {
    pub(crate) fn from_dashboard(dashboard: &DashboardData) -> Self {
        let mut summary = Self::default();
        for task in dashboard
            .tasks
            .iter()
            .filter(|task| task.task_type == "task")
        {
            summary.required += 1;
            let status = compute_display_status(task);
            if task_counts_toward_completion(task) {
                summary.completed += 1;
            } else {
                summary.open += 1;
            }
            match status.as_str() {
                "assigned" | "in_progress" | "active_run" | "changes_requested" => {
                    summary.active += 1
                }
                "review_ready" | "in_review" | "approved" => summary.review += 1,
                "blocked" | "integration_conflict" | "run_failed" | "retry_exhausted" => {
                    summary.blocked += 1
                }
                _ => {}
            }
            summary.followups += open_followup_count(task);
        }
        summary
    }

    fn label_for_width(self, width: u16) -> String {
        let wide = format!(
            "Required {}  Completed {}  Open {}  Active {}  Review {}  Blocked {}  Follow-ups {}",
            self.required,
            self.completed,
            self.open,
            self.active,
            self.review,
            self.blocked,
            self.followups
        );
        if wide.width() <= width as usize {
            return wide;
        }

        let compact = format!(
            "Req {}  Done {}  Open {}  Act {}  Rev {}  Block {}  FUP {}",
            self.required,
            self.completed,
            self.open,
            self.active,
            self.review,
            self.blocked,
            self.followups
        );
        if compact.width() <= width as usize {
            return compact;
        }

        truncate_to(
            &format!(
                "Done {}/{}  Open {}  FUP {}",
                self.completed, self.required, self.open, self.followups
            ),
            width as usize,
        )
    }
}

pub(crate) fn task_scope_summary_line(dashboard: &DashboardData, width: u16) -> Line<'static> {
    let summary = TaskScopeSummary::from_dashboard(dashboard);
    let label = summary.label_for_width(width.saturating_sub(2));
    let status_kind = if summary.required > 0 && summary.open == 0 {
        "merged"
    } else if summary.blocked > 0 || summary.followups > 0 {
        "blocked"
    } else if summary.active > 0 || summary.review > 0 {
        "in_progress"
    } else {
        "pending"
    };

    Line::from(vec![
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            label,
            Style::default()
                .fg(task_status_color(status_kind))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

pub(crate) fn task_counts_toward_completion(task: &TaskInfo) -> bool {
    matches!(
        compute_display_status(task).as_str(),
        "closed" | "completed" | "merged" | "integrated" | "rejected"
    )
}

fn open_followup_count(task: &TaskInfo) -> usize {
    task.feedback.as_ref().map_or(0, |feedback| {
        feedback
            .active_triggers
            .iter()
            .filter(|trigger| trigger.kind == "reviewer_followup")
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str, task_type: &str) -> TaskInfo {
        TaskInfo {
            id: id.to_string(),
            title: id.to_string(),
            status: status.to_string(),
            assignee: None,
            task_type: task_type.to_string(),
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

    #[test]
    fn task_scope_summary_counts_required_completed_and_followups() {
        let mut followup_task = task("T-followup", "blocked", "task");
        followup_task.feedback = Some(brehon_types::FeedbackTaskSummary {
            active_triggers: vec![brehon_types::FeedbackTriggerSummary {
                trigger_id: "fup-1".into(),
                kind: "reviewer_followup".into(),
                summary: "Follow up".into(),
                created_at: "2026-05-17T00:00:00Z".into(),
            }],
            ..Default::default()
        });
        let dashboard = DashboardData {
            tasks: vec![
                task("I-ignored", "pending", "initiative"),
                task("T-done", "merged", "task"),
                task("T-active", "in_progress", "task"),
                task("T-review", "in_review", "task"),
                followup_task,
            ],
            ..DashboardData::default()
        };

        let summary = TaskScopeSummary::from_dashboard(&dashboard);

        assert_eq!(summary.required, 4);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.open, 3);
        assert_eq!(summary.active, 1);
        assert_eq!(summary.review, 1);
        assert_eq!(summary.blocked, 1);
        assert_eq!(summary.followups, 1);
    }

    #[test]
    fn task_scope_summary_uses_compact_label_when_narrow() {
        let summary = TaskScopeSummary {
            required: 12,
            completed: 7,
            open: 5,
            active: 2,
            review: 1,
            blocked: 1,
            followups: 1,
        };

        assert_eq!(summary.label_for_width(40), "Done 7/12  Open 5  FUP 1");
    }
}
