//! Reusable TUI components.

use std::borrow::Cow;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::theme::{brand, chrome};

/// A rounded-border panel with an embedded title, optional subtitle,
/// optional right-aligned label, and focus-aware theming.
pub struct Panel<'a> {
    title: &'a str,
    title_line: Option<Line<'a>>,
    accent: Color,
    focused: bool,
    border: Color,
    focused_border: Color,
    focused_accent: Color,
    bg: Option<Color>,
    right_label: Option<Cow<'a, str>>,
    subtitle: Option<&'a str>,
    footer_label: Option<Cow<'a, str>>,
    footer_style: Option<Style>,
}

impl<'a> Panel<'a> {
    /// Create a new panel with the given title.
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            title_line: None,
            accent: chrome::BORDER,
            focused: false,
            border: chrome::BORDER,
            focused_border: chrome::BORDER_FOCUSED,
            focused_accent: brand::PRIMARY,
            bg: None,
            right_label: None,
            subtitle: None,
            footer_label: None,
            footer_style: None,
        }
    }

    /// Set a fully-styled title line embedded in the top rule.
    pub fn title_line(mut self, line: Line<'a>) -> Self {
        self.title_line = Some(line);
        self
    }

    /// Set the accent colour used for the title when unfocused.
    pub fn accent(mut self, color: Color) -> Self {
        self.accent = color;
        self
    }

    /// Set the border colour used when unfocused.
    pub fn border(mut self, color: Color) -> Self {
        self.border = color;
        self
    }

    /// Set the border colour used when focused.
    pub fn focused_border(mut self, color: Color) -> Self {
        self.focused_border = color;
        self
    }

    /// Set the accent colour used for the title when focused.
    pub fn focused_accent(mut self, color: Color) -> Self {
        self.focused_accent = color;
        self
    }

    /// Toggle the focused state.
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Set a background colour for the panel area.
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Set a right-aligned label drawn in the top rule.
    pub fn right_label(mut self, label: impl Into<Cow<'a, str>>) -> Self {
        let label = label.into();
        if !label.trim().is_empty() {
            self.right_label = Some(label);
        }
        self
    }

    /// Set a subtitle appended after the title in dim text.
    pub fn subtitle(mut self, text: &'a str) -> Self {
        self.subtitle = Some(text);
        self
    }

    /// Set a bottom-rule footer label embedded as `╰─ label ─╯`.
    pub fn footer_label(mut self, text: impl Into<Cow<'a, str>>) -> Self {
        self.footer_label = Some(text.into());
        self
    }

    /// Set the style used for the embedded footer label text.
    pub fn footer_style(mut self, style: Style) -> Self {
        self.footer_style = Some(style);
        self
    }

    /// Render the panel into `area` and return the inner `Rect` (after the
    /// one-cell border).
    pub fn render(self, frame: &mut Frame, area: Rect) -> Rect {
        let border_color = if self.focused {
            self.focused_border
        } else {
            self.border
        };

        let title_color = if self.focused {
            self.focused_accent
        } else {
            self.accent
        };

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // Guard against zero-width or zero-height areas.
        if area.width < 2 || area.height < 2 {
            return inner;
        }

        let buf = frame.buffer_mut();

        // Fill background if specified.
        if let Some(bg) = self.bg {
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    buf[(x, y)].set_bg(bg);
                }
            }
        }

        let mut border_style = Style::default().fg(border_color);
        if let Some(bg) = self.bg {
            border_style = border_style.bg(bg);
        }

        // ── Top rule ─────────────────────────────────────────────────────
        let top_y = area.top();
        let left_x = area.left();
        let right_x = area.right().saturating_sub(1);

        // Corners
        buf[(left_x, top_y)].set_symbol("╭").set_style(border_style);
        buf[(right_x, top_y)]
            .set_symbol("╮")
            .set_style(border_style);

        // Build the top text pieces
        let default_title_line = {
            let mut spans = Vec::new();
            if !self.title.is_empty() {
                spans.push(Span::styled(
                    format!(" {} ", self.title),
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(subtitle) = self.subtitle {
                spans.push(Span::styled(
                    format!("── {}", subtitle),
                    Style::default().fg(chrome::TEXT_DIM),
                ));
            }
            Line::from(spans)
        };
        let title_line = self.title_line.unwrap_or(default_title_line);
        let right_text = self
            .right_label
            .as_ref()
            .map(|l| format!(" {} ", l.as_ref()));

        let left_part_width = title_line.width();
        let right_width = right_text.as_ref().map(|s| s.width()).unwrap_or(0);
        let suffix_width = if right_width > 0 { right_width + 1 } else { 1 }; // +1 for the trailing ─ before ╮

        let available = (right_x.saturating_sub(left_x + 1)) as usize;
        let fill_width = available
            .saturating_sub(1)
            .saturating_sub(left_part_width)
            .saturating_sub(suffix_width);

        let mut x = left_x + 1;

        // Leading ─ before title
        buf[(x, top_y)].set_symbol("─").set_style(border_style);
        x += 1;

        // Title line
        if !title_line.spans.is_empty() {
            let line_style = if let Some(bg) = self.bg {
                title_line.style.bg(bg)
            } else {
                title_line.style
            };
            for span in &title_line.spans {
                if x >= right_x {
                    break;
                }
                let span_remaining = (right_x.saturating_sub(x)) as usize;
                let style = line_style.patch(span.style);
                x = buf
                    .set_stringn(x, top_y, span.content.as_ref(), span_remaining, style)
                    .0;
            }
        }

        // Fill
        if fill_width > 0 && x < right_x {
            let fill_remaining = (right_x.saturating_sub(x)) as usize;
            let fill = "─".repeat(fill_width.min(fill_remaining));
            x = buf
                .set_stringn(x, top_y, &fill, fill_remaining, border_style)
                .0;
        }

        // Right label
        if let Some(ref rt) = right_text {
            let rt_style = Style::default().fg(chrome::TEXT_DIM);
            let rt_remaining = (right_x.saturating_sub(x)) as usize;
            x = buf.set_stringn(x, top_y, rt, rt_remaining, rt_style).0;
        }

        // Trailing ─ before corner
        if x < right_x {
            buf[(x, top_y)].set_symbol("─").set_style(border_style);
        }

        if self.focused && right_x > left_x + 2 {
            let corner_gradient = brand::gradient(brand::PRIMARY_RGB, brand::SECONDARY_RGB, "╭──╮");
            let overlay_positions = [
                (left_x, "╭"),
                (left_x + 1, "─"),
                (right_x.saturating_sub(1), "─"),
                (right_x, "╮"),
            ];

            for ((cell_x, symbol), span) in overlay_positions.into_iter().zip(corner_gradient.spans)
            {
                let style = if let Some(bg) = self.bg {
                    span.style.bg(bg)
                } else {
                    span.style
                };
                buf[(cell_x, top_y)].set_symbol(symbol).set_style(style);
            }
        }

        // ── Vertical sides ───────────────────────────────────────────────
        for y in (area.top() + 1)..area.bottom().saturating_sub(1) {
            buf[(left_x, y)].set_symbol("│").set_style(border_style);
            buf[(right_x, y)].set_symbol("│").set_style(border_style);
        }

        // ── Bottom rule ──────────────────────────────────────────────────
        let bottom_y = area.bottom().saturating_sub(1);
        buf[(left_x, bottom_y)]
            .set_symbol("╰")
            .set_style(border_style);
        buf[(right_x, bottom_y)]
            .set_symbol("╯")
            .set_style(border_style);

        if let Some(label) = self.footer_label.as_ref() {
            let mut footer_x = left_x + 1;
            let footer_style = if let Some(bg) = self.bg {
                self.footer_style
                    .unwrap_or_else(|| Style::default().fg(chrome::FOOTER_LABEL))
                    .bg(bg)
            } else {
                self.footer_style
                    .unwrap_or_else(|| Style::default().fg(chrome::FOOTER_LABEL))
            };
            buf[(footer_x, bottom_y)]
                .set_symbol("─")
                .set_style(border_style);
            footer_x += 1;
            let footer_text = format!(" {} ", label.as_ref());
            let footer_remaining = (right_x.saturating_sub(footer_x)) as usize;
            footer_x = buf
                .set_stringn(
                    footer_x,
                    bottom_y,
                    &footer_text,
                    footer_remaining,
                    footer_style,
                )
                .0;
            for bx in footer_x..right_x {
                buf[(bx, bottom_y)].set_symbol("─").set_style(border_style);
            }
        } else {
            for bx in (left_x + 1)..right_x {
                buf[(bx, bottom_y)].set_symbol("─").set_style(border_style);
            }
        }

        inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn render_panel_builder(title: &str, focused: bool) -> String {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 6);
                Panel::new(title).focused(focused).render(frame, area);
            })
            .unwrap();
        buffer_to_string(terminal.backend().buffer())
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
        let mut lines = Vec::new();
        for row in 0..buffer.area().height {
            let mut line = String::new();
            for col in 0..buffer.area().width {
                line.push_str(buffer.cell((col, row)).unwrap().symbol());
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    fn snapshot_rows(buffer: &Buffer, rows: std::ops::Range<u16>) -> String {
        rows.map(|row| snapshot_row(buffer, row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn snapshot_row(buffer: &Buffer, row: u16) -> String {
        let Some(last_non_space) = (0..buffer.area.width).rev().find(|&x| {
            buffer
                .cell((x, row))
                .is_some_and(|cell| !cell.symbol().trim().is_empty())
        }) else {
            return "<blank>".to_string();
        };

        let mut out = String::new();
        let mut current: Option<(Color, String)> = None;

        for x in 0..=last_non_space {
            let cell = buffer.cell((x, row)).expect("cell");
            let fg = cell.fg;
            let symbol = normalize_cell_symbol(cell);
            match current.as_mut() {
                Some((current_fg, text)) if *current_fg == fg => text.push_str(&symbol),
                Some((current_fg, text)) => {
                    out.push_str(&format!("[{}]@{}", text, color_label(*current_fg)));
                    *current_fg = fg;
                    text.clear();
                    text.push_str(&symbol);
                }
                None => current = Some((fg, symbol)),
            }
        }

        if let Some((fg, text)) = current {
            out.push_str(&format!("[{}]@{}", text, color_label(fg)));
        }

        out
    }

    fn normalize_cell_symbol(cell: &Cell) -> String {
        let symbol = cell.symbol();
        if symbol.is_empty() {
            " ".to_string()
        } else {
            symbol.to_string()
        }
    }

    fn color_label(color: Color) -> String {
        match color {
            Color::Reset => "reset".to_string(),
            Color::Black => "black".to_string(),
            Color::Red => "red".to_string(),
            Color::Green => "green".to_string(),
            Color::Yellow => "yellow".to_string(),
            Color::Blue => "blue".to_string(),
            Color::Magenta => "magenta".to_string(),
            Color::Cyan => "cyan".to_string(),
            Color::Gray => "gray".to_string(),
            Color::DarkGray => "darkgray".to_string(),
            Color::LightRed => "lightred".to_string(),
            Color::LightGreen => "lightgreen".to_string(),
            Color::LightYellow => "lightyellow".to_string(),
            Color::LightBlue => "lightblue".to_string(),
            Color::LightMagenta => "lightmagenta".to_string(),
            Color::LightCyan => "lightcyan".to_string(),
            Color::White => "white".to_string(),
            Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
            Color::Indexed(idx) => format!("idx({idx})"),
        }
    }

    #[test]
    fn panel_default_embeds_title_in_top_rule_unfocused() {
        let panel = render_panel_builder("Factory Status", false);
        let top = panel.lines().next().unwrap_or_default();
        assert!(top.contains("╭─ Factory Status "));
        assert!(!top.contains("│ Factory Status │"));
    }

    #[test]
    fn panel_default_embeds_title_in_top_rule_focused() {
        let panel = render_panel_builder("Factory Status", true);
        let top = panel.lines().next().unwrap_or_default();
        assert!(top.contains("╭─ Factory Status "));
    }

    #[test]
    fn panel_focused_corner_gradient_accents_snapshot() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                Panel::new("Unfocused").render(frame, Rect::new(0, 0, 30, 6));
                Panel::new("Focused")
                    .focused(true)
                    .render(frame, Rect::new(0, 6, 30, 6));
            })
            .unwrap();

        let snapshot = snapshot_rows(terminal.backend().buffer(), 0..7);
        assert_snapshot!("panel_focused_corner_gradient_accents", snapshot);
    }

    #[test]
    fn panel_focused_uses_brand_primary() {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 6);
                Panel::new("Test").focused(true).render(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        // The title " Test " should be rendered with brand::PRIMARY fg
        let title_x = 2; // first title cell after "╭─"
        let cell = buffer.cell((title_x, 0)).unwrap();
        assert_eq!(cell.fg, brand::PRIMARY);
    }

    #[test]
    fn panel_unfocused_uses_accent_default() {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 6);
                Panel::new("Test").render(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let title_x = 2;
        let cell = buffer.cell((title_x, 0)).unwrap();
        assert_eq!(cell.fg, chrome::BORDER);
    }

    #[test]
    fn panel_custom_accent() {
        let custom = Color::Rgb(255, 0, 0);
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 6);
                Panel::new("Test").accent(custom).render(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();

        let title_x = 2;
        let cell = buffer.cell((title_x, 0)).unwrap();
        assert_eq!(cell.fg, custom);
    }

    #[test]
    fn panel_returns_inner_area() {
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut captured_inner = Rect::default();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 6);
                captured_inner = Panel::new("Test").render(frame, area);
            })
            .unwrap();
        assert_eq!(captured_inner, Rect::new(1, 1, 28, 4));
    }

    #[test]
    fn panel_with_subtitle_and_right_label_renders() {
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 40, 6);
                Panel::new("Title")
                    .subtitle("subtitle")
                    .right_label("label")
                    .render(frame, area);
            })
            .unwrap();
        let s = buffer_to_string(terminal.backend().buffer());
        assert!(s.contains("Title"));
        assert!(s.contains("subtitle"));
        assert!(s.contains("label"));
        // Verify border characters are present
        assert!(s.contains('╭'));
        assert!(s.contains('╮'));
        assert!(s.contains('╰'));
        assert!(s.contains('╯'));
    }

    #[test]
    fn panel_footer_label_embeds_text_in_bottom_rule() {
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 40, 6);
                Panel::new("Title")
                    .footer_label("running • ReadFile • ⏱  00:42")
                    .render(frame, area);
            })
            .unwrap();
        let s = buffer_to_string(terminal.backend().buffer());
        let bottom = s.lines().last().unwrap_or_default();
        assert!(bottom.contains("╰─ running • ReadFile • ⏱  00:42 "));
    }
}
