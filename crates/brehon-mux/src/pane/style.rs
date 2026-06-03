use std::sync::OnceLock;

use brehon_protocol::{
    STYLE_BOLD, STYLE_FAINT, STYLE_INVERSE, STYLE_INVISIBLE, STYLE_ITALIC, STYLE_STRIKETHROUGH,
    STYLE_UNDERLINE, StyleRun as ProtoStyleRun,
};
use ghostty_vt::{CellStyle, Rgb, StyleRun};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

pub(crate) fn debug_log_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BREHON_MUX_DEBUG").is_some())
}

/// Convert ghostty_vt CellStyle flags to protocol flags
pub(crate) fn cell_style_to_flags(style: &CellStyle) -> u32 {
    let mut flags = 0u32;
    if style.bold {
        flags |= STYLE_BOLD;
    }
    if style.italic {
        flags |= STYLE_ITALIC;
    }
    if style.underline {
        flags |= STYLE_UNDERLINE;
    }
    if style.strikethrough {
        flags |= STYLE_STRIKETHROUGH;
    }
    if style.inverse {
        flags |= STYLE_INVERSE;
    }
    if style.invisible {
        flags |= STYLE_INVISIBLE;
    }
    if style.faint {
        flags |= STYLE_FAINT;
    }
    flags
}

/// Convert ghostty_vt StyleRuns to protocol StyleRuns
pub(crate) fn convert_style_runs_to_proto(
    text: &str,
    runs: &[StyleRun],
    total_cols: usize,
) -> Vec<ProtoStyleRun> {
    let mut proto_runs = Vec::with_capacity(runs.len());
    let mut rendered_len = 0usize;

    if text.is_ascii() {
        let bytes = text.as_bytes();
        for run in runs {
            let start = (run.start_col as usize).saturating_sub(1);
            let end = run.end_col as usize;

            if start >= bytes.len() {
                continue;
            }
            let end = end.min(bytes.len());

            let run_text = std::str::from_utf8(&bytes[start..end])
                .unwrap_or_default()
                .to_string();

            if run_text.is_empty() {
                continue;
            }

            let flags = cell_style_to_flags(&run.style);
            rendered_len = rendered_len.saturating_add(run_text.len());
            let fg = if is_default_color(&run.style.fg) {
                (0, 0, 0)
            } else {
                (run.style.fg.r, run.style.fg.g, run.style.fg.b)
            };
            let bg = (0, 0, 0);
            proto_runs.push(ProtoStyleRun {
                text: run_text,
                fg,
                bg,
                flags,
            });
        }
    } else {
        // Build a column→char-index mapping. ghostty_vt StyleRun columns are
        // terminal columns (wide chars occupy 2), but we need char indices to
        // slice the chars array.
        let chars: Vec<char> = text.chars().collect();
        let mut col_to_char: Vec<usize> = Vec::with_capacity(total_cols + 1);
        for (ci, &ch) in chars.iter().enumerate() {
            let w = ch.width().unwrap_or(0).max(1);
            for _ in 0..w {
                col_to_char.push(ci);
            }
        }
        // Sentinel: map any column beyond the text to chars.len()
        while col_to_char.len() <= total_cols {
            col_to_char.push(chars.len());
        }

        for run in runs {
            let start_col = (run.start_col as usize).saturating_sub(1);
            let end_col = run.end_col as usize;

            let ci_start = col_to_char.get(start_col).copied().unwrap_or(chars.len());
            let ci_end = col_to_char
                .get(end_col.min(col_to_char.len().saturating_sub(1)))
                .copied()
                .unwrap_or(chars.len());

            let run_text: String = chars
                .get(ci_start..ci_end.min(chars.len()))
                .map(|s| s.iter().collect())
                .unwrap_or_default();

            if run_text.is_empty() {
                continue;
            }

            let flags = cell_style_to_flags(&run.style);
            rendered_len = rendered_len.saturating_add(run_text.chars().count());
            let fg = if is_default_color(&run.style.fg) {
                (0, 0, 0)
            } else {
                (run.style.fg.r, run.style.fg.g, run.style.fg.b)
            };
            let bg = (0, 0, 0);
            proto_runs.push(ProtoStyleRun {
                text: run_text,
                fg,
                bg,
                flags,
            });
        }
    }

    if rendered_len < total_cols {
        let padding = " ".repeat(total_cols.saturating_sub(rendered_len));
        proto_runs.push(ProtoStyleRun {
            text: padding,
            fg: (0, 0, 0),
            bg: (0, 0, 0),
            flags: 0,
        });
    }

    proto_runs
}

/// Convert ghostty_vt CellStyle to ratatui Style
pub(crate) fn cell_style_to_ratatui(cell: &CellStyle) -> Style {
    let mut style = Style::default();

    if !is_default_color(&cell.fg) {
        style = style.fg(Color::Rgb(cell.fg.r, cell.fg.g, cell.fg.b));
    }
    if !is_default_color(&cell.bg) {
        style = style.bg(Color::Rgb(cell.bg.r, cell.bg.g, cell.bg.b));
    }

    let mut modifiers = Modifier::empty();
    if cell.bold {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.faint {
        modifiers |= Modifier::DIM;
    }
    if cell.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    if cell.inverse {
        modifiers |= Modifier::REVERSED;
    }
    if cell.invisible {
        modifiers |= Modifier::HIDDEN;
    }

    if !modifiers.is_empty() {
        style = style.add_modifier(modifiers);
    }

    style
}

/// Check if an RGB color is the default (0, 0, 0)
fn is_default_color(rgb: &Rgb) -> bool {
    rgb.r == 0 && rgb.g == 0 && rgb.b == 0
}

/// Check if two CellStyles are equal (for span grouping)
pub(crate) fn styles_equal(a: &CellStyle, b: &CellStyle) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.faint == b.faint
        && a.strikethrough == b.strikethrough
        && a.inverse == b.inverse
        && a.invisible == b.invisible
}
