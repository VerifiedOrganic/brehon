//! Ratatui widget that paints an brehon_mux::Pane's viewport directly into
//! the frame buffer, walking the ghostty_vt cell grid one cell at a time.
//!
//! Replaces the previous two render paths (`Paragraph::new(viewport_as_lines)`
//! for worker/reviewer panes, `tui_term::PseudoTerminal` for supervisor) with
//! a single in-tree widget. Every PTY pane now travels the same code path,
//! so render bugs only have to be reasoned about — and fixed — once.
//!
//! Per the DIAGNOSIS § F7 (T2-1, T3-1, T3-2, T3-3): a terminal viewport is a
//! cell grid with per-cell attributes, not flowing paragraphs of text;
//! `Paragraph` was the wrong abstraction and `tui_term` was a third-party
//! mapping for a parser (vt100) we no longer use anywhere else in the
//! codebase.

use brehon_mux::Pane;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{StatefulWidget, Widget};
use unicode_width::UnicodeWidthChar;

/// A ratatui widget that renders a Pane's terminal viewport.
///
/// Borrows the pane immutably; the widget is constructed inline at render
/// time and consumed by `frame.render_widget` (or the stateful variant
/// `frame.render_stateful_widget` to wire in a `PaneRenderCache`).
pub(crate) struct PaneViewport<'a> {
    pane: &'a Pane,
    base_style: Style,
    cursor: Option<Style>,
}

/// Per-pane cache of viewport row data, keyed by `Pane::render_generation`.
///
/// Stored on the TUI event-loop side and threaded through
/// `frame.render_stateful_widget`. When the pane's generation hasn't
/// advanced since the last frame, the widget skips the per-row
/// `dump_row` + `row_styles` FFI calls and paints from the cache.
/// At an idle 50 ms tick across 6 panes × 60 rows, that's ~720 FFI
/// calls/sec the renderer no longer makes.
#[derive(Default)]
pub(crate) struct PaneRenderCache {
    last_generation: Option<u64>,
    cached_rows: u16,
    cached_cols: u16,
    rows: Vec<CachedRow>,
}

#[derive(Default, Clone)]
struct CachedRow {
    text: String,
    styles: Vec<brehon_mux::CellStyle>,
}

impl PaneRenderCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn ensure_capacity(&mut self, rows: u16, cols: u16) {
        if self.cached_rows != rows || self.cached_cols != cols {
            self.rows.clear();
            self.rows.resize_with(rows as usize, CachedRow::default);
            self.cached_rows = rows;
            self.cached_cols = cols;
            // Force a refetch since the cache no longer matches the pane.
            self.last_generation = None;
        } else if self.rows.len() < rows as usize {
            self.rows.resize_with(rows as usize, CachedRow::default);
        }
    }
}

impl<'a> PaneViewport<'a> {
    pub(crate) fn new(pane: &'a Pane, base_style: Style) -> Self {
        Self {
            pane,
            base_style,
            cursor: None,
        }
    }

    /// Overlay a cursor cell with the supplied style. The widget reads
    /// the cursor position from the parser, so this just toggles whether
    /// the highlight is applied (and with what colours).
    pub(crate) fn with_cursor(mut self, style: Style) -> Self {
        self.cursor = Some(style);
        self
    }
}

impl<'a> Widget for PaneViewport<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Cache-free path for callers that don't have a `PaneRenderCache`
        // to thread through. Keeps the FFI-per-frame cost the widget used
        // to have, but stays correct.
        let mut scratch = PaneRenderCache::new();
        StatefulWidget::render(self, area, buf, &mut scratch);
    }
}

impl<'a> StatefulWidget for PaneViewport<'a> {
    type State = PaneRenderCache;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let pane_rows = self.pane.rows();
        let pane_cols = self.pane.cols();
        let height = area.height.min(pane_rows);
        let width = area.width.min(pane_cols);

        state.ensure_capacity(pane_rows, pane_cols);

        let current_gen = self.pane.render_generation();
        let cache_hit = state.last_generation == Some(current_gen);
        if !cache_hit {
            // Pane state changed (or this is the first render). Re-fetch
            // every viewport row from ghostty_vt and stash for next frame.
            for row in 0..pane_rows {
                let slot = match state.rows.get_mut(row as usize) {
                    Some(slot) => slot,
                    None => continue,
                };
                slot.text = self.pane.dump_row(row).unwrap_or_default();
                slot.styles = self.pane.row_styles(row).unwrap_or_default();
            }
            state.last_generation = Some(current_gen);
        }

        for row in 0..height {
            let cached = match state.rows.get(row as usize) {
                Some(row) => row,
                None => continue,
            };
            paint_row(
                buf,
                area,
                row,
                width,
                &cached.text,
                &cached.styles,
                self.base_style,
            );
        }

        // Pad the host frame beyond the parser viewport (host area is
        // larger than pane content): fill with base style + space so the
        // diff doesn't carry over glyphs from a previous frame.
        if area.height > height || area.width > width {
            for y in 0..area.height {
                let start_x = if y < height { width } else { 0 };
                for x in start_x..area.width {
                    if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                        cell.set_symbol(" ");
                        cell.set_style(self.base_style);
                    }
                }
            }
        }

        if let Some(style) = self.cursor {
            if let Ok(Some((cursor_col, cursor_row))) = self.pane.display_cursor_position() {
                let cx = cursor_col.saturating_sub(1);
                let cy = cursor_row.saturating_sub(1);
                if cx < area.width && cy < area.height {
                    if let Some(cell) = buf.cell_mut((area.x + cx, area.y + cy)) {
                        if cell.symbol().is_empty() {
                            cell.set_symbol(" ");
                        }
                        cell.set_style(style);
                    }
                }
            }
        }
    }
}

fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    row: u16,
    max_cols: u16,
    text: &str,
    styles: &[brehon_mux::CellStyle],
    base_style: Style,
) {
    let mut col: u16 = 0;
    for ch in text.chars() {
        if col >= max_cols {
            break;
        }
        // Treat newline / carriage return defensively: dump_row should
        // already strip line terminators, but if anything slips through
        // we don't want to advance the column counter past the row.
        if ch == '\n' || ch == '\r' {
            continue;
        }
        let style = styles
            .get(col as usize)
            .map(|cs| cell_style_to_ratatui(cs, base_style))
            .unwrap_or(base_style);
        let width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;

        if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
            let mut sym_buf = [0u8; 4];
            let s = ch.encode_utf8(&mut sym_buf);
            cell.set_symbol(s);
            cell.set_style(style);
        }
        // Wide-char continuation: claim the next cell so the diff
        // doesn't leak through a stale glyph from a previous frame.
        if width == 2 && col + 1 < max_cols {
            if let Some(cell) = buf.cell_mut((area.x + col + 1, area.y + row)) {
                cell.set_symbol("");
                cell.set_style(style);
            }
        }
        col = col.saturating_add(width);
    }

    // Fill the remainder of the parser row with base style + space.
    // ghostty's dump_row already trims to logical content; without this
    // pad, ratatui's diff would re-show whatever was in those cells last
    // frame (the "double-write" symptom).
    while col < max_cols {
        if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
            cell.set_symbol(" ");
            cell.set_style(base_style);
        }
        col = col.saturating_add(1);
    }
}

fn cell_style_to_ratatui(cs: &brehon_mux::CellStyle, base_style: Style) -> Style {
    let mut style = base_style;
    // ghostty reports (0,0,0) when no explicit color was set; treat that
    // as "inherit from base" rather than literal black, which would
    // collapse to an unreadable dark-on-dark cell on dark themes.
    let fg = if cs.fg.r == 0 && cs.fg.g == 0 && cs.fg.b == 0 {
        None
    } else {
        Some(Color::Rgb(cs.fg.r, cs.fg.g, cs.fg.b))
    };
    let bg = if cs.bg.r == 0 && cs.bg.g == 0 && cs.bg.b == 0 {
        None
    } else {
        Some(Color::Rgb(cs.bg.r, cs.bg.g, cs.bg.b))
    };

    let (fg, bg) = if cs.inverse {
        (bg.or(Some(Color::Black)), fg.or(Some(Color::White)))
    } else {
        (fg, bg)
    };

    if let Some(c) = fg {
        style = style.fg(c);
    }
    if let Some(c) = bg {
        style = style.bg(c);
    }
    let mut mods = Modifier::empty();
    if cs.bold {
        mods |= Modifier::BOLD;
    }
    if cs.italic {
        mods |= Modifier::ITALIC;
    }
    if cs.underline {
        mods |= Modifier::UNDERLINED;
    }
    if cs.faint {
        mods |= Modifier::DIM;
    }
    if cs.invisible {
        mods |= Modifier::HIDDEN;
    }
    if cs.strikethrough {
        mods |= Modifier::CROSSED_OUT;
    }
    style.add_modifier(mods)
}
