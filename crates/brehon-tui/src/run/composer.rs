//! Brehon-side message composer for durable supervisor directives.

use std::path::Path;

use brehon_mux::{PromptQueueEntry, SessionScopedQueue};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::components::Panel;
use crate::theme::{brand, chrome};

use super::layout::{centered_dialog_rect, expand_rect, inset_rect};
use super::recovery::runtime_prompt_queue_dir_for_session;
use super::rendering::truncate_to;
use super::types::{ComposerState, ComposerWorkflow, InputMode, MentionCompletionState};

const OPERATOR_SENDER: &str = "operator";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerSubmission {
    pub state: ComposerState,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnqueuedComposerMessage {
    pub entry_id: String,
    pub prompt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerKeyAction {
    Ignored,
    Handled,
    Submitted(ComposerSubmission),
}

pub(crate) fn should_open_composer(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
}

pub(crate) fn handle_composer_key_event(
    key: &KeyEvent,
    input_mode: &mut InputMode,
) -> ComposerKeyAction {
    let InputMode::Composer(state) = input_mode else {
        return ComposerKeyAction::Ignored;
    };

    match key.code {
        KeyCode::Esc => {
            *input_mode = InputMode::Normal;
            ComposerKeyAction::Handled
        }
        KeyCode::Tab => {
            if state.advisor_room_id().is_some() {
                complete_advisor_mention(state, 1);
                return ComposerKeyAction::Handled;
            }
            state.workflow = next_workflow(state.workflow, 1);
            state.status = None;
            ComposerKeyAction::Handled
        }
        KeyCode::BackTab => {
            if state.advisor_room_id().is_some() {
                complete_advisor_mention(state, -1);
                return ComposerKeyAction::Handled;
            }
            state.workflow = next_workflow(state.workflow, -1);
            state.status = None;
            ComposerKeyAction::Handled
        }
        KeyCode::Enter => {
            if state.advisor_room_id().is_none() && key.modifiers.contains(KeyModifiers::SHIFT) {
                insert_text(state, "\n");
                return ComposerKeyAction::Handled;
            }
            submit_current_state(input_mode)
        }
        KeyCode::Char('j') | KeyCode::Char('J')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            insert_text(state, "\n");
            ComposerKeyAction::Handled
        }
        KeyCode::Backspace => {
            delete_before_cursor(state);
            ComposerKeyAction::Handled
        }
        KeyCode::Delete => {
            delete_at_cursor(state);
            ComposerKeyAction::Handled
        }
        KeyCode::Left => {
            state.cursor = previous_char_boundary(&state.text, state.cursor);
            ComposerKeyAction::Handled
        }
        KeyCode::Right => {
            state.cursor = next_char_boundary(&state.text, state.cursor);
            ComposerKeyAction::Handled
        }
        KeyCode::Home => {
            state.cursor = 0;
            ComposerKeyAction::Handled
        }
        KeyCode::End => {
            state.cursor = state.text.len();
            ComposerKeyAction::Handled
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            insert_char(state, ch);
            ComposerKeyAction::Handled
        }
        _ => ComposerKeyAction::Handled,
    }
}

pub(crate) fn handle_composer_paste(text: &str, input_mode: &mut InputMode) -> bool {
    let InputMode::Composer(state) = input_mode else {
        return false;
    };
    insert_text(state, text);
    true
}

pub(crate) fn handle_composer_mouse_event(_mouse: MouseEvent, input_mode: &mut InputMode) -> bool {
    if matches!(input_mode, InputMode::Composer(_)) {
        true
    } else {
        false
    }
}

pub(crate) fn render_composer(frame: &mut Frame, area: Rect, state: &mut ComposerState) {
    if state.advisor_room_id().is_some() {
        render_room_chat_bar(frame, area, state, "Chat");
        return;
    }
    if state.is_research_room() {
        render_room_chat_bar(frame, area, state, "Research");
        return;
    }

    let dialog_area = composer_drawer_rect(area);
    let matte_area = expand_rect(dialog_area, area, 2, 1);
    state.area = dialog_area;

    frame.render_widget(Clear, matte_area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(chrome::PANEL_MATTE_BORDER))
            .style(Style::default().bg(chrome::PANEL_MATTE_BG)),
        matte_area,
    );

    let inner = inset_rect(
        Panel::new("Brehon composer")
            .accent(brand::PRIMARY)
            .border(chrome::PANEL_BORDER_ELEVATED)
            .bg(chrome::PANEL_BG_ELEVATED)
            .right_label(state.workflow.skill().unwrap_or("plain"))
            .footer_label("Enter send  Shift+Enter/C-j newline  Tab workflow  Esc close")
            .render(frame, dialog_area),
        1,
        1,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(workflow_line(state)).style(Style::default().bg(chrome::PANEL_BG_ELEVATED)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(context_line(state)).style(Style::default().bg(chrome::PANEL_BG_ELEVATED)),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(text_with_cursor(state))
            .style(Style::default().fg(chrome::TEXT_SOFT).bg(chrome::PANEL_BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(chrome::PANEL_BORDER))
                    .title(" directive "),
            )
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(status_line(state)).style(Style::default().bg(chrome::PANEL_BG_ELEVATED)),
        chunks[3],
    );
}

fn render_room_chat_bar(frame: &mut Frame, area: Rect, state: &mut ComposerState, label: &str) {
    if area.width == 0 || area.height == 0 {
        state.area = area;
        return;
    }

    let bar_area = advisor_chat_bar_rect(area, state);
    state.area = bar_area;
    frame.render_widget(Clear, bar_area);

    let room_id = state
        .advisor_room_id()
        .or_else(|| state.research_task_id())
        .unwrap_or("new request");
    let title = truncate_to(
        &format!(" {label} {room_id} - Enter post / Esc close "),
        bar_area.width.saturating_sub(4) as usize,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(brand::PRIMARY))
        .style(Style::default().bg(chrome::PANEL_BG_ELEVATED))
        .title(title);
    let input_area = block.inner(bar_area);
    frame.render_widget(block, bar_area);
    let chunks = if input_area.height > 1 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(input_area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1)])
            .split(input_area)
    };
    frame.render_widget(
        Paragraph::new(text_with_cursor(state))
            .style(
                Style::default()
                    .fg(chrome::TEXT_SOFT)
                    .bg(chrome::PANEL_BG_ELEVATED),
            )
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    if chunks.len() > 1 {
        frame.render_widget(
            Paragraph::new(status_line(state)).style(
                Style::default()
                    .fg(chrome::TEXT_DIM)
                    .bg(chrome::PANEL_BG_ELEVATED),
            ),
            chunks[1],
        );
    }
}

fn advisor_chat_bar_rect(area: Rect, state: &ComposerState) -> Rect {
    let text_lines = state.text.lines().count().max(1) as u16;
    let desired_height = (text_lines + 3).clamp(4, 8);
    let height = desired_height.min(area.height);
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    )
}

pub(crate) fn build_composer_message(state: &ComposerState) -> String {
    let request = state.text.trim();
    if state.advisor_room_id().is_some() || state.is_research_room() {
        return request.to_string();
    }

    let context = state
        .task_id
        .as_deref()
        .map(|task_id| format!("Context: operator is inspecting task {task_id}."))
        .unwrap_or_else(|| "Context: current Brehon run.".to_string());

    match state.workflow {
        ComposerWorkflow::Discover => workflow_message(
            "brehon-discovery",
            &context,
            request,
            "Use the Brehon discovery workflow to clarify the goal, surface constraints, and draft an interactive work plan. Do not dispatch workers until the operator approves the plan.",
        ),
        ComposerWorkflow::BreakDown => workflow_message(
            "brehon-breakdown",
            &context,
            request,
            "Use the Brehon breakdown workflow to convert the approved plan into task structure, dependencies, acceptance criteria, and dispatch-ready units. Do not start workers unless explicitly asked.",
        ),
        ComposerWorkflow::Dispatch => workflow_message(
            "brehon-dispatch",
            &context,
            request,
            "Use the Brehon dispatch workflow to refresh ready work, assign or resume executable tasks, and keep dashboard state current.",
        ),
        ComposerWorkflow::Recover => workflow_message(
            "brehon-supervisor-checklist",
            &context,
            request,
            "Use the supervisor checklist to inspect current run state, recover stalled or inconsistent work, and report what you changed.",
        ),
        ComposerWorkflow::Message => {
            format!("[Brehon operator message]\n\n{context}\n\n{request}")
        }
    }
}

pub(crate) fn enqueue_composer_message(
    brehon_root: &Path,
    session_name: Option<&str>,
    target: &str,
    message: &str,
) -> Result<EnqueuedComposerMessage, String> {
    let session_name = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("_legacy");
    let queue_dir = runtime_prompt_queue_dir_for_session(brehon_root, Some(session_name));
    let queue = SessionScopedQueue::<PromptQueueEntry>::new(session_name, queue_dir);
    let entry = PromptQueueEntry::new(target, Some(OPERATOR_SENDER), message);
    let prompt_id = entry.prompt_id.clone();
    let entry_id = queue.enqueue(entry).map_err(|err| err.to_string())?;
    Ok(EnqueuedComposerMessage {
        entry_id,
        prompt_id,
    })
}

fn workflow_message(skill: &str, context: &str, request: &str, directive: &str) -> String {
    format!(
        "[Brehon operator directive]\nWorkflow: {skill}\nSkill: {skill}\nTarget: supervisor\n\n{context}\n\nRequest:\n{request}\n\nDirective:\nUse the Brehon skill named {skill}. {directive}"
    )
}

fn composer_drawer_rect(area: Rect) -> Rect {
    if area.width < 96 || area.height < 18 {
        return centered_dialog_rect(area, 82, 70, 96, 20);
    }

    let width = ((area.width as u32 * 42) / 100) as u16;
    let width = width.max(52).min(88).min(area.width.saturating_sub(4));
    let height = area.height.saturating_sub(4);
    let x = area.x + area.width.saturating_sub(width).saturating_sub(2);
    let y = area.y + 1;
    Rect::new(x, y, width, height)
}

fn submit_current_state(input_mode: &mut InputMode) -> ComposerKeyAction {
    let InputMode::Composer(state) = input_mode else {
        return ComposerKeyAction::Ignored;
    };
    if state.text.trim().is_empty() {
        state.status = Some("Type a directive before sending.".to_string());
        return ComposerKeyAction::Handled;
    }
    let submitted_state = state.clone();
    let message = build_composer_message(&submitted_state);
    *input_mode = InputMode::Normal;
    ComposerKeyAction::Submitted(ComposerSubmission {
        state: submitted_state,
        message,
    })
}

fn next_workflow(current: ComposerWorkflow, direction: isize) -> ComposerWorkflow {
    let workflows = ComposerWorkflow::ALL;
    let current_index = workflows
        .iter()
        .position(|workflow| *workflow == current)
        .unwrap_or(0);
    let len = workflows.len() as isize;
    let next = (current_index as isize + direction).rem_euclid(len) as usize;
    workflows[next]
}

fn insert_char(state: &mut ComposerState, ch: char) {
    state.status = None;
    state.mention_completion = None;
    state.text.insert(state.cursor, ch);
    state.cursor += ch.len_utf8();
}

fn insert_text(state: &mut ComposerState, text: &str) {
    if text.is_empty() {
        return;
    }
    state.status = None;
    state.mention_completion = None;
    state.text.insert_str(state.cursor, text);
    state.cursor += text.len();
}

fn delete_before_cursor(state: &mut ComposerState) {
    if state.cursor == 0 {
        return;
    }
    state.status = None;
    state.mention_completion = None;
    let previous = previous_char_boundary(&state.text, state.cursor);
    state.text.replace_range(previous..state.cursor, "");
    state.cursor = previous;
}

fn delete_at_cursor(state: &mut ComposerState) {
    if state.cursor >= state.text.len() {
        return;
    }
    state.status = None;
    state.mention_completion = None;
    let next = next_char_boundary(&state.text, state.cursor);
    state.text.replace_range(state.cursor..next, "");
}

fn complete_advisor_mention(state: &mut ComposerState, direction: isize) {
    if state.mention_candidates.is_empty() {
        state.status = Some("No worker panes are available for @mentions.".to_string());
        state.mention_completion = None;
        return;
    }

    let Some((token_start, token_end, prefix)) = mention_token_at_cursor(&state.text, state.cursor)
    else {
        state.status = Some("Type @ then a worker name; Tab completes mentions.".to_string());
        state.mention_completion = None;
        return;
    };

    let active = state.mention_completion.as_ref().and_then(|completion| {
        let selected = completion.matches.get(completion.selected)?;
        if completion.token_start == token_start && prefix == *selected {
            Some(completion.clone())
        } else {
            None
        }
    });

    let matches = active
        .as_ref()
        .map(|completion| completion.matches.clone())
        .unwrap_or_else(|| mention_matches(&state.mention_candidates, &prefix));

    if matches.is_empty() {
        state.status = Some(format!("No live worker matches @{prefix}."));
        state.mention_completion = None;
        return;
    }

    let selected = if let Some(completion) = active {
        let len = completion.matches.len() as isize;
        (completion.selected as isize + direction).rem_euclid(len) as usize
    } else {
        0
    };
    let replacement = &matches[selected];
    state
        .text
        .replace_range(token_start + 1..token_end, replacement);
    state.cursor = token_start + 1 + replacement.len();

    if matches.len() == 1 {
        if state.cursor == state.text.len() {
            state.text.push(' ');
            state.cursor += 1;
        } else if !state.text[state.cursor..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            state.text.insert(state.cursor, ' ');
            state.cursor += 1;
        }
        state.status = Some(format!("Mentioning @{replacement}."));
        state.mention_completion = None;
    } else {
        state.status = Some(format!(
            "Mention {}/{}: @{} (Tab cycles)",
            selected + 1,
            matches.len(),
            replacement
        ));
        state.mention_completion = Some(MentionCompletionState {
            token_start,
            matches,
            selected,
        });
    }
}

fn mention_matches(candidates: &[String], prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|candidate| candidate.to_ascii_lowercase().starts_with(&prefix))
        .cloned()
        .collect()
}

fn mention_token_at_cursor(text: &str, cursor: usize) -> Option<(usize, usize, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }

    let before = &text[..cursor];
    let mut start = cursor;
    for (idx, ch) in before.char_indices().rev() {
        if ch == '@' {
            start = idx;
            break;
        }
        if !is_mention_char(ch) {
            return None;
        }
        start = idx;
    }

    if text.as_bytes().get(start) != Some(&b'@') {
        return None;
    }
    if start > 0 {
        let previous = text[..start].chars().next_back()?;
        if is_mention_char(previous) {
            return None;
        }
    }

    let mut end = cursor;
    for (offset, ch) in text[cursor..].char_indices() {
        if is_mention_char(ch) {
            end = cursor + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    let prefix = text[start + 1..cursor].to_string();
    Some((start, end, prefix))
}

fn is_mention_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')
}

pub(crate) fn worker_mentions_in_message(
    message: &str,
    worker_ids: &[String],
) -> Result<Vec<String>, Vec<String>> {
    let known = worker_ids.iter().collect::<std::collections::HashSet<_>>();
    let mut mentions = Vec::new();
    let mut unknown = Vec::new();

    let mut idx = 0usize;
    while let Some(relative) = message[idx..].find('@') {
        let start = idx + relative;
        if start > 0 {
            let previous = message[..start].chars().next_back().unwrap_or(' ');
            if is_mention_char(previous) {
                idx = start + 1;
                continue;
            }
        }

        let mut end = start + 1;
        for (offset, ch) in message[start + 1..].char_indices() {
            if is_mention_char(ch) {
                end = start + 1 + offset + ch.len_utf8();
            } else {
                break;
            }
        }
        if end == start + 1 {
            idx = start + 1;
            continue;
        }

        let mention = message[start + 1..end].to_string();
        if known.contains(&mention) {
            if !mentions.contains(&mention) {
                mentions.push(mention);
            }
        } else if !unknown.contains(&mention) {
            unknown.push(mention);
        }
        idx = end;
    }

    if unknown.is_empty() {
        Ok(mentions)
    } else {
        Err(unknown)
    }
}

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .last()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    if cursor >= value.len() {
        return value.len();
    }
    value[cursor..]
        .char_indices()
        .nth(1)
        .map(|(idx, _)| cursor + idx)
        .unwrap_or(value.len())
}

fn workflow_line(state: &ComposerState) -> Line<'static> {
    if let Some(room_id) = state.advisor_room_id() {
        return Line::from(vec![
            Span::styled("Mode ", Style::default().fg(chrome::TEXT_DIM)),
            Span::styled(
                " Advisor room ",
                Style::default()
                    .fg(brand::PRIMARY)
                    .bg(chrome::PANEL_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {room_id}"),
                Style::default().fg(chrome::TEXT_SOFT),
            ),
        ]);
    }
    if state.is_research_room() {
        let target = state.research_task_id().unwrap_or("new task request");
        return Line::from(vec![
            Span::styled("Mode ", Style::default().fg(chrome::TEXT_DIM)),
            Span::styled(
                " Research room ",
                Style::default()
                    .fg(brand::PRIMARY)
                    .bg(chrome::PANEL_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {target}"), Style::default().fg(chrome::TEXT_SOFT)),
        ]);
    }

    let active = state.workflow;
    let mut spans = vec![
        Span::styled("Workflow ", Style::default().fg(chrome::TEXT_DIM)),
        Span::raw(" "),
    ];
    for workflow in ComposerWorkflow::ALL {
        let selected = workflow == active;
        let style = if selected {
            Style::default()
                .fg(brand::PRIMARY)
                .bg(chrome::PANEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(chrome::TEXT_SOFT)
        };
        spans.push(Span::styled(format!(" {} ", workflow.label()), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn context_line(state: &ComposerState) -> Line<'static> {
    if let Some(room_id) = state.advisor_room_id() {
        return Line::from(vec![
            Span::styled("Room ", Style::default().fg(chrome::TEXT_DIM)),
            Span::styled(
                room_id.to_string(),
                Style::default()
                    .fg(chrome::TEXT_LABEL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  local transcript update; advisors reply asynchronously",
                Style::default().fg(chrome::TEXT_MUTED),
            ),
        ]);
    }
    if state.is_research_room() {
        let target = state.research_task_id().unwrap_or("/task required");
        return Line::from(vec![
            Span::styled("Task ", Style::default().fg(chrome::TEXT_DIM)),
            Span::styled(
                target.to_string(),
                Style::default()
                    .fg(chrome::TEXT_LABEL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  queues async research; no wait on model output",
                Style::default().fg(chrome::TEXT_MUTED),
            ),
        ]);
    }

    let task = state
        .task_id
        .as_deref()
        .map(|task_id| format!("  task {task_id}"))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled("Target ", Style::default().fg(chrome::TEXT_DIM)),
        Span::styled(
            state.target.clone(),
            Style::default()
                .fg(chrome::TEXT_LABEL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(task, Style::default().fg(chrome::TEXT_MUTED)),
    ])
}

fn status_line(state: &ComposerState) -> Line<'static> {
    let status = state.status.clone().unwrap_or_else(|| {
        if state.advisor_room_id().is_some() {
            "Posted locally after send; no wait on advisor output.".to_string()
        } else if state.is_research_room() {
            "Queued as a research job after send; artifacts attach back to the task.".to_string()
        } else {
            "Queued through Brehon prompt queue after send.".to_string()
        }
    });
    Line::from(vec![Span::styled(
        status,
        Style::default().fg(chrome::TEXT_DIM),
    )])
}

fn text_with_cursor(state: &ComposerState) -> String {
    let mut rendered = state.text.clone();
    rendered.insert(state.cursor, '|');
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::recovery::read_queued_prompt;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    fn buffer_text(buffer: &Buffer) -> String {
        let mut rows = Vec::new();
        for row in 0..buffer.area.height {
            let text = (0..buffer.area.width)
                .filter_map(|col| buffer.cell((col, row)).map(|cell| cell.symbol()))
                .collect::<String>();
            rows.push(text);
        }
        rows.join("\n")
    }

    #[test]
    fn workflow_message_keeps_skill_contract() {
        let mut state = ComposerState::new("supervisor", Some("T-12".to_string()));
        state.text = "Turn this request into a plan".to_string();

        let message = build_composer_message(&state);

        assert!(message.contains("Workflow: brehon-discovery"));
        assert!(message.contains("Skill: brehon-discovery"));
        assert!(message.contains("Use the Brehon skill named brehon-discovery"));
        assert!(message.contains("task T-12"));
        assert!(message.contains("Do not dispatch workers"));
    }

    #[test]
    fn advisor_composer_posts_plain_room_message() {
        let mut state = ComposerState::new_advisor("release-war-room");
        state.text = "Compare the remaining release risks.".to_string();

        let message = build_composer_message(&state);

        assert_eq!(message, "Compare the remaining release risks.");
        assert_eq!(state.advisor_room_id(), Some("release-war-room"));
    }

    #[test]
    fn research_composer_posts_plain_request() {
        let mut state = ComposerState::new_research(Some("T-42".to_string()));
        state.text = "Find the relevant normative sections.".to_string();

        let message = build_composer_message(&state);

        assert_eq!(message, "Find the relevant normative sections.");
        assert_eq!(state.research_task_id(), Some("T-42"));
    }

    #[test]
    fn advisor_composer_renders_as_bottom_chat_bar() {
        let mut state = ComposerState::new_advisor("norma-war-room");
        state.text = "Ask the room for a boundary check".to_string();
        state.cursor = state.text.len();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        terminal
            .draw(|frame| render_composer(frame, frame.area(), &mut state))
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert_eq!(state.area.y, 16);
        assert_eq!(state.area.height, 4);
        assert!(rendered.contains("Chat norma-war-room"));
        assert!(rendered.contains("Ask the room for a boundary check|"));
        assert!(!rendered.contains("Brehon composer"));
    }

    #[test]
    fn research_composer_renders_as_constrained_bottom_bar() {
        let mut state = ComposerState::new_research(Some("T-42".to_string()));
        state.text = "Find source citations".to_string();
        state.cursor = state.text.len();
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();

        terminal
            .draw(|frame| render_composer(frame, Rect::new(8, 4, 60, 12), &mut state))
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert_eq!(state.area, Rect::new(8, 12, 60, 4));
        assert!(rendered.contains("Research T-42"));
        assert!(rendered.contains("Find source citations|"));
        assert!(!rendered.contains("Brehon composer"));
    }

    #[test]
    fn advisor_composer_respects_constrained_panel_area() {
        let mut state = ComposerState::new_advisor("norma-war-room");
        state.text = "Ask the room for a boundary check".to_string();
        state.cursor = state.text.len();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        terminal
            .draw(|frame| render_composer(frame, Rect::new(10, 6, 48, 12), &mut state))
            .unwrap();

        assert_eq!(state.area, Rect::new(10, 14, 48, 4));
    }

    #[test]
    fn advisor_enter_submits_even_with_shift_modifier() {
        let mut input_mode = InputMode::Composer(ComposerState::new_advisor("norma-war-room"));
        let InputMode::Composer(state) = &mut input_mode else {
            panic!("composer should be open");
        };
        state.text = "Ship this to the room".to_string();
        state.cursor = state.text.len();

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let action = handle_composer_key_event(&enter, &mut input_mode);

        let ComposerKeyAction::Submitted(submission) = action else {
            panic!("advisor Enter should submit");
        };
        assert_eq!(submission.message, "Ship this to the room");
        assert!(matches!(input_mode, InputMode::Normal));
    }

    #[test]
    fn advisor_tab_autocompletes_worker_mentions() {
        let mut input_mode = InputMode::Composer(
            ComposerState::new_advisor("norma-war-room").with_mention_candidates(vec![
                "quick-cod-72".to_string(),
                "quiet-cow-11".to_string(),
            ]),
        );
        let InputMode::Composer(state) = &mut input_mode else {
            panic!("composer should be open");
        };
        state.text = "Can @quick".to_string();
        state.cursor = state.text.len();

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(
            handle_composer_key_event(&tab, &mut input_mode),
            ComposerKeyAction::Handled
        );

        let InputMode::Composer(state) = input_mode else {
            panic!("composer should stay open");
        };
        assert_eq!(state.text, "Can @quick-cod-72 ");
        assert_eq!(state.cursor, state.text.len());
        assert!(state.status.unwrap().contains("@quick-cod-72"));
    }

    #[test]
    fn advisor_tab_cycles_ambiguous_worker_mentions() {
        let mut input_mode = InputMode::Composer(
            ComposerState::new_advisor("norma-war-room").with_mention_candidates(vec![
                "quick-cod-72".to_string(),
                "quick-fox-91".to_string(),
            ]),
        );
        let InputMode::Composer(state) = &mut input_mode else {
            panic!("composer should be open");
        };
        state.text = "@qu".to_string();
        state.cursor = state.text.len();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());

        assert_eq!(
            handle_composer_key_event(&tab, &mut input_mode),
            ComposerKeyAction::Handled
        );
        assert_eq!(
            handle_composer_key_event(&tab, &mut input_mode),
            ComposerKeyAction::Handled
        );

        let InputMode::Composer(state) = input_mode else {
            panic!("composer should stay open");
        };
        assert_eq!(state.text, "@quick-fox-91");
        assert!(state.status.unwrap().contains("2/2"));
    }

    #[test]
    fn worker_mentions_validate_against_live_workers() {
        let workers = vec!["quick-cod-72".to_string(), "fair-owl-10".to_string()];

        let mentions = worker_mentions_in_message(
            "Please check this @quick-cod-72, then @fair-owl-10.",
            &workers,
        )
        .unwrap();

        assert_eq!(mentions, vec!["quick-cod-72", "fair-owl-10"]);
        assert_eq!(
            worker_mentions_in_message("@missing-worker should fail", &workers).unwrap_err(),
            vec!["missing-worker"]
        );
    }

    #[test]
    fn composer_drawer_anchors_to_right_on_wide_terminals() {
        let area = Rect::new(0, 0, 120, 40);
        let drawer = composer_drawer_rect(area);

        assert_eq!(drawer.width, 52);
        assert_eq!(drawer.height, 36);
        assert_eq!(drawer.x, 66);
        assert_eq!(drawer.y, 1);
    }

    #[test]
    fn composer_backspace_deletes_last_character() {
        let mut input_mode = InputMode::Composer(ComposerState::new("supervisor", None));
        for ch in ['a', 'b', 'c'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty());
            assert_eq!(
                handle_composer_key_event(&key, &mut input_mode),
                ComposerKeyAction::Handled
            );
        }
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty());
        assert_eq!(
            handle_composer_key_event(&backspace, &mut input_mode),
            ComposerKeyAction::Handled
        );

        let InputMode::Composer(state) = input_mode else {
            panic!("composer should stay open");
        };
        assert_eq!(state.text, "ab");
        assert_eq!(state.cursor, 2);
    }

    #[test]
    fn enqueue_writes_session_scoped_prompt_entry() {
        let temp = tempfile::tempdir().unwrap();
        let result = enqueue_composer_message(
            temp.path(),
            Some("session-a"),
            "supervisor",
            "hello supervisor",
        )
        .unwrap();
        assert!(result.prompt_id.is_some());

        let queue_path = temp
            .path()
            .join("runtime")
            .join("prompt-queue")
            .join("session-a")
            .join(format!("{}.entry", result.entry_id));
        let queued = read_queued_prompt(&queue_path).unwrap();
        assert_eq!(queued.target, "supervisor");
        assert_eq!(queued.from.as_deref(), Some(OPERATOR_SENDER));
        assert_eq!(queued.message, "hello supervisor");
        assert_eq!(queued.session_name.as_deref(), Some("session-a"));
        assert!(queued.prompt_id.is_some());
    }
}
