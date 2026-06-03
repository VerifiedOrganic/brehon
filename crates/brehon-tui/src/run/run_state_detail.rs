//! Durable run-state rendering for the task detail dialog.

use chrono::Utc;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::layout::append_section_heading;
use super::types::{TaskInfo, TaskRunInfo};

pub(crate) fn append_run_state_section(
    lines: &mut Vec<Line<'static>>,
    task: &TaskInfo,
    run: &TaskRunInfo,
) {
    let now = Utc::now();
    let heading_color = if run.pending_confirmation || run.claim_is_stale_at(now) {
        crate::theme::status::WARNING
    } else if run.retry_exhausted() || run.is_failed() {
        crate::theme::status::ERROR
    } else {
        crate::theme::status::INFO
    };
    append_section_heading(lines, "Run State", heading_color);
    push_kv_row(
        lines,
        &[
            ("Status", Some(run.status.clone())),
            (
                "Attempt",
                run.attempt.map(|attempt| {
                    run.max_attempts
                        .map_or_else(|| attempt.to_string(), |max| format!("{attempt}/{max}"))
                }),
            ),
            ("Role", run.role.clone()),
        ],
    );
    push_kv_row(
        lines,
        &[
            ("Run", run.run_id.clone()),
            (
                "Task",
                Some(run.task_id.clone().unwrap_or_else(|| task.id.clone())),
            ),
        ],
    );
    push_kv_row(
        lines,
        &[
            ("Owner", run.owner.clone()),
            ("Session", run.session.clone()),
        ],
    );
    push_kv_row(
        lines,
        &[
            ("Last Activity", run.last_activity_at.clone()),
            ("Lease Expiry", run.lease_expires_at.clone()),
            ("Retry At", run.retry_at.clone()),
        ],
    );
    push_kv_row(
        lines,
        &[
            ("Updated", run.updated_at.clone()),
            ("Source", run.state_source.clone()),
            ("State", Some(run.confirmation_label_at(now).to_string())),
        ],
    );
    push_kv_row(
        lines,
        &[
            ("Retry Reason", run.retry_reason.clone()),
            ("Failure", run.failure_reason.clone()),
        ],
    );
}

fn push_kv_row(lines: &mut Vec<Line<'static>>, pairs: &[(&str, Option<String>)]) {
    let values = pairs
        .iter()
        .filter_map(|(label, value)| value.as_ref().map(|value| (*label, value.trim())))
        .filter(|(_, value)| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    let mut spans = vec![Span::styled("  ", Style::default())];
    for (idx, (label, value)) in values.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
        }
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ));
        spans.push(Span::styled(
            value.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(spans));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_string(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn task() -> TaskInfo {
        TaskInfo {
            id: "T-run".to_string(),
            title: "Run task".to_string(),
            status: "in_progress".to_string(),
            assignee: Some("worker-1".to_string()),
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

    fn run() -> TaskRunInfo {
        TaskRunInfo {
            run_id: Some("RUN-42".to_string()),
            task_id: Some("T-run".to_string()),
            role: Some("worker".to_string()),
            status: "running".to_string(),
            owner: Some("worker-1".to_string()),
            session: Some("session-1".to_string()),
            attempt: Some(2),
            max_attempts: Some(3),
            last_activity_at: Some("2999-05-16T12:01:00Z".to_string()),
            lease_expires_at: Some("2999-05-16T12:10:00Z".to_string()),
            retry_at: Some("2999-05-16T12:15:00Z".to_string()),
            retry_reason: Some("transient failure".to_string()),
            failure_reason: None,
            updated_at: Some("2026-05-16T12:02:00Z".to_string()),
            state_source: Some("durable projection".to_string()),
            continuation_turns: Some(1),
            retry_exhausted: false,
            pending_confirmation: false,
            stale: false,
        }
    }

    #[test]
    fn run_state_detail_renders_claim_timing_and_confirmation_fields() {
        let mut lines = Vec::new();
        append_run_state_section(&mut lines, &task(), &run());
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Run State"));
        assert!(rendered.contains("RUN-42"));
        assert!(rendered.contains("Owner worker-1"));
        assert!(rendered.contains("Session session-1"));
        assert!(rendered.contains("Lease Expiry 2999-05-16T12:10:00Z"));
        assert!(rendered.contains("Retry At 2999-05-16T12:15:00Z"));
        assert!(rendered.contains("State confirmed projection"));
    }

    #[test]
    fn run_state_detail_renders_pending_confirmation() {
        let mut run = run();
        run.pending_confirmation = true;
        let mut lines = Vec::new();
        append_run_state_section(&mut lines, &task(), &run);
        assert!(lines_to_string(&lines).contains("State pending confirmation"));
    }
}
