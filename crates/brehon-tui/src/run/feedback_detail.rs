//! Supervisor feedback rendering for the task detail dialog (Phase 6.9).
//!
//! Reads the compact `FeedbackTaskSummary` mirrored into
//! `.brehon/runtime/feedback/{task_id}.json` by the supervisor feedback
//! cache writer and appends a bounded "Supervisor Feedback" section to
//! the task detail lines.

use brehon_types::FeedbackTaskSummary;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::layout::{append_bullet_section, append_section_heading};

/// Append a "Supervisor Feedback" section to `lines` for the given task
/// summary. Renders nothing when the task has no feedback summary
/// attached, matching other optional task-detail sections.
pub(crate) fn append_feedback_section(
    lines: &mut Vec<Line<'static>>,
    feedback: &FeedbackTaskSummary,
) {
    let heading_color = if feedback.drain_active || feedback.safe_mode_active {
        crate::theme::detail::FINDING_BLOCKING
    } else if !feedback.escalations.is_empty() || !feedback.pending_clarifications.is_empty() {
        crate::theme::detail::FINDING_SUGGESTION
    } else {
        crate::theme::status::INFO
    };
    append_section_heading(lines, "Supervisor Feedback", heading_color);

    // Status row: drain/safe-mode flags + counts.
    let mut status_spans: Vec<Span<'static>> = vec![Span::styled("  ", Style::default())];
    let mode = if feedback.drain_active {
        "drain"
    } else if feedback.safe_mode_active {
        "safe mode"
    } else {
        "normal"
    };
    status_spans.push(Span::styled(
        "Mode ",
        Style::default().fg(crate::theme::chrome::TEXT_LABEL),
    ));
    status_spans.push(Span::styled(
        mode.to_string(),
        Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD),
    ));
    status_spans.push(Span::styled(
        "  │  ",
        Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
    ));
    let count_text = format!(
        "active {} · decisions {} · pending {} · escalations {}",
        feedback.active_triggers.len(),
        feedback.recent_decisions.len(),
        feedback.pending_clarifications.len(),
        feedback.escalations.len(),
    );
    status_spans.push(Span::styled(count_text, Style::default().fg(Color::White)));
    lines.push(Line::from(status_spans));

    if !feedback.active_triggers.is_empty() {
        let lines_text: Vec<String> = feedback
            .active_triggers
            .iter()
            .map(|trigger| format!("[{}] {}", trigger.kind, trigger.summary))
            .collect();
        append_bullet_section(
            lines,
            "Active triggers",
            &lines_text,
            crate::theme::detail::FINDING_SUGGESTION,
        );
    }

    if !feedback.recent_decisions.is_empty() {
        let lines_text: Vec<String> = feedback
            .recent_decisions
            .iter()
            .map(|decision| {
                format!(
                    "{} · {} · {}",
                    decision.decided_at, decision.outcome_kind, decision.summary
                )
            })
            .collect();
        append_bullet_section(
            lines,
            "Recent decisions",
            &lines_text,
            crate::theme::status::INFO,
        );
    }

    if !feedback.pending_clarifications.is_empty() {
        let lines_text: Vec<String> = feedback
            .pending_clarifications
            .iter()
            .map(|clarification| {
                format!(
                    "{} — {}",
                    clarification.requested_at, clarification.question
                )
            })
            .collect();
        append_bullet_section(
            lines,
            "Pending clarifications",
            &lines_text,
            crate::theme::detail::FINDING_SUGGESTION,
        );
    }

    if !feedback.escalations.is_empty() {
        let lines_text: Vec<String> = feedback
            .escalations
            .iter()
            .map(|esc| format!("{} — {}", esc.raised_at, esc.reason))
            .collect();
        append_bullet_section(
            lines,
            "Escalations",
            &lines_text,
            crate::theme::detail::FINDING_BLOCKING,
        );
    }

    if let Some(updated_at) = &feedback.updated_at {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                "Updated ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(updated_at.clone(), Style::default().fg(Color::White)),
        ]));
    }
}

#[cfg(test)]
mod feedback_detail_tests {
    use super::*;
    use brehon_types::{
        FeedbackClarificationSummary, FeedbackDecisionSummary, FeedbackEscalationSummary,
        FeedbackTriggerSummary,
    };

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

    fn populated_summary() -> FeedbackTaskSummary {
        FeedbackTaskSummary {
            active_triggers: vec![FeedbackTriggerSummary {
                trigger_id: "fb-1".into(),
                kind: "reviewer_followup".into(),
                summary: "Open follow-up FUP-1".into(),
                created_at: "2026-05-16T01:00:00Z".into(),
            }],
            recent_decisions: vec![FeedbackDecisionSummary {
                trigger_id: "fb-1".into(),
                turn_id: "turn-1".into(),
                outcome_kind: "promote_reviewer_followup".into(),
                summary: "promoted FUP-1 to T-followup-2".into(),
                decided_at: "2026-05-16T01:05:00Z".into(),
            }],
            pending_clarifications: vec![FeedbackClarificationSummary {
                trigger_id: "fb-2".into(),
                question: "Confirm rebase plan".into(),
                requested_at: "2026-05-16T01:06:00Z".into(),
            }],
            escalations: vec![FeedbackEscalationSummary {
                trigger_id: "fb-3".into(),
                reason: "Retry exhausted on run-1".into(),
                raised_at: "2026-05-16T01:07:00Z".into(),
            }],
            drain_active: false,
            safe_mode_active: false,
            updated_at: Some("2026-05-16T01:08:00Z".into()),
        }
    }

    #[test]
    fn renders_active_triggers_decisions_clarifications_and_escalations() {
        let summary = populated_summary();
        let mut lines = Vec::new();
        append_feedback_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Supervisor Feedback"));
        assert!(rendered.contains("Active triggers"));
        assert!(rendered.contains("Open follow-up FUP-1"));
        assert!(rendered.contains("Recent decisions"));
        assert!(rendered.contains("promote_reviewer_followup"));
        assert!(rendered.contains("Pending clarifications"));
        assert!(rendered.contains("Confirm rebase plan"));
        assert!(rendered.contains("Escalations"));
        assert!(rendered.contains("Retry exhausted on run-1"));
    }

    #[test]
    fn drain_status_renders_in_mode_row() {
        let mut summary = populated_summary();
        summary.drain_active = true;
        let mut lines = Vec::new();
        append_feedback_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Mode drain"));
    }

    #[test]
    fn safe_mode_renders_in_mode_row() {
        let mut summary = populated_summary();
        summary.safe_mode_active = true;
        let mut lines = Vec::new();
        append_feedback_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Mode safe mode"));
    }

    #[test]
    fn empty_feedback_summary_renders_only_status_row() {
        let summary = FeedbackTaskSummary::default();
        let mut lines = Vec::new();
        append_feedback_section(&mut lines, &summary);
        let rendered = lines_to_string(&lines);
        assert!(rendered.contains("Supervisor Feedback"));
        assert!(rendered.contains("active 0"));
        assert!(!rendered.contains("Active triggers"));
        assert!(!rendered.contains("Recent decisions"));
    }
}
