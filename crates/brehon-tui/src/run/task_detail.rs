//! Task detail dialog: building detail lines, rendering the overlay, and mouse handling.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::components::Panel;
use ratatui::Frame;

use crate::theme::chrome::TEXT_DIM;
use crate::theme::{status_style, StatusKind};
use brehon_types::task::normalize_task_status;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use super::helpers::description_mentions_heading;
use super::layout::{
    append_bullet_section, append_section_heading, append_section_rule, append_text_block,
    centered_dialog_rect, expand_rect, inset_rect,
};
use super::rendering::truncate_with_ellipsis;
use super::types::*;

fn format_token_count(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted.chars().rev().collect()
}

pub(crate) fn compute_display_status(task: &TaskInfo) -> String {
    let normalized = task.status.to_ascii_lowercase();

    if normalized == "changes_requested"
        && task.integration_conflict_owner.as_deref() == Some("supervisor")
    {
        return "integration_conflict".to_string();
    }

    if normalized == "approved" || normalized == "closed" {
        if let Some(ref integ_status) = task.integration_status {
            if integ_status == "integrated" {
                return "integrated".to_string();
            }
        }
    }

    if task
        .merged_commit
        .as_ref()
        .is_some_and(|commit| !commit.is_empty())
        || task
            .merged_branch
            .as_ref()
            .is_some_and(|branch| !branch.is_empty())
    {
        return "merged".to_string();
    }

    if normalized == "in_review" {
        match task.review_status.as_deref() {
            Some("collecting") => return "in_review".to_string(),
            Some(other) => return other.to_string(),
            None => return "review_ready".to_string(),
        }
    }

    if let Some(display_status) = task.run.as_ref().and_then(TaskRunInfo::display_status) {
        return display_status.to_string();
    }

    normalized
}

pub(crate) fn format_task_reference(
    task_id: &str,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> String {
    match tasks_by_id.get(task_id).copied() {
        Some(task) => format!(
            "{} — {} [{}]",
            task.id,
            task.title,
            compute_display_status(task)
        ),
        None => task_id.to_string(),
    }
}

pub(crate) fn format_task_reference_list(
    task_ids: &[String],
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> Vec<String> {
    task_ids
        .iter()
        .filter(|task_id| !task_id.trim().is_empty())
        .map(|task_id| format_task_reference(task_id, tasks_by_id))
        .collect()
}

pub(crate) fn task_dashboard_hint(
    task: &TaskInfo,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> Option<String> {
    if task.integration_conflict_owner.as_deref() == Some("supervisor") {
        let first = task
            .integration_conflict_conflicting_files
            .first()
            .map(|value| truncate_with_ellipsis(value, 32))
            .unwrap_or_else(|| "unknown files".to_string());
        return Some(if task.integration_conflict_conflicting_files.len() > 1 {
            format!(
                "  [supervisor conflict: {} +{}]",
                first,
                task.integration_conflict_conflicting_files
                    .len()
                    .saturating_sub(1)
            )
        } else {
            format!("  [supervisor conflict: {first}]")
        });
    }

    if let Some(run) = task.run.as_ref() {
        return Some(format!("  [{}]", run.dashboard_hint(chrono::Utc::now())));
    }

    if normalize_task_status(&task.status) != Some("blocked") {
        return None;
    }

    if !task.blocked_by.is_empty() {
        let first = format_task_reference(&task.blocked_by[0], tasks_by_id);
        let compact_first = truncate_with_ellipsis(&first, 42);
        return Some(if task.blocked_by.len() > 1 {
            format!(
                "  [waiting on {} +{}]",
                compact_first,
                task.blocked_by.len().saturating_sub(1)
            )
        } else {
            format!("  [waiting on {compact_first}]")
        });
    }

    task.blockers
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| format!("  [blocked: {}]", truncate_with_ellipsis(text, 42)))
}

pub(crate) fn task_status_kind(status: &str) -> StatusKind {
    match status {
        "pending" | "Pending" | "assigned" | "Assigned" => StatusKind::Idle,
        "in_progress" | "InProgress" | "active_run" => StatusKind::Info,
        "review_ready" | "ReviewReady" | "in_review" | "InReview" => StatusKind::Running,
        "changes_requested" | "ChangesRequested" | "retry_queued" => StatusKind::Warning,
        "integration_conflict" | "blocked" | "Blocked" | "rejected" => StatusKind::Error,
        "run_failed" | "retry_exhausted" => StatusKind::Error,
        "approved" | "Approved" | "integrated" | "Integrated" | "merged" | "Merged" => {
            StatusKind::Success
        }
        "closed" | "Closed" => StatusKind::Idle,
        _ => StatusKind::Idle,
    }
}

pub(crate) fn task_status_style(status: &str) -> Style {
    status_style(task_status_kind(status))
}

pub(crate) fn task_status_color(status: &str) -> Color {
    task_status_style(status).fg.unwrap_or(TEXT_DIM)
}

pub(crate) fn build_task_detail_lines(
    task: &TaskInfo,
    dashboard: &DashboardData,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let tasks_by_id: std::collections::HashMap<&str, &TaskInfo> = dashboard
        .tasks
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate))
        .collect();

    let display_status = compute_display_status(task);
    let status_color = task_status_color(&display_status);

    let type_label = match task.task_type.as_str() {
        "initiative" => "INITIATIVE",
        "epic" => "EPIC",
        "task" => "TASK",
        other => other,
    };
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            task.id.clone(),
            Style::default()
                .fg(DASH_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            type_label.to_string(),
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!(" {} ", display_status),
            Style::default()
                .fg(crate::theme::detail::STATUS_BADGE_TEXT)
                .bg(status_color),
        ),
    ]));

    let mut meta_parts: Vec<(&str, String)> = Vec::new();
    if let Some(ref assignee) = task.assignee {
        meta_parts.push(("Assignee", assignee.clone()));
    }
    if let Some(percent) = task.percent {
        meta_parts.push(("Progress", format!("{percent}%")));
    }
    if task.tokens_used > 0 {
        meta_parts.push(("Tokens", format_token_count(task.tokens_used)));
    }
    if let Some(run) = task.run.as_ref() {
        meta_parts.push(("Run", run.dashboard_hint(chrono::Utc::now())));
    }
    if let Some(ref mode) = task.completion_mode {
        meta_parts.push(("Completion", mode.clone()));
    }
    if let Some(ref priority) = task.priority {
        meta_parts.push(("Priority", priority.clone()));
    }
    if let Some(ref parent_id) = task.parent_id {
        meta_parts.push(("Parent", parent_id.clone()));
    }

    if !meta_parts.is_empty() {
        let mut spans = vec![Span::styled("  ", Style::default())];
        for (i, (label, value)) in meta_parts.iter().enumerate() {
            if i > 0 {
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
                value.clone(),
                Style::default().fg(Color::White),
            ));
        }
        lines.push(Line::from(spans));
    }

    let child_ids: Vec<String> = dashboard
        .tasks
        .iter()
        .filter(|candidate| candidate.parent_id.as_deref() == Some(task.id.as_str()))
        .map(|candidate| candidate.id.clone())
        .collect();
    if !child_ids.is_empty() {
        let child_label = match task.task_type.as_str() {
            "initiative" => "Epics",
            "epic" => "Subtasks",
            _ => "Children",
        };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("{child_label} "),
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
            Span::styled(
                format!("{} ({})", child_ids.join(", "), child_ids.len()),
                Style::default().fg(Color::White),
            ),
        ]));
    }

    append_section_rule(&mut lines, 60);

    if let Some(ref run) = task.run {
        super::run_state_detail::append_run_state_section(&mut lines, task, run);
    }

    if !task.dependencies.is_empty() {
        append_bullet_section(
            &mut lines,
            "Dependencies",
            &format_task_reference_list(&task.dependencies, &tasks_by_id),
            crate::theme::status::INFO,
        );
    }

    if !task.blocked_by.is_empty() {
        append_bullet_section(
            &mut lines,
            "Blocked By",
            &format_task_reference_list(&task.blocked_by, &tasks_by_id),
            crate::theme::detail::BLOCKED_BY,
        );
    }

    if task.review_status.is_some()
        || task.review_panel_id.is_some()
        || task.review_panel_lease_state.is_some()
    {
        append_section_heading(&mut lines, "Review Context", crate::theme::role::DIRECTOR);
        if task.review_status.is_some() || task.review_round.is_some() || task.review_id.is_some() {
            let mut review_spans = vec![Span::styled("  ", Style::default())];
            if let Some(ref review_status) = task.review_status {
                review_spans.push(Span::styled(
                    "Status ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ));
                review_spans.push(Span::styled(
                    review_status.clone(),
                    task_status_style(review_status),
                ));
            }
            if let Some(review_round) = task.review_round {
                review_spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
                ));
                review_spans.push(Span::styled(
                    "Round ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ));
                review_spans.push(Span::styled(
                    review_round.to_string(),
                    Style::default().fg(Color::White),
                ));
            }
            if let Some(ref review_id) = task.review_id {
                review_spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
                ));
                review_spans.push(Span::styled(
                    "ID ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ));
                review_spans.push(Span::styled(
                    review_id.clone(),
                    Style::default().fg(Color::White),
                ));
            }
            lines.push(Line::from(review_spans));
        }

        if task.review_panel_id.is_some() || task.review_panel_lease_state.is_some() {
            let mut panel_spans = vec![Span::styled("  ", Style::default())];
            if let Some(ref panel_id) = task.review_panel_id {
                panel_spans.push(Span::styled(
                    "Panel ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ));
                panel_spans.push(Span::styled(
                    panel_id.clone(),
                    Style::default().fg(Color::White),
                ));
            }
            if let Some(ref lease_state) = task.review_panel_lease_state {
                panel_spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
                ));
                panel_spans.push(Span::styled(
                    "Lease ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ));
                panel_spans.push(Span::styled(
                    lease_state.clone(),
                    Style::default().fg(Color::White),
                ));
            }
            lines.push(Line::from(panel_spans));
        }

        if !task.review_panel_members.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Members ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(
                    task.review_panel_members.join(", "),
                    Style::default().fg(Color::White),
                ),
            ]));
        }
    }

    if !task.research_context.is_empty() {
        append_section_heading(&mut lines, "Research Context", crate::theme::status::INFO);
        for entry in &task.research_context {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(entry.artifact_id.clone(), Style::default().fg(DASH_ACCENT)),
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{} ", entry.role),
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(entry.title.clone(), Style::default().fg(Color::White)),
            ]));
            if !entry.summary.trim().is_empty() {
                append_text_block(&mut lines, &format!("  {}", entry.summary.trim()));
            }
            if let Some(path) = entry
                .artifact_path
                .as_ref()
                .filter(|value| !value.is_empty())
            {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(
                        "Artifact ",
                        Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                    ),
                    Span::styled(path.clone(), Style::default().fg(TEXT_DIM)),
                ]));
            }
        }
    }

    if !task.description.trim().is_empty() {
        append_section_heading(&mut lines, "Brief", DASH_ACCENT);
        append_text_block(&mut lines, &task.description);
    }

    if !task.acceptance_criteria.is_empty()
        && !description_mentions_heading(&task.description, &["acceptance criteria"])
    {
        append_bullet_section(
            &mut lines,
            "Acceptance Criteria",
            &task.acceptance_criteria,
            crate::theme::status::APPROVED,
        );
    }
    if !task.file_hints.is_empty()
        && !description_mentions_heading(&task.description, &["file hints"])
    {
        append_bullet_section(&mut lines, "File Hints", &task.file_hints, DASH_ACCENT);
    }
    if !task.plan_steps.is_empty() && !description_mentions_heading(&task.description, &["plan"]) {
        append_bullet_section(
            &mut lines,
            "Plan",
            &task.plan_steps,
            crate::theme::status::RUNNING,
        );
    }
    if !task.constraints.is_empty()
        && !description_mentions_heading(&task.description, &["constraints"])
    {
        append_bullet_section(
            &mut lines,
            "Constraints",
            &task.constraints,
            crate::theme::detail::CONSTRAINTS,
        );
    }
    if !task.test_requirements.is_empty()
        && !description_mentions_heading(&task.description, &["test requirements", "test plan"])
    {
        append_bullet_section(
            &mut lines,
            "Test Requirements",
            &task.test_requirements,
            crate::theme::role::DIRECTOR,
        );
    }
    if let Some(ref notes) = task.implementation_notes {
        if !notes.trim().is_empty()
            && !description_mentions_heading(&task.description, &["implementation notes"])
        {
            append_section_heading(
                &mut lines,
                "Implementation Notes",
                crate::theme::status::RUNNING,
            );
            append_text_block(&mut lines, notes);
        }
    }

    if let Some(ref activity) = task.activity {
        append_section_heading(
            &mut lines,
            "Current Activity",
            crate::theme::status::RUNNING,
        );
        append_text_block(&mut lines, activity);
    }
    if let Some(ref blockers) = task.blockers {
        append_section_heading(&mut lines, "Blockers", crate::theme::status::CONFLICT);
        append_text_block(&mut lines, blockers);
    }
    if task.integration_conflict_owner.as_deref() == Some("supervisor") {
        append_section_heading(
            &mut lines,
            "Integration Conflict",
            crate::theme::status::CONFLICT,
        );
        let mut conflict_meta = vec![Span::styled("  ", Style::default())];
        conflict_meta.push(Span::styled(
            "Owner ",
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ));
        conflict_meta.push(Span::styled(
            task.integration_conflict_owner
                .clone()
                .unwrap_or_else(|| "supervisor".to_string()),
            Style::default().fg(Color::White),
        ));
        if let Some(ref source) = task.integration_conflict_source {
            conflict_meta.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
            conflict_meta.push(Span::styled(
                "Source ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            conflict_meta.push(Span::styled(
                source.clone(),
                Style::default().fg(Color::White),
            ));
        }
        lines.push(Line::from(conflict_meta));
        if let Some(ref merge_target) = task.integration_conflict_merge_target {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Merge Target ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(merge_target.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if let Some(ref reviewed_commit) = task.integration_conflict_reviewed_commit {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Reviewed Commit ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(reviewed_commit.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if let Some(ref previous_worker) = task.integration_conflict_previous_worker {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Previous Worker ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(previous_worker.clone(), Style::default().fg(Color::White)),
            ]));
        }
        append_bullet_section(
            &mut lines,
            "Conflicting Files",
            &task.integration_conflict_conflicting_files,
            crate::theme::detail::CONSTRAINTS,
        );
    }
    if let Some(ref notes) = task.notes {
        append_section_heading(
            &mut lines,
            "Latest Notes",
            crate::theme::detail::MUTED_ACCENT,
        );
        append_text_block(&mut lines, notes);
    }

    if task.review_feedback_outcome.is_some()
        || !task.review_feedback_blocking.is_empty()
        || !task.review_feedback_suggestions.is_empty()
        || !task.review_feedback_dissent.is_empty()
    {
        append_section_heading(
            &mut lines,
            "Review Feedback",
            crate::theme::detail::REVIEW_FEEDBACK,
        );
        let mut feedback_spans = vec![Span::styled("  ", Style::default())];
        let mut has_feedback = false;
        if let Some(ref outcome) = task.review_feedback_outcome {
            has_feedback = true;
            feedback_spans.push(Span::styled(
                "Outcome ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            feedback_spans.push(Span::styled(outcome.clone(), task_status_style(outcome)));
        }
        if let Some(ref reason) = task.review_feedback_threshold_reason {
            if has_feedback {
                feedback_spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
                ));
            }
            has_feedback = true;
            feedback_spans.push(Span::styled(
                "Reason ",
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ));
            feedback_spans.push(Span::styled(
                reason.clone(),
                Style::default().fg(Color::White),
            ));
        }
        if has_feedback {
            lines.push(Line::from(feedback_spans));
        }
        if let Some(ref evaluated_at) = task.review_feedback_evaluated_at {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Evaluated ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(evaluated_at.clone(), Style::default().fg(Color::White)),
            ]));
        }

        append_bullet_section(
            &mut lines,
            "Blocking Findings",
            &task.review_feedback_blocking,
            crate::theme::detail::FINDING_BLOCKING,
        );
        append_bullet_section(
            &mut lines,
            "Suggestions",
            &task.review_feedback_suggestions,
            crate::theme::detail::FINDING_SUGGESTION,
        );
        append_bullet_section(
            &mut lines,
            "Nitpicks",
            &task.review_feedback_nitpicks,
            crate::theme::detail::FINDING_NITPICK,
        );
        append_bullet_section(
            &mut lines,
            "Dissent",
            &task.review_feedback_dissent,
            crate::theme::detail::FINDING_DISSENT,
        );
    }

    if let Some(ref proof) = task.proof {
        super::proof_detail::append_proof_section(&mut lines, proof);
    }

    if let Some(ref feedback) = task.feedback {
        super::feedback_detail::append_feedback_section(&mut lines, feedback);
    }

    let mut audit_spans: Vec<Span> = vec![Span::styled("  ", Style::default())];
    let mut has_audit = false;
    if let Some(ref created_at) = task.created_at {
        has_audit = true;
        audit_spans.push(Span::styled(
            "Created ",
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ));
        audit_spans.push(Span::styled(
            created_at.clone(),
            Style::default().fg(Color::White),
        ));
    }
    if let Some(ref updated_at) = task.updated_at {
        if has_audit {
            audit_spans.push(Span::styled(
                "  │  ",
                Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
            ));
        }
        has_audit = true;
        audit_spans.push(Span::styled(
            "Updated ",
            Style::default().fg(crate::theme::chrome::TEXT_LABEL),
        ));
        audit_spans.push(Span::styled(
            updated_at.clone(),
            Style::default().fg(Color::White),
        ));
    }
    if has_audit {
        append_section_heading(&mut lines, "Timing", crate::theme::detail::FINDING_NITPICK);
        lines.push(Line::from(audit_spans));
    }

    if task.closed_at.is_some() || task.closed_by.is_some() || task.merged_branch.is_some() {
        append_section_heading(&mut lines, "Terminal State", crate::theme::status::APPROVED);
        if let Some(ref closed_by) = task.closed_by {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Closed by  ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(closed_by.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if let Some(ref closed_at) = task.closed_at {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Closed at  ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(closed_at.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if let Some(ref merged_branch) = task.merged_branch {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Branch     ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(merged_branch.clone(), Style::default().fg(Color::White)),
            ]));
        }
        if let Some(ref merged_commit) = task.merged_commit {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Commit     ",
                    Style::default().fg(crate::theme::chrome::TEXT_LABEL),
                ),
                Span::styled(
                    merged_commit.clone(),
                    Style::default().fg(crate::theme::detail::MUTED_ACCENT),
                ),
            ]));
        }
    }

    lines
}

pub(crate) fn render_task_detail_dialog(
    frame: &mut Frame,
    area: Rect,
    dashboard: &DashboardData,
    state: &mut TaskDetailState,
) {
    let dialog_area = centered_dialog_rect(area, 80, 85, 120, 50);
    let matte_area = expand_rect(dialog_area, area, 2, 1);
    state.area = dialog_area;
    frame.render_widget(Clear, matte_area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(crate::theme::chrome::PANEL_MATTE_BORDER))
            .style(Style::default().bg(crate::theme::chrome::PANEL_MATTE_BG)),
        matte_area,
    );

    let Some(task) = dashboard.tasks.iter().find(|task| task.id == state.task_id) else {
        let title = format!("Task {}", state.task_id);
        let inner = inset_rect(
            Panel::new(&title)
                .accent(DASH_ACCENT)
                .border(crate::theme::chrome::PANEL_BORDER_ELEVATED)
                .bg(crate::theme::chrome::PANEL_BG_ELEVATED)
                .render(frame, dialog_area),
            1,
            1,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Task is no longer visible on the dashboard."),
                Line::from("It may have closed or been removed from the current runtime."),
                Line::from(""),
                Line::from("Press Esc or click outside this dialog to close."),
            ])
            .style(Style::default().bg(crate::theme::chrome::PANEL_BG_ELEVATED))
            .wrap(Wrap { trim: false }),
            inner,
        );
        state.max_scroll = 0;
        state.scroll = 0;
        return;
    };

    // Truncate the title for the border to prevent overflow
    let max_title_width = dialog_area.width.saturating_sub(6) as usize; // border + padding
    let title_text = task.title.clone();
    let display_title = if title_text.chars().count() > max_title_width {
        let mut t = String::with_capacity(max_title_width);
        t.extend(title_text.chars().take(max_title_width.saturating_sub(1)));
        t.push('\u{2026}');
        t
    } else {
        title_text
    };

    let inner = inset_rect(
        Panel::new(&display_title)
            .accent(Color::White)
            .border(crate::theme::chrome::PANEL_BORDER)
            .bg(crate::theme::chrome::PANEL_BG)
            .render(frame, dialog_area),
        1,
        0,
    );

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    let lines = build_task_detail_lines(task, dashboard);
    let content_area = chunks[0];
    let footer_area = chunks[1];
    let max_scroll = lines.len().saturating_sub(content_area.height as usize) as u16;
    state.max_scroll = max_scroll;
    state.scroll = state.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(crate::theme::chrome::PANEL_BG))
            .scroll((state.scroll, 0))
            .wrap(Wrap { trim: false }),
        content_area,
    );

    // ── Footer with scroll indicator ────────────────────────────────
    let scroll_indicator = if max_scroll > 0 {
        let pct = if max_scroll > 0 {
            ((state.scroll as f32 / max_scroll as f32) * 100.0) as u16
        } else {
            0
        };
        format!(" {pct}% ")
    } else {
        String::new()
    };

    let rule_width = footer_area.width as usize;
    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            "\u{2500}".repeat(rule_width),
            Style::default().fg(crate::theme::chrome::FOOTER_RULE),
        )),
        Line::from(vec![
            Span::styled(
                " Esc ",
                Style::default()
                    .fg(crate::theme::chrome::PANEL_BORDER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "close  ",
                Style::default().fg(crate::theme::chrome::FOOTER_LABEL),
            ),
            Span::styled(
                " \u{2191}\u{2193} ",
                Style::default()
                    .fg(crate::theme::chrome::PANEL_BORDER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "scroll  ",
                Style::default().fg(crate::theme::chrome::FOOTER_LABEL),
            ),
            Span::styled(
                scroll_indicator,
                Style::default().fg(crate::theme::chrome::TEXT_LABEL),
            ),
        ]),
    ])
    .style(Style::default().bg(crate::theme::chrome::PANEL_BG));
    frame.render_widget(footer, footer_area);
}

pub(crate) fn handle_task_detail_mouse_event(
    mouse: MouseEvent,
    task_detail: &mut Option<TaskDetailState>,
    selection: &mut Option<SelectionState>,
    pending_down: &mut Option<(SelectionPane, String, PanePos)>,
) -> bool {
    let Some(detail) = task_detail.as_mut() else {
        return false;
    };
    let pos = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !detail.area.contains(pos) {
                *task_detail = None;
                *selection = None;
                *pending_down = None;
            }
            true
        }
        MouseEventKind::ScrollUp => {
            if detail.area.contains(pos) {
                detail.scroll = detail.scroll.saturating_sub(3);
                true
            } else {
                false
            }
        }
        MouseEventKind::ScrollDown => {
            if detail.area.contains(pos) {
                detail.scroll = (detail.scroll + 3).min(detail.max_scroll);
                true
            } else {
                false
            }
        }
        MouseEventKind::Up(MouseButton::Left)
        | MouseEventKind::Drag(MouseButton::Left)
        | MouseEventKind::Moved => detail.area.contains(pos),
        _ => false,
    }
}
