use crate::error::{Error, Result};
use crate::pane::Pane;
use crate::pane::style::{cell_style_to_flags, convert_style_runs_to_proto, debug_log_enabled};
use brehon_protocol::{CacheRow, CursorPosition, RowData, TerminalCell, TerminalSnapshot};
use unicode_width::UnicodeWidthChar;

impl Pane {
    /// Get scrollback information from the terminal
    pub fn scrollback_info(&self) -> ghostty_vt::ScrollbackInfo {
        self.terminal.scrollback_info()
    }

    /// Get the current scroll offset (lines from bottom, 0 = at bottom)
    pub fn scroll_offset(&self) -> u32 {
        self.terminal.scrollback_info().viewport_offset
    }

    /// Get the total number of scrollback lines
    pub fn scrollback_lines(&self) -> u32 {
        self.terminal.scrollback_info().total_scrollback
    }

    /// Get a full terminal snapshot for the current viewport
    pub fn get_full_snapshot(&self) -> Result<TerminalSnapshot> {
        let mut cells = Vec::with_capacity((self.rows as usize) * (self.cols as usize));
        let viewport_rows = self.viewport_rows_for_display()?;

        for display_row in 0..self.rows {
            let row = viewport_rows.get(display_row as usize);
            let text = row.map(|row| row.text.as_str()).unwrap_or_default();
            let styles = row
                .map(|row| self.row_styles(row.source_row).unwrap_or_default())
                .unwrap_or_default();

            // Build column-aligned cells: wide chars occupy 2 columns, so we
            // advance by display width and fill continuation columns with space.
            let mut col = 0usize;
            for ch in text.chars() {
                let w = ch.width().unwrap_or(0).max(1);
                let style = styles.get(col).cloned().unwrap_or_default();
                cells.push(TerminalCell {
                    codepoint: ch as u32,
                    fg: (style.fg.r, style.fg.g, style.fg.b),
                    bg: (style.bg.r, style.bg.g, style.bg.b),
                    flags: cell_style_to_flags(&style),
                    width: w as u8,
                });
                col += 1;
                // Fill continuation columns for wide characters.
                for _ in 1..w {
                    if col >= self.cols as usize {
                        break;
                    }
                    let style = styles.get(col).cloned().unwrap_or_default();
                    cells.push(TerminalCell {
                        codepoint: ' ' as u32,
                        fg: (style.fg.r, style.fg.g, style.fg.b),
                        bg: (style.bg.r, style.bg.g, style.bg.b),
                        flags: cell_style_to_flags(&style),
                        width: 0,
                    });
                    col += 1;
                }
            }
            // Pad remaining columns with spaces.
            while col < self.cols as usize {
                let style = styles.get(col).cloned().unwrap_or_default();
                cells.push(TerminalCell {
                    codepoint: ' ' as u32,
                    fg: (style.fg.r, style.fg.g, style.fg.b),
                    bg: (style.bg.r, style.bg.g, style.bg.b),
                    flags: cell_style_to_flags(&style),
                    width: 1,
                });
                col += 1;
            }
        }

        let (cursor_col, cursor_row) = self
            .display_cursor_position()?
            .unwrap_or_else(|| self.cursor_position());
        Ok(TerminalSnapshot {
            cells,
            cursor: CursorPosition {
                x: cursor_col.saturating_sub(1),
                y: cursor_row.saturating_sub(1),
            },
            cols: self.cols,
            rows: self.rows,
        })
    }

    /// Get incremental row updates since the last call, if any rows are dirty.
    ///
    /// Returns `(dirty_rows, cursor_position, sequence_number)` or `None` if clean.
    #[allow(clippy::type_complexity)]
    pub fn get_incremental_update(
        &mut self,
    ) -> Result<Option<(Vec<RowData>, Option<CursorPosition>, u64)>> {
        let mut force_all = self.take_force_all_dirty();
        let scroll_info = self.terminal.scrollback_info();
        let viewport_rows = self.viewport_rows_for_display()?;
        let mut display_rows_by_source = vec![None; self.rows as usize];
        for (display_row, row) in viewport_rows.iter().enumerate() {
            if let Some(slot) = display_rows_by_source.get_mut(row.source_row as usize) {
                *slot = Some(display_row as u16);
            }
        }
        if scroll_info.total_scrollback != self.last_total_scrollback {
            if scroll_info.total_scrollback > self.last_total_scrollback {
                force_all = true;
            }
            self.last_total_scrollback = scroll_info.total_scrollback;
        }

        let dirty_rows = if force_all {
            if debug_log_enabled() {
                tracing::debug!(
                    "Pane {}: force_all_dirty, returning all {} rows",
                    self.id,
                    self.rows
                );
            }
            (0..self.rows).collect::<Vec<_>>()
        } else {
            let source_rows = self.terminal.take_dirty_rows(self.rows);
            if debug_log_enabled() && !source_rows.is_empty() {
                tracing::debug!(
                    "Pane {}: {} dirty rows from terminal",
                    self.id,
                    source_rows.len()
                );
            }

            let mut rows = Vec::with_capacity(source_rows.len());
            let mut needs_full_redraw = false;
            for source_row in source_rows {
                match display_rows_by_source
                    .get(source_row as usize)
                    .and_then(|display_row| *display_row)
                {
                    Some(display_row) => rows.push(display_row),
                    None => {
                        needs_full_redraw = true;
                        break;
                    }
                }
            }

            if needs_full_redraw {
                if debug_log_enabled() {
                    tracing::debug!(
                        "Pane {}: filtered chrome row changed, forcing full redraw",
                        self.id
                    );
                }
                (0..self.rows).collect::<Vec<_>>()
            } else {
                rows.sort_unstable();
                rows.dedup();
                rows
            }
        };

        if dirty_rows.is_empty() {
            return Ok(None);
        }

        let mut rows = Vec::with_capacity(dirty_rows.len());
        for display_row in dirty_rows {
            let (text, style_runs) = if let Some(row) = viewport_rows.get(display_row as usize) {
                let style_runs = self
                    .terminal
                    .row_style_runs(row.source_row)
                    .map_err(|e| Error::terminal(e.to_string()))?;
                (row.text.as_str(), style_runs)
            } else {
                ("", Vec::new())
            };

            let runs = convert_style_runs_to_proto(text, &style_runs, self.cols as usize);
            rows.push(RowData {
                row: display_row,
                runs,
            });
        }

        let (cursor_col, cursor_row) = self
            .display_cursor_position()?
            .unwrap_or_else(|| self.cursor_position());
        let cursor = Some(CursorPosition {
            x: cursor_col.saturating_sub(1),
            y: cursor_row.saturating_sub(1),
        });

        let seq = self.seq_counter;
        self.seq_counter = self.seq_counter.wrapping_add(1);

        Ok(Some((rows, cursor, seq)))
    }

    /// Create a full terminal snapshot with surrounding cache rows for smooth scrolling.
    pub fn create_snapshot_with_cache(
        &self,
        cache_window: u32,
    ) -> Result<(TerminalSnapshot, Vec<CacheRow>, Option<u32>)> {
        let snapshot = self.get_full_snapshot()?;

        if cache_window == 0 {
            return Ok((snapshot, Vec::new(), None));
        }

        let info = self.terminal.scrollback_info();
        let viewport_rows = info.viewport_rows as u32;
        let total = info.total_scrollback;

        let viewport_bottom = total.saturating_sub(info.viewport_offset);
        let viewport_top = viewport_bottom.saturating_sub(viewport_rows);

        let buffer = cache_window.min(viewport_rows * 2);
        let cache_start = viewport_top.saturating_sub(buffer);
        let cache_end = (viewport_bottom + buffer).min(total);

        let mut cache_rows = Vec::new();
        for screen_row in cache_start..cache_end {
            if screen_row >= viewport_top && screen_row < viewport_bottom {
                continue;
            }

            let text = self
                .terminal
                .dump_screen_row(screen_row)
                .unwrap_or_default();
            let style_runs = self
                .terminal
                .screen_row_style_runs(screen_row)
                .unwrap_or_default();

            let proto_runs = convert_style_runs_to_proto(&text, &style_runs, self.cols as usize);
            cache_rows.push(CacheRow {
                screen_row,
                text,
                style_runs: proto_runs,
            });
        }

        Ok((snapshot, cache_rows, Some(cache_start)))
    }

    /// Create a `RowData`-based snapshot with surrounding cache rows.
    pub fn create_snapshot_rows_with_cache(
        &self,
        cache_window: u32,
    ) -> Result<(Vec<RowData>, Vec<CacheRow>, Option<u32>)> {
        let info = self.terminal.scrollback_info();
        let viewport_rows = info.viewport_rows as u32;
        let total = info.total_scrollback;

        let rows = self.get_viewport_rows_data()?;

        if cache_window == 0 {
            return Ok((rows, Vec::new(), None));
        }

        let viewport_bottom = total.saturating_sub(info.viewport_offset);
        let viewport_top = viewport_bottom.saturating_sub(viewport_rows);

        let buffer = cache_window.min(viewport_rows * 2);
        let cache_start = viewport_top.saturating_sub(buffer);
        let cache_end = (viewport_bottom + buffer).min(total);

        let mut cache_rows = Vec::new();
        for screen_row in cache_start..cache_end {
            if screen_row >= viewport_top && screen_row < viewport_bottom {
                continue;
            }

            let text = self
                .terminal
                .dump_screen_row(screen_row)
                .unwrap_or_default();
            let style_runs = self
                .terminal
                .screen_row_style_runs(screen_row)
                .unwrap_or_default();

            let proto_runs = convert_style_runs_to_proto(&text, &style_runs, self.cols as usize);
            cache_rows.push(CacheRow {
                screen_row,
                text,
                style_runs: proto_runs,
            });
        }

        Ok((rows, cache_rows, Some(cache_start)))
    }

    /// Get styled `RowData` for all viewport rows.
    pub fn get_viewport_rows_data(&self) -> Result<Vec<RowData>> {
        let mut rows = Vec::with_capacity(self.rows as usize);
        let viewport_rows = self.viewport_rows_for_display()?;
        for display_row in 0..self.rows {
            let (text, style_runs) = if let Some(row) = viewport_rows.get(display_row as usize) {
                let style_runs = self
                    .terminal
                    .row_style_runs(row.source_row)
                    .map_err(|e| Error::terminal(e.to_string()))?;
                (row.text.as_str(), style_runs)
            } else {
                ("", Vec::new())
            };
            let runs = convert_style_runs_to_proto(text, &style_runs, self.cols as usize);
            rows.push(RowData {
                row: display_row,
                runs,
            });
        }
        Ok(rows)
    }
}
