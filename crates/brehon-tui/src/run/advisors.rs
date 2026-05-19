//! Advisor room rendering.

use std::path::Path;

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::components::Panel;
use crate::theme::chrome::{self, TEXT_DIM, TEXT_MUTED};

use super::rendering::truncate_to;
use super::types::AdvisorRoomViewState;

#[derive(Debug, Clone, Default, Deserialize)]
struct AdvisorRoomFile {
    room_id: String,
    title: Option<String>,
    #[serde(default = "default_turn_mode")]
    turn_mode: String,
    #[serde(default)]
    participants: Vec<String>,
    #[serde(default)]
    messages: Vec<AdvisorMessageFile>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
struct AdvisorMessageFile {
    seq: u64,
    author: String,
    role: String,
    #[serde(default)]
    kind: String,
    content: String,
    created_at: Option<DateTime<Utc>>,
}

fn default_turn_mode() -> String {
    "open_chat".to_string()
}

pub(crate) fn advisor_room_count(brehon_root: Option<&Path>) -> usize {
    read_advisor_rooms(brehon_root).len()
}

pub(crate) fn active_advisor_room_id(brehon_root: Option<&Path>) -> String {
    read_advisor_rooms(brehon_root)
        .into_iter()
        .next()
        .map(|room| room.room_id)
        .unwrap_or_else(|| "general".to_string())
}

pub(crate) fn post_operator_advisor_message(
    brehon_root: &Path,
    room_id: &str,
    content: &str,
) -> Result<u64, String> {
    let path = advisor_room_path(brehon_root, room_id);
    let mut room = if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read advisor room {}: {err}", path.display()))?;
        serde_json::from_str::<Value>(&content)
            .map_err(|err| format!("failed to parse advisor room {}: {err}", path.display()))?
    } else {
        serde_json::json!({
            "room_id": room_id,
            "title": room_id,
            "turn_mode": "open_chat",
            "participants": [],
            "messages": [],
            "created_at": Utc::now(),
        })
    };

    let room_obj = room
        .as_object_mut()
        .ok_or_else(|| format!("advisor room {} is not a JSON object", path.display()))?;
    room_obj
        .entry("room_id".to_string())
        .or_insert_with(|| Value::String(room_id.to_string()));
    room_obj
        .entry("turn_mode".to_string())
        .or_insert_with(|| Value::String("open_chat".to_string()));
    let messages = room_obj
        .entry("messages".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| format!("advisor room {} has non-array messages", path.display()))?;
    let latest_seq = messages
        .iter()
        .filter_map(|message| message.get("seq").and_then(Value::as_u64))
        .max()
        .unwrap_or(0);
    let seq = latest_seq + 1;
    let now = Utc::now();
    messages.push(serde_json::json!({
        "seq": seq,
        "id": format!("m-{seq}-{}", now.timestamp_millis()),
        "author": "operator",
        "role": "human",
        "kind": "message",
        "content": content,
        "created_at": now,
    }));
    room_obj.insert("updated_at".to_string(), serde_json::json!(now));

    write_advisor_room_path(&path, &room)?;
    Ok(seq)
}

pub(crate) fn render_advisors_view(
    frame: &mut Frame,
    area: Rect,
    brehon_root: Option<&Path>,
    state: &mut AdvisorRoomViewState,
) {
    let rooms = read_advisor_rooms(brehon_root);
    let inner = Panel::new("Advisors")
        .subtitle("non-blocking rooms")
        .render(frame, area);

    if inner.width == 0 || inner.height == 0 {
        state.area = inner;
        state.max_scroll = 0;
        state.scroll = 0;
        return;
    }

    if rooms.is_empty() {
        state.area = inner;
        state.max_scroll = 0;
        state.scroll = 0;
        let lines = vec![
            Line::from(Span::styled(
                "No advisor rooms yet.",
                Style::default()
                    .fg(crate::theme::chrome::TEXT_BODY)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Enable the commented advisors block in .brehon/config.yaml, then seed a room with advisor action=create_room or action=post.",
                Style::default().fg(TEXT_DIM),
            )),
            Line::from(Span::styled(
                "This tab only reads local room files, so it stays responsive while agents are thinking.",
                Style::default().fg(TEXT_MUTED),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let columns = if inner.width > 72 {
        Layout::horizontal([Constraint::Length(30), Constraint::Min(20)]).split(inner)
    } else {
        Layout::horizontal([Constraint::Percentage(100)]).split(inner)
    };
    let room_list = columns[0];
    let detail = if columns.len() > 1 {
        columns[1]
    } else {
        columns[0]
    };
    let selected = &rooms[0];

    if columns.len() > 1 {
        render_room_list(frame, room_list, &rooms);
    }
    render_room_detail(frame, detail, selected, state);
}

fn render_room_list(frame: &mut Frame, area: Rect, rooms: &[AdvisorRoomFile]) {
    let mut lines = Vec::new();
    for (idx, room) in rooms.iter().take(area.height as usize).enumerate() {
        let active = idx == 0;
        let label = room.title.as_deref().unwrap_or(&room.room_id);
        let prefix = if active { "> " } else { "  " };
        let style = if active {
            Style::default()
                .fg(crate::theme::brand::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(crate::theme::chrome::TEXT_BODY)
        };
        lines.push(Line::from(vec![Span::styled(
            truncate_to(&format!("{prefix}{label}"), area.width as usize),
            style,
        )]));
        if lines.len() < area.height as usize {
            lines.push(Line::from(Span::styled(
                truncate_to(
                    &format!(
                        "  {} msgs / {}",
                        room.messages.len(),
                        room.turn_mode.replace('_', "-")
                    ),
                    area.width as usize,
                ),
                Style::default().fg(TEXT_MUTED),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_room_detail(
    frame: &mut Frame,
    area: Rect,
    room: &AdvisorRoomFile,
    state: &mut AdvisorRoomViewState,
) {
    state.area = area;
    let lines = room_detail_lines(room, area.width);
    let max_scroll = lines.len().saturating_sub(area.height as usize) as u16;
    state.max_scroll = max_scroll;
    state.scroll = state.scroll.min(max_scroll);
    frame.render_widget(Paragraph::new(lines).scroll((state.scroll, 0)), area);
}

fn room_detail_lines(room: &AdvisorRoomFile, width: u16) -> Vec<Line<'static>> {
    let title = room.title.as_deref().unwrap_or(&room.room_id);
    let participants = if room.participants.is_empty() {
        "no participants".to_string()
    } else {
        room.participants.join(", ")
    };
    let latest = room
        .updated_at
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut lines = vec![
        Line::from(vec![Span::styled(
            truncate_to(title, width as usize),
            Style::default()
                .fg(crate::theme::chrome::TEXT_BODY)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            truncate_to(
                &format!(
                    "{} / updated {} / {}",
                    room.turn_mode.replace('_', "-"),
                    latest,
                    participants
                ),
                width as usize,
            ),
            Style::default().fg(TEXT_DIM),
        )]),
        Line::from(""),
    ];

    if room.messages.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_to(
                "No messages yet. Press Ctrl-o to post to this room.",
                width as usize,
            ),
            Style::default().fg(TEXT_MUTED),
        )));
        return lines;
    }

    for (idx, message) in room.messages.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(advisor_message_block_lines(message, width as usize));
    }
    lines
}

fn advisor_message_block_lines(message: &AdvisorMessageFile, width: usize) -> Vec<Line<'static>> {
    let bubble_width = message_bubble_width(width);
    let author_color = advisor_author_color(&message.author, &message.role);
    let bubble_bg = if message.role == "human" {
        chrome::BG_ELEVATED
    } else {
        chrome::PANEL_BG_ELEVATED
    };
    let bubble_style = Style::default().fg(chrome::TEXT_BODY).bg(bubble_bg);
    let kind_style = Style::default().fg(TEXT_DIM).bg(bubble_bg);
    let time = message
        .created_at
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default();

    let mut lines = vec![Line::from(vec![
        Span::styled(
            truncate_to(
                &format!("#{:03} ", message.seq),
                width.saturating_sub(time.width() + 1),
            ),
            Style::default().fg(TEXT_DIM),
        ),
        Span::styled(
            truncate_to(
                &format!(
                    "{}{}",
                    message.author,
                    if message.role.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", message.role)
                    }
                ),
                width.saturating_sub(time.width() + 6),
            ),
            Style::default()
                .fg(author_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {time}"), Style::default().fg(TEXT_MUTED)),
    ])];

    if !message.kind.is_empty() && message.kind != "message" {
        lines.push(bubble_line(
            &format!("kind: {}", message.kind),
            bubble_width,
            kind_style,
            author_color,
        ));
    }

    let mut body =
        markdown_bubble_lines(&message.content, bubble_width, bubble_style, author_color);
    if body.is_empty() {
        body.push(bubble_line(
            "(empty message)",
            bubble_width,
            bubble_style,
            author_color,
        ));
    }
    lines.extend(body);

    lines
}

pub(crate) fn markdown_bubble_lines(
    content: &str,
    bubble_width: usize,
    paragraph_style: Style,
    author_color: Color,
) -> Vec<Line<'static>> {
    let content_width = bubble_width.saturating_sub(2).max(1);
    let heading_style = paragraph_style
        .fg(crate::theme::brand::PRIMARY)
        .add_modifier(Modifier::BOLD);
    let strong_style = paragraph_style
        .fg(chrome::TEXT_SOFT)
        .add_modifier(Modifier::BOLD);
    let quote_style = paragraph_style.fg(TEXT_DIM).add_modifier(Modifier::ITALIC);
    let code_style = Style::default().fg(chrome::TEXT_SOFT).bg(chrome::PANEL_BG);

    let mut lines = Vec::new();
    let mut in_code_block = false;
    for source_line in content.lines() {
        let trimmed = source_line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            push_wrapped_markdown_line(
                &mut lines,
                "",
                source_line,
                content_width,
                bubble_width,
                code_style,
                author_color,
            );
            continue;
        }

        if trimmed.is_empty() {
            lines.push(bubble_line("", bubble_width, paragraph_style, author_color));
            continue;
        }

        if let Some(heading) = markdown_heading(trimmed) {
            push_wrapped_markdown_line(
                &mut lines,
                "",
                &clean_inline_markdown(heading),
                content_width,
                bubble_width,
                heading_style,
                author_color,
            );
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            push_wrapped_markdown_line(
                &mut lines,
                "| ",
                &clean_inline_markdown(quote.trim_start()),
                content_width,
                bubble_width,
                quote_style,
                author_color,
            );
        } else if let Some((prefix, body)) = markdown_list_item(trimmed) {
            push_wrapped_markdown_line(
                &mut lines,
                &prefix,
                &clean_inline_markdown(body),
                content_width,
                bubble_width,
                paragraph_style,
                author_color,
            );
        } else if let Some(strong) = full_strong_markdown(trimmed) {
            push_wrapped_markdown_line(
                &mut lines,
                "",
                &clean_inline_markdown(strong),
                content_width,
                bubble_width,
                strong_style,
                author_color,
            );
        } else {
            push_wrapped_markdown_line(
                &mut lines,
                "",
                &clean_inline_markdown(trimmed),
                content_width,
                bubble_width,
                paragraph_style,
                author_color,
            );
        }
    }

    lines
}

fn push_wrapped_markdown_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    content_width: usize,
    bubble_width: usize,
    style: Style,
    author_color: Color,
) {
    let prefix_width = prefix.width().min(content_width.saturating_sub(1));
    let text_width = content_width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_advisor_text(text, text_width);
    if wrapped.is_empty() {
        lines.push(bubble_line(
            prefix.trim_end(),
            bubble_width,
            style,
            author_color,
        ));
        return;
    }

    let continuation = " ".repeat(prefix_width);
    for (idx, line) in wrapped.iter().enumerate() {
        let line_prefix = if idx == 0 { prefix } else { &continuation };
        lines.push(bubble_line(
            &format!("{line_prefix}{line}"),
            bubble_width,
            style,
            author_color,
        ));
    }
}

fn markdown_heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some(line[hashes + 1..].trim())
    } else {
        None
    }
}

fn markdown_list_item(line: &str) -> Option<(String, &str)> {
    if let Some(body) = line.strip_prefix("- ") {
        return Some(("- ".to_string(), body.trim()));
    }
    if let Some(body) = line.strip_prefix("* ") {
        return Some(("- ".to_string(), body.trim()));
    }
    if let Some(body) = line.strip_prefix("+ ") {
        return Some(("- ".to_string(), body.trim()));
    }

    let dot = line.find(". ")?;
    if dot == 0 || !line[..dot].chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((format!("{}. ", &line[..dot]), line[dot + 2..].trim()))
}

fn full_strong_markdown(line: &str) -> Option<&str> {
    line.strip_prefix("**")
        .and_then(|value| value.strip_suffix("**"))
        .or_else(|| {
            line.strip_prefix("__")
                .and_then(|value| value.strip_suffix("__"))
        })
        .map(str::trim)
}

fn clean_inline_markdown(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '*' | '_' | '`') && chars.peek() == Some(&ch) {
            chars.next();
            continue;
        }
        if ch == '`' {
            continue;
        }
        output.push(ch);
    }
    output
}

pub(crate) fn bubble_line(
    text: &str,
    bubble_width: usize,
    style: Style,
    author_color: Color,
) -> Line<'static> {
    let content_width = bubble_width.saturating_sub(2).max(1);
    let body = truncate_to(text, content_width);
    let body = pad_to_width(&body, content_width);
    Line::from(vec![
        Span::styled("  ", Style::default().bg(author_color)),
        Span::styled(format!(" {body} "), style),
    ])
}

pub(crate) fn advisor_author_color(author: &str, role: &str) -> Color {
    if role == "human" || author == "operator" {
        return crate::theme::brand::PRIMARY;
    }

    const PALETTE: [Color; 8] = [
        Color::Rgb(255, 176, 76),
        Color::Rgb(110, 185, 255),
        Color::Rgb(80, 220, 160),
        Color::Rgb(180, 130, 255),
        Color::Rgb(255, 150, 110),
        Color::Rgb(120, 210, 220),
        Color::Rgb(230, 180, 255),
        Color::Rgb(170, 220, 120),
    ];

    let hash = author.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte as usize)
    });
    PALETTE[hash % PALETTE.len()]
}

pub(crate) fn message_bubble_width(width: usize) -> usize {
    width.saturating_sub(2).clamp(1, 96)
}

fn pad_to_width(value: &str, width: usize) -> String {
    let mut output = value.to_string();
    let used = output.width();
    if used < width {
        output.push_str(&" ".repeat(width - used));
    }
    output
}

fn wrap_advisor_text(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for source_line in value.lines() {
        if source_line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut current = String::new();
        for word in source_line.split_whitespace() {
            let word_width = word.width();
            if word_width > width {
                if !current.is_empty() {
                    lines.push(current);
                    current = String::new();
                }
                lines.extend(split_long_word(word, width));
            } else if current.is_empty() {
                current.push_str(word);
            } else if current.width() + 1 + word_width <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

fn split_long_word(word: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used_width = 0usize;
    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used_width + ch_width > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
            used_width = 0;
        }
        current.push(ch);
        used_width += ch_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn read_advisor_rooms(brehon_root: Option<&Path>) -> Vec<AdvisorRoomFile> {
    let Some(brehon_root) = brehon_root else {
        return Vec::new();
    };
    let dir = brehon_root.join("runtime").join("advisors").join("rooms");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rooms = Vec::new();
    for entry in entries.flatten().take(32) {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(room) = serde_json::from_str::<AdvisorRoomFile>(&content) {
            rooms.push(room);
        }
    }
    rooms.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.room_id.cmp(&right.room_id))
    });
    rooms
}

fn advisor_room_path(brehon_root: &Path, room_id: &str) -> std::path::PathBuf {
    let file_name = format!("{}.json", sanitize_room_id(room_id));
    brehon_root
        .join("runtime")
        .join("advisors")
        .join("rooms")
        .join(file_name)
}

fn sanitize_room_id(room_id: &str) -> String {
    room_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn write_advisor_room_path(path: &Path, room: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create advisor room dir {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = serde_json::to_string_pretty(room)
        .map_err(|err| format!("failed to serialize advisor room: {err}"))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, payload).map_err(|err| {
        format!(
            "failed to write advisor room temp file {}: {err}",
            tmp.display()
        )
    })?;
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("failed to install advisor room {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

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
    fn read_advisor_rooms_ignores_missing_root() {
        assert!(read_advisor_rooms(None).is_empty());
    }

    #[test]
    fn advisor_room_count_reads_runtime_rooms() {
        let temp = tempfile::tempdir().unwrap();
        let rooms_dir = temp.path().join("runtime/advisors/rooms");
        std::fs::create_dir_all(&rooms_dir).unwrap();
        std::fs::write(
            rooms_dir.join("release-war-room.json"),
            r#"{
              "room_id": "release-war-room",
              "turn_mode": "open_chat",
              "participants": ["advisor-1"],
              "messages": [],
              "updated_at": "2026-05-17T00:00:00Z"
            }"#,
        )
        .unwrap();

        assert_eq!(advisor_room_count(Some(temp.path())), 1);
    }

    #[test]
    fn post_operator_message_creates_room_without_blocking() {
        let temp = tempfile::tempdir().unwrap();

        let seq = post_operator_advisor_message(
            temp.path(),
            "release-war-room",
            "What should we debate next?",
        )
        .unwrap();

        assert_eq!(seq, 1);
        assert_eq!(advisor_room_count(Some(temp.path())), 1);
        let room_id = active_advisor_room_id(Some(temp.path()));
        assert_eq!(room_id, "release-war-room");
    }

    #[test]
    fn advisor_message_block_wraps_long_content() {
        let message = AdvisorMessageFile {
            seq: 7,
            author: "fair-owl-61".to_string(),
            role: "advisor".to_string(),
            kind: "message".to_string(),
            content: "Real tension: it can look overreaching if Norma tries to be planner, workflow engine, review policy, and domain platform all at once.".to_string(),
            created_at: None,
        };

        let lines = advisor_message_block_lines(&message, 42);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(rendered.len() > 4);
        assert!(rendered[0].contains("#007 fair-owl-61 (advisor)"));
        assert!(rendered.iter().any(|line| line.contains("Real tension")));
        assert!(rendered.iter().any(|line| line.contains("workflow")));
        assert!(rendered.iter().any(|line| line.contains("domain")));
        assert!(rendered.iter().any(|line| line.contains("platform")));
        assert!(rendered.iter().all(|line| line.width() <= 42));
    }

    #[test]
    fn advisor_message_block_formats_markdown() {
        let message = AdvisorMessageFile {
            seq: 4,
            author: "soft-pig-81".to_string(),
            role: "advisor".to_string(),
            kind: "message".to_string(),
            content: "## Assessment: Legitimately Interesting\n\n**Through my correctness/safety lens:**\n\n1. **The problem is real.** Carrier operators need this.\n- Keep runtime boundaries hard.\n> Watch gateway coupling.\n```text\nmake test-e2e\n```".to_string(),
            created_at: None,
        };

        let lines = advisor_message_block_lines(&message, 72);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("Assessment: Legitimately Interesting"));
        assert!(rendered.contains("Through my correctness/safety lens:"));
        assert!(rendered.contains("1. The problem is real. Carrier operators need this."));
        assert!(rendered.contains("- Keep runtime boundaries hard."));
        assert!(rendered.contains("| Watch gateway coupling."));
        assert!(rendered.contains("make test-e2e"));
        assert!(!rendered.contains("##"));
        assert!(!rendered.contains("**"));
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn room_detail_renders_grouped_wrapped_thread() {
        let room = AdvisorRoomFile {
            room_id: "norma-war-room".to_string(),
            title: Some("Norma War Room".to_string()),
            turn_mode: "debate".to_string(),
            participants: vec!["fair-owl-61".to_string(), "deep-bee-91".to_string()],
            messages: vec![
                AdvisorMessageFile {
                    seq: 1,
                    author: "operator".to_string(),
                    role: "human".to_string(),
                    kind: "message".to_string(),
                    content: "What are your thoughts on Norma?".to_string(),
                    created_at: None,
                },
                AdvisorMessageFile {
                    seq: 2,
                    author: "deep-bee-91".to_string(),
                    role: "advisor".to_string(),
                    kind: "message".to_string(),
                    content: "From an architectural and maintainability perspective, Project Norma is legitimately interesting but needs a sharp boundary.".to_string(),
                    created_at: None,
                },
            ],
            updated_at: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
        let mut state = AdvisorRoomViewState::default();

        terminal
            .draw(|frame| render_room_detail(frame, Rect::new(0, 0, 64, 16), &room, &mut state))
            .unwrap();

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("#001 operator (human)"));
        assert!(rendered.contains("#002 deep-bee-91 (advisor)"));
        assert!(rendered.contains("From an architectural and maintainability"));
        assert!(rendered.contains("Project Norma is legitimately interesting"));
    }

    #[test]
    fn room_detail_scrolls_through_long_reply() {
        let content = format!(
            "Opening assessment.\n{}\nFinal conclusion: ambitious but tractable with hard boundaries.",
            (0..24)
                .map(|idx| format!("supporting detail line {idx}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let room = AdvisorRoomFile {
            room_id: "norma-war-room".to_string(),
            title: Some("Norma War Room".to_string()),
            turn_mode: "debate".to_string(),
            participants: vec!["soft-pig-81".to_string()],
            messages: vec![AdvisorMessageFile {
                seq: 4,
                author: "soft-pig-81".to_string(),
                role: "advisor".to_string(),
                kind: "message".to_string(),
                content,
                created_at: None,
            }],
            updated_at: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(72, 10)).unwrap();
        let mut state = AdvisorRoomViewState::default();

        terminal
            .draw(|frame| render_room_detail(frame, Rect::new(0, 0, 72, 10), &room, &mut state))
            .unwrap();
        let top = buffer_text(terminal.backend().buffer());
        assert!(state.max_scroll > 0);
        assert!(top.contains("Opening assessment"));
        assert!(!top.contains("Final conclusion"));

        state.scroll = state.max_scroll;
        terminal
            .draw(|frame| render_room_detail(frame, Rect::new(0, 0, 72, 10), &room, &mut state))
            .unwrap();
        let bottom = buffer_text(terminal.backend().buffer());
        assert!(bottom.contains("Final conclusion"));
    }
}
