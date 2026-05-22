//! Pane rendering, text helpers, and status bar.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::ghostty_widget::{PaneRenderCache, PaneViewport};
use panesmith::{TerminalPaneWidget, TerminalViewport};

// Thread-local map of pane-id → render cache. The TUI render path is
// single-threaded (ratatui draws from one thread), so a thread-local
// keeps the F7 generation-cache opt-in without rewriting every signature
// in the `render_pane_in_area*` test surface. The cache is purely an
// optimisation — dropping or resetting it never changes what gets
// rendered, only how many `dump_row`/`row_styles` FFI calls happen.
std::thread_local! {
    static PANE_RENDER_CACHES: std::cell::RefCell<HashMap<String, PaneRenderCache>>
        = std::cell::RefCell::new(HashMap::new());
}

use crate::components::Panel;
use ratatui::Frame;

use crate::theme::chrome::{BG, BORDER, TEXT_DIM, TEXT_MUTED};
use crate::theme::{status_style, StatusKind};
use brehon_mux::{
    ActivityBuffer, ActivityEntry, ActivityKind, DeathReason, Mux, PaneBackendOwnership, PaneKind,
    PaneState, TaskBlockedReason, TaskContextSnapshot,
};
use brehon_types::{task::TaskStatus, RuntimePaneState, RuntimeSource};

use super::dashboard::{
    runtime_terminal_host_attach_command, RuntimeDaemonDashboardStatus, RuntimePaneDashboardInfo,
};
use super::types::*;

pub(crate) const SUPERVISOR_IDLE_INDICATOR_THRESHOLD: Duration = Duration::from_secs(30);
const COLLAPSED_ACTIVITY_TEXT_LINES: usize = 2;
const EXPANDED_ACTIVITY_TEXT_LINES: usize = 24;

fn pane_configured_identity(pane: &brehon_mux::Pane) -> Option<&str> {
    let cli_name = pane.cli_type().name();
    pane.configured_agent_type()
        .filter(|agent_type| *agent_type != cli_name)
}

fn pane_identity_label(pane: &brehon_mux::Pane) -> &str {
    pane_configured_identity(pane).unwrap_or(pane.cli_type().name())
}

fn pane_model_label(pane: &brehon_mux::Pane) -> &str {
    pane.configured_agent_type()
        .unwrap_or(pane.cli_type().name())
}

fn pane_title_line(pane: &brehon_mux::Pane) -> Line<'static> {
    let mut spans = Vec::new();
    spans.push(Span::raw(" "));
    spans.extend(
        crate::theme::brand::gradient(
            crate::theme::brand::PRIMARY_RGB,
            crate::theme::brand::SECONDARY_RGB,
            "BREHON",
        )
        .spans,
    );
    spans.push(Span::styled(" / ", Style::default().fg(TEXT_DIM)));
    spans.push(Span::styled(
        format!("{} ", crate::theme::role::glyph(pane.kind())),
        Style::default()
            .fg(crate::theme::role::color(pane.kind()))
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        pane.id().to_string(),
        Style::default()
            .fg(crate::theme::agent::color(pane.cli_type().name()))
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Render a pane's PTY viewport using the unified ghostty_vt → ratatui
/// widget. Replaces the previous two render paths (Paragraph for
/// worker/reviewer, tui_term::PseudoTerminal for supervisor).
///
/// Uses a thread-local `PaneRenderCache` so the widget can skip the
/// per-row FFI round-trip when the pane's `render_generation` hasn't
/// advanced since the last frame — see `PaneRenderCache` for details.
fn render_pane_viewport(frame: &mut Frame, inner: Rect, pane: &brehon_mux::Pane, focused: bool) {
    let base = pane_content_style(pane);
    let mut widget = PaneViewport::new(pane, base);
    if focused && pane.accepts_manual_input() && pane.display_scroll_offset() == 0 {
        let cursor_style = Style::default()
            .fg(Color::Black)
            .bg(crate::theme::agent::color(pane.cli_type().name()));
        widget = widget.with_cursor(cursor_style);
    }
    PANE_RENDER_CACHES.with(|caches| {
        let mut caches = caches.borrow_mut();
        let cache = caches
            .entry(pane.id().to_string())
            .or_insert_with(PaneRenderCache::new);
        frame.render_stateful_widget(widget, inner, cache);
    });
}

fn render_terminal_viewport(
    frame: &mut Frame,
    inner: Rect,
    mux: &Mux,
    pane_id: &str,
    pane: &brehon_mux::Pane,
    focused: bool,
    structured_scroll_offset: Option<usize>,
) {
    if let Some(snapshot) = mux.panesmith_snapshot(pane_id) {
        let scroll_offset = structured_scroll_offset.unwrap_or_default();
        let mut widget = TerminalPaneWidget::new(snapshot)
            .focused(focused && pane.accepts_manual_input() && scroll_offset == 0);
        if let Some(scrollback) = mux.panesmith_scrollback(pane_id) {
            widget = widget.with_scrollback(scrollback);
        }
        if scroll_offset > 0 {
            widget = widget.with_viewport(TerminalViewport::scrolled(scroll_offset));
        }
        frame.render_widget(widget, inner);
        return;
    }

    render_pane_viewport(frame, inner, pane, focused);
}

fn pane_right_label(
    pane: &brehon_mux::Pane,
    backend_ownership: Option<PaneBackendOwnership>,
) -> String {
    let mut label = format!("{} / {}", pane.cli_type().name(), pane_model_label(pane));
    if backend_ownership == Some(PaneBackendOwnership::Panesmith) {
        label.push_str(" / ");
        label.push_str(PaneBackendOwnership::Panesmith.label());
    }
    label
}

fn pane_footer_text(
    pane: &brehon_mux::Pane,
    activity_buffer: Option<&ActivityBuffer>,
    viewport_scroll_offset: Option<usize>,
) -> String {
    if let Some(PaneState::Dead { reason, .. }) = pane.pane_state() {
        return format!("exited with error: {}", death_reason_summary(reason));
    }

    let mut parts = Vec::new();
    let mut state = "idle".to_string();

    match pane.pane_state() {
        Some(PaneState::Dead { .. }) => state = "error".to_string(),
        Some(PaneState::Busy { .. }) => state = "running".to_string(),
        _ => {}
    }

    if let Some(activity_buffer) = activity_buffer {
        if let Some(active) = activity_buffer.active_tools().next() {
            state = "running".to_string();
            let badge = crate::theme::elapsed_badge(active.started_at);
            parts.push(active.tool_name.clone());
            parts.push(badge.content.into_owned());
        } else if let Some(operation) = activity_buffer.active_operation() {
            state = "running".to_string();
            if !is_low_signal_activity_operation(operation) {
                parts.push(truncate_to(operation, 32));
            }
        } else if let Some(last) = activity_buffer.last_entry_or_pending() {
            if matches!(last.kind, ActivityKind::ToolCall)
                && matches!(last.status.as_deref(), Some("failed" | "error"))
            {
                state = "error".to_string();
                if let Some(tool_name) = last.tool_name.as_deref() {
                    parts.push(tool_name.to_string());
                }
            }
        }
    }

    let mut footer = vec![state];
    footer.extend(parts);
    if pane.display_scroll_offset() > 0 || viewport_scroll_offset.unwrap_or_default() > 0 {
        footer.push("scrolled".to_string());
    }
    footer.join(" • ")
}

fn death_reason_summary(reason: &DeathReason) -> &str {
    match reason {
        DeathReason::Quarantined(reason) | DeathReason::SpawnFailed(reason) => reason.as_str(),
        DeathReason::MaxTurnExceeded => "max turn duration exceeded",
        DeathReason::SessionDropped => "session dropped",
        DeathReason::TransportClosed => "transport closed",
    }
}

pub(crate) fn apply_entry_chrome_fade(frame: &mut Frame) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            let cell = &mut buf[Position::new(x, y)];
            if cell.fg == crate::theme::chrome::TEXT {
                cell.modifier |= Modifier::DIM;
            }
        }
    }
}

fn pane_content_style(pane: &brehon_mux::Pane) -> Style {
    let mut style = Style::default();
    if matches!(pane.pane_state(), Some(PaneState::Dead { .. })) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn pane_footer_style(pane: &brehon_mux::Pane) -> Option<Style> {
    if matches!(pane.pane_state(), Some(PaneState::Dead { .. })) {
        Some(Style::default().fg(crate::theme::status::ERROR))
    } else {
        None
    }
}

fn render_pane_panel(
    frame: &mut Frame,
    area: Rect,
    pane: &brehon_mux::Pane,
    focused: bool,
    footer_text: &str,
    right_label_padding: usize,
    backend_ownership: Option<PaneBackendOwnership>,
) -> Rect {
    let cli_name = pane.cli_type().name();
    let scrolled = footer_text.contains("scrolled");
    let mut right_label = pane_right_label(pane, backend_ownership);
    if right_label_padding > 0 {
        right_label.push_str(&" ".repeat(right_label_padding));
    }

    let mut panel = Panel::new("")
        .title_line(pane_title_line(pane))
        .right_label(right_label)
        .footer_label(footer_text)
        .focused(focused)
        .accent(if scrolled { Color::Yellow } else { BORDER })
        .focused_accent(if scrolled {
            Color::Yellow
        } else {
            crate::theme::agent::color(cli_name)
        })
        .focused_border(crate::theme::agent::color(cli_name));

    if let Some(style) = pane_footer_style(pane) {
        panel = panel.footer_style(style);
    }

    panel.render(frame, area)
}

fn render_missing_pane_placeholder(frame: &mut Frame, area: Rect, pane_id: &str, focused: bool) {
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "BREHON",
            Style::default()
                .fg(crate::theme::brand::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            pane_id.to_string(),
            Style::default()
                .fg(crate::theme::status::WARNING)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    let inner = Panel::new("")
        .title_line(title)
        .right_label("missing pane")
        .footer_label("not present in this run")
        .focused(focused)
        .accent(crate::theme::status::WARNING)
        .focused_accent(crate::theme::status::WARNING)
        .focused_border(crate::theme::status::WARNING)
        .render(frame, area);

    let lines = vec![
        Line::from(Span::styled(
            format!("Pane '{pane_id}' is not present in this Brehon run."),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "The review panel lease references stale runtime state. Restarting or reseating the panel should reconcile it.",
            Style::default().fg(TEXT_DIM),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Truncate a string to fit in `max_chars` columns, appending `…` when
/// the string is too long.  Uses a single Unicode ellipsis character to
/// waste minimal space in table columns.
pub(crate) fn truncate_to(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if value.width() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out = String::new();
    let mut used_width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used_width + ch_width > keep {
            break;
        }
        out.push(ch);
        used_width += ch_width;
    }
    out.push('…');
    out
}

pub(crate) fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.width() <= max_chars {
        return value.to_string();
    }

    let keep = max_chars.saturating_sub(3);
    let mut out = String::new();
    let mut used_width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used_width + ch_width > keep {
            break;
        }
        out.push(ch);
        used_width += ch_width;
    }
    out.push_str("...");
    out
}

pub(crate) fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&'[') = chars.peek() {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_lowercase() || next.is_ascii_uppercase() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

pub(crate) fn render_task_context_lines(
    task_context: &TaskContextSnapshot,
    inner_width: usize,
) -> Vec<Line<'static>> {
    let completion_mode = task_context.completion_mode.as_deref().unwrap_or("unknown");
    let merge_target = task_context.merge_target.as_deref().unwrap_or("unknown");
    let epic_branch = task_context.epic_branch.as_deref().unwrap_or("unknown");
    let epic_worktree = task_context
        .epic_worktree
        .as_ref()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let blocked_reason = format_blocked_reason(task_context.blocked_reason.as_ref());

    let lines = vec![
        Line::from(vec![
            Span::styled(" task ", Style::default().fg(crate::theme::status::INFO)),
            Span::styled(
                format!(
                    "{} | {} | mode:{}",
                    task_context.task_id,
                    format_task_status(task_context.status),
                    completion_mode
                ),
                Style::default().fg(crate::theme::chrome::TEXT_SOFT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" title ", Style::default().fg(crate::theme::status::INFO)),
            Span::styled(
                task_context.title.clone(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled(" merge ", Style::default().fg(crate::theme::status::INFO)),
            Span::styled(
                format!("{merge_target} | epic:{epic_branch}"),
                Style::default().fg(crate::theme::chrome::TEXT_SOFT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                " worktree ",
                Style::default().fg(crate::theme::status::INFO),
            ),
            Span::styled(
                epic_worktree,
                Style::default().fg(crate::theme::chrome::TEXT_PATH),
            ),
        ]),
        Line::from(vec![
            Span::styled(" blocked ", Style::default().fg(crate::theme::status::INFO)),
            Span::styled(
                blocked_reason,
                Style::default().fg(crate::theme::status::BLOCKED),
            ),
        ]),
        Line::from(Span::styled(
            "─".repeat(inner_width.max(1)),
            Style::default().fg(crate::theme::chrome::RULE_STRONG),
        )),
    ];
    lines
}

pub(crate) fn format_task_status(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Assigned => "assigned",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::InReview => "in_review",
        TaskStatus::ChangesRequested => "changes_requested",
        TaskStatus::Approved => "approved",
        TaskStatus::Merged => "merged",
        TaskStatus::Blocked => "blocked",
    }
}

pub(crate) fn format_blocked_reason(reason: Option<&TaskBlockedReason>) -> String {
    let Some(reason) = reason else {
        return "unknown".to_string();
    };
    let summary = reason
        .summary
        .as_ref()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty());
    match (reason.blocker_task_id.as_deref(), summary) {
        (Some(task_id), Some(summary)) => format!("{task_id}: {summary}"),
        (Some(task_id), None) => task_id.to_string(),
        (None, Some(summary)) => summary.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

fn tool_status_kind(status: &str) -> StatusKind {
    match status {
        "started" => StatusKind::Running,
        "completed" | "success" => StatusKind::Success,
        "failed" | "error" => StatusKind::Error,
        _ => StatusKind::Idle,
    }
}

fn activity_kind_code(kind: ActivityKind) -> u8 {
    match kind {
        ActivityKind::Operation => 0,
        ActivityKind::Permission => 1,
        ActivityKind::Progress => 2,
        ActivityKind::ToolCall => 3,
        ActivityKind::Output => 4,
    }
}

fn activity_operation_display_message(entry: &ActivityEntry) -> Option<String> {
    let message = entry.message.as_deref()?;
    if is_low_signal_activity_operation(message) {
        return if matches!(
            entry.status.as_deref(),
            Some("failed" | "error" | "cancelled")
        ) {
            Some(format!(
                "{} {}",
                human_activity_operation(message),
                entry.status.as_deref().unwrap_or("failed")
            ))
        } else {
            None
        };
    }

    Some(message.to_string())
}

fn human_activity_operation(operation: &str) -> &str {
    if is_turn_activity_operation(operation) {
        "response"
    } else {
        operation
    }
}

fn is_low_signal_activity_operation(operation: &str) -> bool {
    let normalized = operation.trim().to_ascii_lowercase();
    normalized == "response" || normalized == "step" || is_turn_activity_operation(operation)
}

fn is_turn_activity_operation(operation: &str) -> bool {
    let normalized = operation.trim().to_ascii_lowercase();
    normalized == "turn" || normalized.ends_with(" turn")
}

fn activity_entry_key(entry: &ActivityEntry, index: usize) -> String {
    if let Some(tool_id) = entry.tool_id.as_deref() {
        return format!(
            "tool:{}:{}",
            tool_id,
            entry.status.as_deref().unwrap_or_default()
        );
    }

    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    activity_kind_code(entry.kind).hash(&mut hasher);
    entry.tool_name.hash(&mut hasher);
    entry.status.hash(&mut hasher);
    entry.message.hash(&mut hasher);
    if let Some(chunks) = entry.output_chunks.as_ref() {
        for chunk in chunks {
            chunk.hash(&mut hasher);
        }
    }
    format!("entry:{index}:{:016x}", hasher.finish())
}

#[derive(Clone)]
struct ActivityDisplayLine {
    line: Line<'static>,
    key: Option<String>,
}

fn plain_activity_line(line: Line<'static>) -> ActivityDisplayLine {
    ActivityDisplayLine { line, key: None }
}

fn keyed_activity_line(line: Line<'static>, key: &str) -> ActivityDisplayLine {
    ActivityDisplayLine {
        line,
        key: Some(key.to_string()),
    }
}

fn activity_line(line: Line<'static>, key: &str, expandable: bool) -> ActivityDisplayLine {
    if expandable {
        keyed_activity_line(line, key)
    } else {
        plain_activity_line(line)
    }
}

fn wrap_plain_text(value: &str, width: usize, max_lines: usize) -> (Vec<String>, bool) {
    if max_lines == 0 {
        return (Vec::new(), !value.trim().is_empty());
    }
    if width == 0 {
        return (Vec::new(), !value.trim().is_empty());
    }

    let mut lines = Vec::new();
    let mut source_lines = value.lines().peekable();
    while let Some(source_line) = source_lines.next() {
        if source_line.is_empty() {
            if lines.len() == max_lines {
                return (lines, true);
            }
            lines.push(String::new());
            if lines.len() == max_lines {
                return (lines, source_lines.any(|line| !line.trim().is_empty()));
            }
            continue;
        }

        let mut current = String::new();
        let mut used_width = 0usize;
        for ch in source_line.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if used_width + ch_width > width && !current.is_empty() {
                lines.push(current);
                if lines.len() == max_lines {
                    return (lines, true);
                }
                current = String::new();
                used_width = 0;
            }
            current.push(ch);
            used_width += ch_width;
        }
        if !current.is_empty() {
            lines.push(current);
            if lines.len() == max_lines {
                return (lines, source_lines.any(|line| !line.trim().is_empty()));
            }
        }
    }

    (lines, false)
}

fn push_wrapped_activity_text(
    lines: &mut Vec<ActivityDisplayLine>,
    key: &str,
    prefix: &str,
    continuation_prefix: &str,
    text: &str,
    area_width: usize,
    max_lines: usize,
    text_style: Style,
) {
    let text_width = area_width
        .saturating_sub(continuation_prefix.width().max(prefix.width()))
        .max(1);
    let (wrapped, truncated) = wrap_plain_text(text, text_width, max_lines);
    if wrapped.is_empty() {
        lines.push(keyed_activity_line(
            Line::from(vec![
                Span::styled(prefix.to_string(), Style::default().fg(BORDER)),
                Span::styled("", text_style),
            ]),
            key,
        ));
        return;
    }

    for (idx, wrapped_line) in wrapped.into_iter().enumerate() {
        let line_prefix = if idx == 0 {
            prefix
        } else {
            continuation_prefix
        };
        lines.push(keyed_activity_line(
            Line::from(vec![
                Span::styled(line_prefix.to_string(), Style::default().fg(BORDER)),
                Span::styled(wrapped_line, text_style),
            ]),
            key,
        ));
    }

    if truncated {
        lines.push(keyed_activity_line(
            Line::from(vec![
                Span::styled(continuation_prefix.to_string(), Style::default().fg(BORDER)),
                Span::styled("…".to_string(), Style::default().fg(TEXT_MUTED)),
            ]),
            key,
        ));
    }
}

fn activity_visible_range(
    total_lines: usize,
    viewport_height: usize,
    scroll_back: usize,
) -> (usize, usize) {
    if viewport_height == 0 {
        return (0, 0);
    }
    let max_scroll = total_lines.saturating_sub(viewport_height);
    let scroll_back = scroll_back.min(max_scroll);
    let start = max_scroll.saturating_sub(scroll_back);
    let end = start.saturating_add(viewport_height).min(total_lines);
    (start, end)
}

pub(crate) fn format_duration(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{}ms", millis)
    } else {
        let secs = duration.as_secs();
        let ms = duration.subsec_millis();
        if secs < 60 {
            format!("{}.{}s", secs, ms / 100)
        } else {
            let mins = secs / 60;
            let remaining_secs = secs % 60;
            format!("{}m{}s", mins, remaining_secs)
        }
    }
}

#[allow(dead_code)]
pub(crate) fn render_structured_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &brehon_mux::Pane,
    activity_buffer: &ActivityBuffer,
    focused: bool,
) {
    let expanded_activity_rows = HashSet::new();
    render_structured_pane_with_padding(
        frame,
        area,
        pane,
        activity_buffer,
        focused,
        0,
        &expanded_activity_rows,
        None,
        None,
    );
}

fn render_structured_pane_with_padding(
    frame: &mut Frame,
    area: Rect,
    pane: &brehon_mux::Pane,
    activity_buffer: &ActivityBuffer,
    focused: bool,
    right_label_padding: usize,
    expanded_activity_rows: &HashSet<(String, String)>,
    structured_scroll_offset: Option<usize>,
    activity_click_regions: Option<&mut Vec<ClickRegion>>,
) {
    let footer_text = pane_footer_text(pane, Some(activity_buffer), structured_scroll_offset);
    let inner = render_pane_panel(
        frame,
        area,
        pane,
        focused,
        &footer_text,
        right_label_padding,
        None,
    );

    // Iterate committed entries by reference (no full-buffer clone), then
    // append the optional pending output entry at the end.
    let pending = activity_buffer.pending_output_entry();

    // ── Pre-process: collapse consecutive completed tool calls ─────────────
    struct DisplayGroup {
        entry: ActivityEntry,
        count: usize,
        key: String,
    }

    let mut groups: Vec<DisplayGroup> = Vec::with_capacity(activity_buffer.len() + 1);
    for (index, entry) in activity_buffer.entries().chain(pending.iter()).enumerate() {
        let dominated = match entry.kind {
            ActivityKind::ToolCall
                if matches!(entry.status.as_deref(), Some("completed" | "success")) =>
            {
                if let Some(last) = groups.last_mut() {
                    last.entry.kind == ActivityKind::ToolCall
                        && matches!(last.entry.status.as_deref(), Some("completed" | "success"))
                        && last.entry.tool_name == entry.tool_name
                } else {
                    false
                }
            }
            _ => false,
        };

        if dominated {
            let last = groups.last_mut().unwrap();
            last.count += 1;
            // Accumulate duration from collapsed entries
            if let (Some(existing), Some(new_dur)) = (&last.entry.duration, &entry.duration) {
                last.entry.duration = Some(*existing + *new_dur);
            } else if last.entry.duration.is_none() {
                last.entry.duration = entry.duration;
            }
        } else {
            groups.push(DisplayGroup {
                entry: entry.clone(),
                count: 1,
                key: activity_entry_key(entry, index),
            });
        }
    }

    // ── Render display groups into lines ───────────────────────────────────
    let mut header_lines: Vec<Line<'static>> = Vec::new();
    let mut lines: Vec<ActivityDisplayLine> = Vec::new();
    let inner_width = inner.width as usize;
    if !matches!(pane.kind(), PaneKind::Reviewer) {
        if let Some(task_context) = pane.task_context() {
            let mut context_lines = render_task_context_lines(task_context, inner_width);
            header_lines.append(&mut context_lines);
        }
    }

    if matches!(pane.kind(), PaneKind::Reviewer) {
        if let Some(ctx) = pane.review_context() {
            header_lines.push(Line::from(Span::styled(
                " Review context",
                Style::default()
                    .fg(crate::theme::detail::ACTIVE_BADGE)
                    .add_modifier(Modifier::BOLD),
            )));
            header_lines.push(Line::from(Span::styled(
                format!(
                    "  review {}  task {}  round {}",
                    ctx.review_id, ctx.task_id, ctx.round
                ),
                Style::default().fg(TEXT_DIM),
            )));
            header_lines.push(Line::from(Span::styled(
                format!("  panel {}/{} complete", ctx.panel_done, ctx.panel_total),
                Style::default().fg(TEXT_DIM),
            )));

            let verdict_text = ctx.verdict.clone().unwrap_or_else(|| "pending".to_string());
            header_lines.push(Line::from(Span::styled(
                format!("  verdict {verdict_text}"),
                Style::default().fg(TEXT_DIM),
            )));

            let score_text = ctx
                .score
                .map(|score| score.to_string())
                .unwrap_or_else(|| "pending".to_string());
            header_lines.push(Line::from(Span::styled(
                format!("  score {score_text}"),
                Style::default().fg(TEXT_DIM),
            )));

            if let Some(summary) = &ctx.findings_summary {
                let summary = truncate_with_ellipsis(summary, REVIEW_FINDINGS_SUMMARY_MAX_CHARS);
                header_lines.push(Line::from(Span::styled(
                    format!("  findings {summary}"),
                    Style::default().fg(TEXT_DIM),
                )));
            }

            header_lines.push(Line::from(Span::styled(
                format!("  {}", "─".repeat(inner_width.saturating_sub(4))),
                Style::default().fg(BORDER),
            )));
        }
    }

    for group in &groups {
        let entry = &group.entry;
        let expandable = match entry.kind {
            ActivityKind::ToolCall => entry
                .message
                .as_deref()
                .is_some_and(|message| !message.trim().is_empty()),
            ActivityKind::Output | ActivityKind::Progress | ActivityKind::Permission => true,
            ActivityKind::Operation => activity_operation_display_message(entry).is_some(),
        };
        let expanded = expandable
            && expanded_activity_rows.contains(&(pane.id().to_string(), group.key.clone()));
        let marker = if expanded { "▾" } else { "▸" };
        match entry.kind {
            ActivityKind::ToolCall => {
                let tool_name = entry.tool_name.as_deref().unwrap_or("unknown");
                let tool_id = entry.tool_id.as_deref().unwrap_or("");
                let status = entry.status.as_deref().unwrap_or("");

                let icon = match status {
                    "started" => "⟳",
                    "completed" | "success" => "✓",
                    "failed" | "error" => "✗",
                    _ => "•",
                };

                let status_style = status_style(tool_status_kind(status));

                // Duration: show for running tools (live) and completed tools (stored)
                let duration_text = if status == "started" {
                    activity_buffer
                        .active_tool(tool_id)
                        .map(|t| {
                            format_duration(std::time::Instant::now().duration_since(t.started_at))
                        })
                        .unwrap_or_default()
                } else {
                    entry.duration.map(format_duration).unwrap_or_default()
                };

                let count_text = if group.count > 1 {
                    format!(" ×{}", group.count)
                } else {
                    String::new()
                };

                let marker = if expandable { marker } else { " " };
                let prefix = format!(" {marker}{icon} ");
                let mut summary = tool_name.to_string();
                summary.push_str(&count_text);
                if !duration_text.is_empty() {
                    summary.push_str("  ");
                    summary.push_str(&duration_text);
                }
                let summary =
                    truncate_to(&summary, inner_width.saturating_sub(prefix.width()).max(1));

                lines.push(activity_line(
                    Line::from(vec![
                        Span::styled(prefix, status_style),
                        Span::styled(
                            summary,
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    &group.key,
                    expandable,
                ));

                if expanded {
                    if let Some(detail) = entry.message.as_deref() {
                        push_wrapped_activity_text(
                            &mut lines,
                            &group.key,
                            "    ",
                            "    ",
                            detail,
                            inner_width,
                            EXPANDED_ACTIVITY_TEXT_LINES,
                            Style::default().fg(TEXT_MUTED),
                        );
                    }
                    if group.count > 1 {
                        lines.push(keyed_activity_line(
                            Line::from(vec![
                                Span::styled("    ".to_string(), Style::default().fg(BORDER)),
                                Span::styled(
                                    format!("collapsed {} calls", group.count),
                                    Style::default().fg(TEXT_MUTED),
                                ),
                            ]),
                            &group.key,
                        ));
                    }
                }
            }
            ActivityKind::Output => {
                if let Some(chunks) = &entry.output_chunks {
                    let mut output_lines = Vec::new();
                    for chunk in chunks {
                        let cleaned = strip_ansi_codes(chunk);
                        for text_line in cleaned.lines() {
                            if !text_line.trim().is_empty() {
                                // Section separators for review boundaries
                                let lower = text_line.to_ascii_lowercase();
                                if lower.contains("review submitted")
                                    || lower.contains("standing by")
                                {
                                    let sep_width = inner_width.saturating_sub(4);
                                    let separator = "─".repeat(sep_width);
                                    lines.push(plain_activity_line(Line::from(Span::styled(
                                        format!("  {}", separator),
                                        Style::default().fg(DASH_SECTION_BORDER),
                                    ))));
                                }
                                output_lines.push(text_line.to_string());
                            }
                        }
                    }
                    if !output_lines.is_empty() {
                        let max_lines = if expanded {
                            EXPANDED_ACTIVITY_TEXT_LINES
                        } else {
                            COLLAPSED_ACTIVITY_TEXT_LINES
                        };
                        let display_text = if expanded {
                            output_lines.join("\n")
                        } else {
                            let start = output_lines.len().saturating_sub(max_lines);
                            output_lines[start..].join("\n")
                        };
                        let prefix = if expanded { " ▾│ " } else { " ▸│ " };
                        push_wrapped_activity_text(
                            &mut lines,
                            &group.key,
                            prefix,
                            "  │ ",
                            &display_text,
                            inner_width,
                            max_lines,
                            Style::default().fg(Color::White),
                        );
                        lines.push(plain_activity_line(Line::from("")));
                    }
                }
            }
            ActivityKind::Progress => {
                if let Some(msg) = &entry.message {
                    let style = status_style(StatusKind::Info);
                    let prefix = format!(" {marker}◐ ");
                    push_wrapped_activity_text(
                        &mut lines,
                        &group.key,
                        &prefix,
                        "    ",
                        msg,
                        inner_width,
                        if expanded { 8 } else { 2 },
                        style,
                    );
                }
            }
            ActivityKind::Permission => {
                if let Some(msg) = &entry.message {
                    let style = status_style(StatusKind::Warning);
                    let prefix = format!(" {marker}⚠ ");
                    push_wrapped_activity_text(
                        &mut lines,
                        &group.key,
                        &prefix,
                        "    ",
                        msg,
                        inner_width,
                        if expanded { 8 } else { 2 },
                        style,
                    );
                }
            }
            ActivityKind::Operation => {
                if let Some(msg) = activity_operation_display_message(entry) {
                    let style = status_style(StatusKind::Idle);
                    let prefix = format!(" {marker}• ");
                    push_wrapped_activity_text(
                        &mut lines,
                        &group.key,
                        &prefix,
                        "    ",
                        &msg,
                        inner_width,
                        if expanded { 8 } else { 2 },
                        style,
                    );
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(plain_activity_line(Line::from(Span::styled(
            " No activity yet",
            Style::default().fg(TEXT_MUTED),
        ))));
    }

    let header_height = header_lines.len().min(inner.height as usize) as u16;
    if header_height > 0 {
        let header_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: header_height,
        };
        let visible_header: Vec<Line<'static>> = header_lines
            .into_iter()
            .take(header_height as usize)
            .collect();
        frame.render_widget(
            Paragraph::new(visible_header).style(pane_content_style(pane)),
            header_area,
        );
    }

    let body_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(header_height),
        width: inner.width,
        height: inner.height.saturating_sub(header_height),
    };

    if body_area.height == 0 {
        return;
    }

    // ── Auto-follow scroll (body only) ───────────────────────────────────
    let total_lines = lines.len();
    let inner_height = body_area.height as usize;
    let scroll_back =
        structured_scroll_offset.unwrap_or_else(|| pane.display_scroll_offset() as usize);
    let (start, end) = activity_visible_range(total_lines, inner_height, scroll_back);

    if let Some(regions) = activity_click_regions {
        for (visible_row, line) in lines[start..end].iter().enumerate() {
            if let Some(key) = line.key.as_ref() {
                regions.push(ClickRegion {
                    rect: Rect::new(
                        body_area.x,
                        body_area.y.saturating_add(visible_row as u16),
                        body_area.width,
                        1,
                    ),
                    target: ClickTarget::ActivityRow {
                        pane_id: pane.id().to_string(),
                        entry_key: key.clone(),
                    },
                });
            }
        }
    }

    let visible_lines: Vec<Line<'static>> = lines[start..end]
        .iter()
        .map(|line| line.line.clone())
        .collect();

    frame.render_widget(
        Paragraph::new(visible_lines).style(pane_content_style(pane)),
        body_area,
    );
}

#[allow(dead_code)]
pub(crate) fn render_pane_in_area(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    pane_id: &str,
    focused: bool,
    selection: Option<&SelectionState>,
    structured_mode: bool,
) -> Option<Rect> {
    let expanded_activity_rows = HashSet::new();
    render_pane_in_area_with_activity_regions(
        frame,
        area,
        mux,
        pane_id,
        focused,
        selection,
        structured_mode,
        &expanded_activity_rows,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane_in_area_with_activity_regions(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    pane_id: &str,
    focused: bool,
    selection: Option<&SelectionState>,
    structured_mode: bool,
    expanded_activity_rows: &HashSet<(String, String)>,
    structured_scroll_offset: Option<usize>,
    activity_click_regions: Option<&mut Vec<ClickRegion>>,
) -> Option<Rect> {
    if let Some(pane) = mux.get(pane_id) {
        let backend_ownership = mux.pane_backend_ownership(pane_id);
        let is_gateway = pane.is_gateway_backed();
        let can_manual_reset = match pane.kind() {
            PaneKind::Worker => is_gateway,
            PaneKind::Reviewer => pane.review_context().is_none(),
            PaneKind::Advisor => true,
            PaneKind::Research => true,
            PaneKind::Supervisor => true,
            PaneKind::Director | PaneKind::Shell => false,
        };

        if is_gateway && structured_mode {
            if let Some(activity_buffer) = pane.activity_buffer() {
                let reset_padding = if can_manual_reset {
                    "[reset]".len() + 1
                } else {
                    0
                };
                render_structured_pane_with_padding(
                    frame,
                    area,
                    pane,
                    activity_buffer,
                    focused,
                    reset_padding,
                    expanded_activity_rows,
                    structured_scroll_offset,
                    activity_click_regions,
                );
            } else {
                let footer_text = pane_footer_text(pane, None, structured_scroll_offset);
                let right_label_padding = if can_manual_reset {
                    "[reset]".len() + 1
                } else {
                    0
                };
                let inner = render_pane_panel(
                    frame,
                    area,
                    pane,
                    focused,
                    &footer_text,
                    right_label_padding,
                    backend_ownership,
                );
                render_terminal_viewport(
                    frame,
                    inner,
                    mux,
                    pane_id,
                    pane,
                    focused,
                    structured_scroll_offset,
                );

                if let Some(sel) = selection.filter(|s| s.pane_id == pane_id) {
                    render_selection_overlay(frame, inner, sel);
                }
            }
        } else {
            let footer_text =
                pane_footer_text(pane, pane.activity_buffer(), structured_scroll_offset);
            let right_label_padding = if can_manual_reset {
                "[reset]".len() + 1
            } else {
                0
            };
            let inner = render_pane_panel(
                frame,
                area,
                pane,
                focused,
                &footer_text,
                right_label_padding,
                backend_ownership,
            );
            render_terminal_viewport(
                frame,
                inner,
                mux,
                pane_id,
                pane,
                focused,
                structured_scroll_offset,
            );

            // Highlight selected cells by inverting fg/bg in the rendered buffer.
            if let Some(sel) = selection.filter(|s| s.pane_id == pane_id) {
                render_selection_overlay(frame, inner, sel);
            }
        }

        if can_manual_reset {
            let label = "[reset]";
            let label_width = label.len() as u16;
            if area.width > label_width + 2 {
                let rect = Rect::new(
                    area.x + area.width.saturating_sub(label_width + 2),
                    area.y,
                    label_width,
                    1,
                );
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        label,
                        Style::default()
                            .fg(if focused {
                                crate::theme::agent::color(pane.cli_type().name())
                            } else {
                                Color::Yellow
                            })
                            .add_modifier(Modifier::BOLD),
                    ))),
                    rect,
                );
                return Some(rect);
            }
        }
    } else {
        render_missing_pane_placeholder(frame, area, pane_id, focused);
    }

    None
}

fn render_selection_overlay(frame: &mut Frame, inner: Rect, sel: &SelectionState) {
    let (start, end) = sel.ordered();
    let buf = frame.buffer_mut();
    for row in start.row..=end.row.min(inner.height.saturating_sub(1)) {
        let c0 = if row == start.row { start.col } else { 0 };
        let c1 = if row == end.row {
            end.col
        } else {
            inner.width.saturating_sub(1)
        };
        for col in c0..=c1.min(inner.width.saturating_sub(1)) {
            let x = inner.x + col;
            let y = inner.y + row;
            let cell = &mut buf[Position::new(x, y)];
            std::mem::swap(&mut cell.fg, &mut cell.bg);
            if cell.bg == Color::Reset {
                cell.bg = Color::White;
            }
            if cell.fg == Color::Reset {
                cell.fg = Color::Black;
            }
        }
    }
}

pub(crate) fn render_host_owned_pane_in_area(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    pane_id: &str,
    focused: bool,
    runtime_status: Option<&RuntimeDaemonDashboardStatus>,
) -> Option<Rect> {
    let pane = mux.get(pane_id)?;

    let can_manual_reset = match pane.kind() {
        PaneKind::Worker => true,
        PaneKind::Reviewer => pane.review_context().is_none(),
        PaneKind::Advisor => true,
        PaneKind::Research => true,
        PaneKind::Supervisor => true,
        PaneKind::Director | PaneKind::Shell => false,
    };
    let right_label_padding = if can_manual_reset {
        "[reset]".len() + 1
    } else {
        0
    };
    let runtime_pane = runtime_status.and_then(|status| runtime_pane_for(status, pane_id));
    let footer_text = host_owned_pane_footer(runtime_pane);
    let inner = render_pane_panel(
        frame,
        area,
        pane,
        focused,
        &footer_text,
        right_label_padding,
        Some(PaneBackendOwnership::HostOwned),
    );

    let lines = host_owned_pane_lines(runtime_status, runtime_pane, pane);
    frame.render_widget(Paragraph::new(lines), inner);

    if can_manual_reset {
        let label = "[reset]";
        let label_width = label.len() as u16;
        if area.width > label_width + 2 {
            let rect = Rect::new(
                area.x + area.width.saturating_sub(label_width + 2),
                area.y,
                label_width,
                1,
            );
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(if focused {
                            crate::theme::agent::color(pane.cli_type().name())
                        } else {
                            Color::Yellow
                        })
                        .add_modifier(Modifier::BOLD),
                ))),
                rect,
            );
            return Some(rect);
        }
    }

    None
}

fn runtime_pane_for<'a>(
    status: &'a RuntimeDaemonDashboardStatus,
    pane_id: &str,
) -> Option<&'a RuntimePaneDashboardInfo> {
    status
        .registry
        .panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)
}

fn host_owned_pane_footer(runtime_pane: Option<&RuntimePaneDashboardInfo>) -> String {
    match runtime_pane {
        Some(pane) => format!(
            "host {} • {} • gen {}",
            runtime_source_label(pane.source.as_ref()),
            runtime_pane_state_label(&pane.state),
            pane.generation
        ),
        None => "host pane pending".to_string(),
    }
}

fn host_owned_pane_lines(
    runtime_status: Option<&RuntimeDaemonDashboardStatus>,
    runtime_pane: Option<&RuntimePaneDashboardInfo>,
    pane: &brehon_mux::Pane,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("terminal host pane", Style::default().fg(Color::White)),
        Span::styled(
            "  runtime state is owned by the daemon",
            Style::default().fg(TEXT_DIM),
        ),
    ]));

    match runtime_pane {
        Some(runtime_pane) => {
            lines.push(Line::from(vec![
                Span::styled("state ", Style::default().fg(TEXT_DIM)),
                Span::styled(
                    runtime_pane_state_label(&runtime_pane.state),
                    runtime_pane_state_style(&runtime_pane.state),
                ),
                Span::styled(
                    format!(
                        "  source={}  generation={}  role={}",
                        runtime_source_label(runtime_pane.source.as_ref()),
                        runtime_pane.generation,
                        runtime_pane_kind_label(pane.kind())
                    ),
                    Style::default().fg(TEXT_DIM),
                ),
            ]));
            if let Some(title) = runtime_pane.title.as_deref() {
                lines.push(Line::from(vec![
                    Span::styled("title ", Style::default().fg(TEXT_DIM)),
                    Span::styled(title.to_string(), Style::default().fg(Color::White)),
                ]));
            }
            if let Some(last_output_ms) = runtime_pane.last_output_ms {
                lines.push(Line::from(Span::styled(
                    format!("last_output_ms {last_output_ms}"),
                    Style::default().fg(TEXT_DIM),
                )));
            }
            if let Some(reason) = runtime_pane.exit_reason.as_deref() {
                lines.push(Line::from(Span::styled(
                    format!("exit_reason {reason}"),
                    Style::default().fg(crate::theme::status::ERROR),
                )));
            }
        }
        None => {
            lines.push(Line::from(Span::styled(
                "waiting for runtime pane registration",
                Style::default().fg(crate::theme::status::PENDING),
            )));
        }
    }

    if let Some(host) = runtime_status.and_then(|status| status.terminal_host.as_ref()) {
        lines.push(Line::from(Span::styled(
            format!(
                "host {:?}  observation={}  pane_owner={:?}",
                host.kind,
                if host.observation_running {
                    "on"
                } else {
                    "off"
                },
                host.pane_ownership
            ),
            Style::default().fg(TEXT_DIM),
        )));
        if let Some(command) = runtime_terminal_host_attach_command(host) {
            lines.push(Line::from(Span::styled(
                format!("open {command}"),
                Style::default().fg(TEXT_DIM),
            )));
        }
    }

    lines.push(Line::from(Span::styled(
        "pane controls route through runtime commands",
        Style::default().fg(TEXT_DIM),
    )));
    lines
}

fn runtime_source_label(source: Option<&RuntimeSource>) -> &'static str {
    match source {
        Some(RuntimeSource::Mux) => "mux",
        Some(RuntimeSource::Daemon) => "daemon",
        Some(RuntimeSource::EmbeddedTui) => "embedded-tui",
        Some(RuntimeSource::Web) => "web",
        Some(RuntimeSource::NativeGui) => "native-gui",
        Some(RuntimeSource::Headless) => "headless",
        Some(RuntimeSource::Detector) => "detector",
        Some(RuntimeSource::Policy) => "policy",
        Some(RuntimeSource::Other { .. }) => "other",
        None => "unknown",
    }
}

fn runtime_pane_state_label(state: &RuntimePaneState) -> &'static str {
    match state {
        RuntimePaneState::Ready => "ready",
        RuntimePaneState::Busy => "busy",
        RuntimePaneState::Dead => "dead",
        RuntimePaneState::Unknown => "unknown",
    }
}

fn runtime_pane_state_style(state: &RuntimePaneState) -> Style {
    match state {
        RuntimePaneState::Ready => Style::default().fg(crate::theme::status::SUCCESS),
        RuntimePaneState::Busy => Style::default().fg(crate::theme::status::PENDING),
        RuntimePaneState::Dead => Style::default().fg(crate::theme::status::ERROR),
        RuntimePaneState::Unknown => Style::default().fg(TEXT_DIM),
    }
}

fn runtime_pane_kind_label(kind: &PaneKind) -> &'static str {
    match kind {
        PaneKind::Supervisor => "supervisor",
        PaneKind::Worker => "worker",
        PaneKind::Reviewer => "reviewer",
        PaneKind::Advisor => "advisor",
        PaneKind::Research => "research",
        PaneKind::Director => "director",
        PaneKind::Shell => "shell",
    }
}

// ── Rendering: status bar ───────────────────────────────────────────────────

pub(crate) fn supervisor_idle_duration(
    mux: &Mux,
    last_activity: &HashMap<String, Instant>,
    now: Instant,
) -> Option<Duration> {
    let supervisor = mux.supervisor()?;
    let last_seen = last_activity
        .get(supervisor.id())
        .copied()
        .unwrap_or_else(|| supervisor.last_output_at());
    let idle_for = now.saturating_duration_since(last_seen);
    (idle_for > SUPERVISOR_IDLE_INDICATOR_THRESHOLD).then_some(idle_for)
}

pub(crate) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    mux: &Mux,
    last_activity: &HashMap<String, Instant>,
    now: Instant,
) {
    let focused = mux.focused();
    let focused_name = focused.map(|p| p.id().to_string()).unwrap_or_default();
    let focused_cli = focused
        .map(|p| pane_identity_label(p).to_string())
        .unwrap_or_default();
    let focused_adapter_color = focused
        .as_ref()
        .map(|p| crate::theme::agent::color(p.cli_type().name()))
        .unwrap_or(Color::White);
    let focused_kind = focused.map(|p| p.kind().clone());
    let focused_panesmith_supervisor = focused.as_ref().is_some_and(|pane| {
        *pane.kind() == PaneKind::Supervisor && mux.is_panesmith_managed(pane.id())
    });
    let pane_count = mux.panes().count();
    let key_style = Style::default().fg(crate::theme::brand::PRIMARY).bg(BG);
    let label_style = Style::default().fg(TEXT_DIM).bg(BG);
    let separator_style = Style::default().fg(TEXT_MUTED).bg(BG);
    let separator = || {
        Span::styled(
            format!("  {}  ", crate::theme::glyph::BULLET),
            separator_style,
        )
    };

    let mut spans = Vec::new();
    if let Some(idle_for) = supervisor_idle_duration(mux, last_activity, now) {
        spans.push(Span::styled(
            format!(
                "{} supervisor idle {}s",
                crate::theme::role::glyph(&PaneKind::Supervisor),
                idle_for.as_secs()
            ),
            Style::default().fg(crate::theme::status::IDLE).bg(BG),
        ));
        spans.push(separator());
    }
    spans.extend([
        Span::styled(" C-q", key_style),
        Span::styled(":Quit", label_style),
        separator(),
        Span::styled("C-]", key_style),
        Span::styled(":Next", label_style),
        separator(),
        Span::styled("C-d", key_style),
        Span::styled(":Dash", label_style),
        separator(),
        Span::styled("C-o", key_style),
        Span::styled(":Compose", label_style),
        separator(),
        Span::styled("C-t", key_style),
        Span::styled(":Runtime", label_style),
        separator(),
        Span::styled("C-a", key_style),
        Span::styled(":Advisors", label_style),
        separator(),
        Span::styled("C-w", key_style),
        Span::styled(":Workers", label_style),
        separator(),
        Span::styled("C-e", key_style),
        Span::styled(":Reviewers", label_style),
        separator(),
        Span::styled("C-s", key_style),
        Span::styled(":Supervisor", label_style),
        separator(),
        Span::styled("C-v", key_style),
        Span::styled(":Struct", label_style),
        separator(),
    ]);
    if focused_panesmith_supervisor {
        spans.extend([
            Span::styled("C-f", key_style),
            Span::styled(":Full", label_style),
            separator(),
        ]);
    }
    spans.extend([
        Span::styled("C-r", key_style),
        Span::styled(
            if matches!(focused_kind.as_ref(), Some(PaneKind::Supervisor)) {
                ":Reset*"
            } else {
                ":Reset"
            },
            label_style,
        ),
        separator(),
        Span::styled(
            format!("{} panes", pane_count),
            Style::default().fg(Color::Cyan).bg(BG),
        ),
        separator(),
        Span::styled(
            focused_name,
            Style::default()
                .fg(focused_adapter_color)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", focused_cli),
            Style::default().fg(TEXT_MUTED).bg(BG),
        ),
        Span::styled(
            if matches!(focused_kind.as_ref(), Some(PaneKind::Supervisor)) {
                "  *click header reset for supervisor"
            } else {
                ""
            },
            Style::default().fg(TEXT_MUTED).bg(BG),
        ),
    ]);
    let status = Line::from(spans);
    frame.render_widget(Paragraph::new(status).style(Style::default().bg(BG)), area);
}

#[cfg(test)]
mod tests {
    use super::activity_visible_range;

    #[test]
    fn activity_visible_range_scrolls_expanded_structured_content() {
        assert_eq!(activity_visible_range(120, 20, 0), (100, 120));
        assert_eq!(activity_visible_range(120, 20, 30), (70, 90));
        assert_eq!(activity_visible_range(120, 20, usize::MAX), (0, 20));
    }

    #[test]
    fn activity_visible_range_handles_short_or_empty_viewports() {
        assert_eq!(activity_visible_range(5, 20, 10), (0, 5));
        assert_eq!(activity_visible_range(5, 0, 10), (0, 0));
    }
}
