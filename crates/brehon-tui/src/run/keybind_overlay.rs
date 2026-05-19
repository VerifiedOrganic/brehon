//! Centered keyboard-shortcut overlay rendered above the main TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::components::Panel;
use crate::theme::{brand, chrome};

use super::layout::{centered_dialog_rect, expand_rect, inset_rect};
use super::types::{InputMode, KeybindOverlayState};

const KEYBIND_ROWS: [(&str, &str, &str); 10] = [
    ("?", "Show keyboard help", "global"),
    ("C-d", "Open dashboard", "global"),
    ("C-o", "Open Brehon composer", "global"),
    ("C-r", "Open research rooms", "global"),
    ("C-w", "Focus workers", "global"),
    ("C-e", "Focus reviewers", "global"),
    ("C-s", "Focus supervisor", "global"),
    ("C-] / S-Tab", "Cycle tabs", "global"),
    ("↑ ↓ PgUp PgDn", "Scroll task list", "dashboard"),
    ("Esc", "Scroll focused pane to bottom", "pane"),
];

fn pad_cell(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.width());
    format!("{value}{}", " ".repeat(padding))
}

pub(crate) fn build_keybind_overlay_lines() -> Vec<Line<'static>> {
    let key_width = KEYBIND_ROWS
        .iter()
        .map(|(key, _, _)| key.width())
        .chain(std::iter::once("Key".width()))
        .max()
        .unwrap_or(0);
    let action_width = KEYBIND_ROWS
        .iter()
        .map(|(_, action, _)| action.width())
        .chain(std::iter::once("Action".width()))
        .max()
        .unwrap_or(0);
    let context_width = KEYBIND_ROWS
        .iter()
        .map(|(_, _, context)| context.width())
        .chain(std::iter::once("Context".width()))
        .max()
        .unwrap_or(0);

    let mut lines = Vec::with_capacity(KEYBIND_ROWS.len() + 3);
    lines.push(Line::from(vec![Span::styled(
        "Quick reference for the current workspace.",
        Style::default().fg(chrome::TEXT_SOFT),
    )]));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            pad_cell("Key", key_width),
            Style::default()
                .fg(brand::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pad_cell("Action", action_width),
            Style::default()
                .fg(chrome::TEXT_LABEL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pad_cell("Context", context_width),
            Style::default()
                .fg(chrome::TEXT_LABEL)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    for (key, action, context) in KEYBIND_ROWS {
        lines.push(Line::from(vec![
            Span::styled(
                pad_cell(key, key_width),
                Style::default()
                    .fg(chrome::PANEL_BORDER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                pad_cell(action, action_width),
                Style::default().fg(chrome::TEXT_SOFT),
            ),
            Span::raw("  "),
            Span::styled(
                pad_cell(context, context_width),
                Style::default().fg(chrome::TEXT_DIM),
            ),
        ]));
    }

    lines
}

pub(crate) fn handle_keybind_overlay_key_event(key: &KeyEvent, input_mode: &mut InputMode) -> bool {
    if matches!(input_mode, InputMode::KeybindOverlay(_)) {
        *input_mode = InputMode::Normal;
        return true;
    }

    if key.code == KeyCode::Char('?')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        *input_mode = InputMode::KeybindOverlay(KeybindOverlayState::default());
        return true;
    }

    false
}

pub(crate) fn handle_keybind_overlay_mouse_event(
    _mouse: MouseEvent,
    input_mode: &mut InputMode,
) -> bool {
    if matches!(input_mode, InputMode::KeybindOverlay(_)) {
        *input_mode = InputMode::Normal;
        true
    } else {
        false
    }
}

pub(crate) fn render_keybind_overlay(
    frame: &mut Frame,
    area: Rect,
    state: &mut KeybindOverlayState,
) {
    let dialog_area = centered_dialog_rect(area, 72, 72, 86, 15);
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
        Panel::new("Keyboard shortcuts")
            .accent(brand::PRIMARY)
            .border(chrome::PANEL_BORDER_ELEVATED)
            .bg(chrome::PANEL_BG_ELEVATED)
            .footer_label("press any key to close")
            .render(frame, dialog_area),
        1,
        1,
    );

    frame.render_widget(
        Paragraph::new(build_keybind_overlay_lines())
            .style(Style::default().bg(chrome::PANEL_BG_ELEVATED))
            .wrap(Wrap { trim: false }),
        inner,
    );
}
