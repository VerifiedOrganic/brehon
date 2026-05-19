//! Layout computation and tab rendering.

use crate::theme::chrome::{BG, TEXT_DIM};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::types::*;

// ── Layout constants ───────────────────────────────────────────────────────

pub(crate) const SUPERVISOR_PCT: u16 = 40;
pub(crate) const TAB_ROW_HEIGHT: u16 = 1;
pub(crate) const SUB_TAB_HEIGHT: u16 = 3; // 3-row bar with underline
pub(crate) const STATUS_BAR_HEIGHT: u16 = 1;
pub(crate) const MIN_CONTENT_HEIGHT: u16 = 5;

pub(crate) fn calculate_layout(
    area: Rect,
    group_tab: GroupTab,
    has_reviewer_panels: bool,
) -> LayoutAreas {
    // How many rows the left tab section needs
    let tab_rows = match group_tab {
        GroupTab::Dashboard | GroupTab::Runtime | GroupTab::Advisors | GroupTab::Research => {
            TAB_ROW_HEIGHT
        } // just group bar, content fills rest
        GroupTab::Workers => TAB_ROW_HEIGHT + SUB_TAB_HEIGHT,
        GroupTab::Reviewers if has_reviewer_panels => {
            TAB_ROW_HEIGHT + SUB_TAB_HEIGHT + SUB_TAB_HEIGHT
        }
        GroupTab::Reviewers => TAB_ROW_HEIGHT + SUB_TAB_HEIGHT,
    };

    let vertical = Layout::vertical([
        Constraint::Length(tab_rows),
        Constraint::Min(MIN_CONTENT_HEIGHT),
        Constraint::Length(STATUS_BAR_HEIGHT),
    ])
    .split(area);

    let tab_stack_area = vertical[0];
    let content_row = vertical[1];

    // Horizontal split for tab stack
    let tab_horiz = Layout::horizontal([
        Constraint::Percentage(100 - SUPERVISOR_PCT),
        Constraint::Percentage(SUPERVISOR_PCT),
    ])
    .split(tab_stack_area);

    // Horizontal split for content
    let content_horiz = Layout::horizontal([
        Constraint::Percentage(100 - SUPERVISOR_PCT),
        Constraint::Percentage(SUPERVISOR_PCT),
    ])
    .split(content_row);

    // Group tab bar: first row of the left tab stack
    let group_tab_bar = Rect::new(
        tab_horiz[0].x,
        tab_horiz[0].y,
        tab_horiz[0].width,
        TAB_ROW_HEIGHT,
    );

    // Remaining left tab area (for sub-tabs and panel/member tabs)
    let left_tab_stack = Rect::new(
        tab_horiz[0].x,
        tab_horiz[0].y + TAB_ROW_HEIGHT,
        tab_horiz[0].width,
        tab_horiz[0].height - TAB_ROW_HEIGHT,
    );

    // Supervisor spans from right of tab stack through right of content
    let sup_y = tab_horiz[1].y;
    let sup_height = tab_horiz[1].height + content_horiz[1].height;
    let supervisor_area = Rect::new(tab_horiz[1].x, sup_y, tab_horiz[1].width, sup_height);

    LayoutAreas {
        group_tab_bar,
        left_tab_stack,
        left_content: content_horiz[0],
        supervisor_area,
        status_bar: vertical[2],
    }
}

pub(crate) fn calculate_host_owned_layout(
    area: Rect,
    group_tab: GroupTab,
    has_reviewer_panels: bool,
) -> LayoutAreas {
    let base = calculate_layout(area, group_tab, has_reviewer_panels);
    LayoutAreas {
        group_tab_bar: Rect::new(
            area.x,
            base.group_tab_bar.y,
            area.width,
            base.group_tab_bar.height,
        ),
        left_tab_stack: Rect::new(
            area.x,
            base.left_tab_stack.y,
            area.width,
            base.left_tab_stack.height,
        ),
        left_content: Rect::new(
            area.x,
            base.left_content.y,
            area.width,
            base.left_content.height,
        ),
        supervisor_area: Rect::new(
            area.x.saturating_add(area.width),
            base.supervisor_area.y,
            0,
            0,
        ),
        status_bar: base.status_bar,
    }
}

pub(crate) fn centered_dialog_rect(
    area: Rect,
    width_pct: u16,
    height_pct: u16,
    max_w: u16,
    max_h: u16,
) -> Rect {
    let dialog_width = ((area.width as u32 * width_pct as u32) / 100) as u16;
    let dialog_height = ((area.height as u32 * height_pct as u32) / 100) as u16;
    let dialog_width = dialog_width
        .max(40)
        .min(max_w)
        .min(area.width.saturating_sub(4));
    let dialog_height = dialog_height
        .max(12)
        .min(max_h)
        .min(area.height.saturating_sub(4));
    let dialog_x = area.x + (area.width.saturating_sub(dialog_width)) / 2;
    let dialog_y = area.y + (area.height.saturating_sub(dialog_height)) / 2;
    Rect::new(dialog_x, dialog_y, dialog_width, dialog_height)
}

pub(crate) fn expand_rect(area: Rect, bounds: Rect, horizontal: u16, vertical: u16) -> Rect {
    let left = area.x.saturating_sub(horizontal).max(bounds.x);
    let top = area.y.saturating_sub(vertical).max(bounds.y);
    let right = area
        .x
        .saturating_add(area.width)
        .saturating_add(horizontal)
        .min(bounds.x.saturating_add(bounds.width));
    let bottom = area
        .y
        .saturating_add(area.height)
        .saturating_add(vertical)
        .min(bounds.y.saturating_add(bounds.height));
    Rect::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    )
}

pub(crate) fn inset_rect(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    let doubled_horizontal = horizontal.saturating_mul(2);
    let doubled_vertical = vertical.saturating_mul(2);
    if area.width <= doubled_horizontal || area.height <= doubled_vertical {
        return area;
    }
    Rect::new(
        area.x.saturating_add(horizontal),
        area.y.saturating_add(vertical),
        area.width.saturating_sub(doubled_horizontal),
        area.height.saturating_sub(doubled_vertical),
    )
}

pub(crate) fn append_text_block(lines: &mut Vec<Line<'static>>, text: &str) {
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                line.to_string(),
                Style::default().fg(crate::theme::chrome::TEXT_BODY),
            ),
        ]));
    }
}

/// Dim horizontal rule used between sections in the detail dialog.
pub(crate) fn append_section_rule(lines: &mut Vec<Line<'static>>, width: usize) {
    let rule_width = width.min(60);
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(rule_width)),
        Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
    )));
}

pub(crate) fn append_section_heading(
    lines: &mut Vec<Line<'static>>,
    heading: &str,
    color: ratatui::style::Color,
) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            heading.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]));
    // Thin underline below heading
    let underline_len = heading.len() + 2;
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(underline_len)),
        Style::default().fg(crate::theme::chrome::RULE_SUBTLE),
    )));
}

pub(crate) fn append_bullet_section(
    lines: &mut Vec<Line<'static>>,
    heading: &str,
    items: &[String],
    color: ratatui::style::Color,
) {
    if items.is_empty() {
        return;
    }
    append_section_heading(lines, heading, color);
    for item in items {
        lines.push(Line::from(vec![
            Span::styled("  ▸ ", Style::default().fg(color)),
            Span::styled(
                item.clone(),
                Style::default().fg(crate::theme::chrome::TEXT_BODY),
            ),
        ]));
    }
}

// ── Rendering: group tab bar (1 row) ────────────────────────────────────────

pub(crate) fn render_group_tabs(
    frame: &mut Frame,
    area: Rect,
    active: GroupTab,
    worker_count: usize,
    reviewer_count: usize,
    advisor_count: usize,
    research_count: usize,
) -> Vec<ClickRegion> {
    let mut regions = Vec::new();
    let mut spans = Vec::new();

    let compact = area.width < 96;
    let dashboard_label = if compact { "Dash" } else { "Dashboard" };
    let runtime_label = "Runtime";
    let advisors_label = tab_count_label("Advisors", advisor_count, compact);
    let research_label = tab_count_label("Research", research_count, compact);
    let workers_label = tab_count_label(
        if compact { "Work" } else { "Workers" },
        worker_count,
        false,
    );
    let reviewers_label = tab_count_label(
        if compact { "Review" } else { "Reviewers" },
        reviewer_count,
        false,
    );

    spans.push(Span::styled("  ", Style::default().bg(BG)));

    // Dashboard group tab
    let dashboard_label = tab_label(dashboard_label, active == GroupTab::Dashboard);
    let dx = area.x + 2;
    let dw = dashboard_label.width as u16;
    spans.extend(dashboard_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(dx, area.y, dw, 1),
        target: ClickTarget::GroupTab(GroupTab::Dashboard),
    });

    spans.push(Span::styled("   ", Style::default().bg(BG)));

    // Runtime group tab
    let runtime_label = tab_label(runtime_label, active == GroupTab::Runtime);
    let tx = dx + dw + 3;
    let tw = runtime_label.width as u16;
    spans.extend(runtime_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(tx, area.y, tw, 1),
        target: ClickTarget::GroupTab(GroupTab::Runtime),
    });

    spans.push(Span::styled("   ", Style::default().bg(BG)));

    // Advisors group tab
    let advisors_label = tab_label(&advisors_label, active == GroupTab::Advisors);
    let ax = tx + tw + 3;
    let aw = advisors_label.width as u16;
    spans.extend(advisors_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(ax, area.y, aw, 1),
        target: ClickTarget::GroupTab(GroupTab::Advisors),
    });

    spans.push(Span::styled("   ", Style::default().bg(BG)));

    // Research group tab
    let research_label = tab_label(&research_label, active == GroupTab::Research);
    let rx = ax + aw + 3;
    let rw = research_label.width as u16;
    spans.extend(research_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(rx, area.y, rw, 1),
        target: ClickTarget::GroupTab(GroupTab::Research),
    });

    spans.push(Span::styled("   ", Style::default().bg(BG)));

    // Workers group tab
    let workers_label = tab_label(&workers_label, active == GroupTab::Workers);
    let wx = rx + rw + 3;
    let ww = workers_label.width as u16;
    spans.extend(workers_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(wx, area.y, ww, 1),
        target: ClickTarget::GroupTab(GroupTab::Workers),
    });

    spans.push(Span::styled("   ", Style::default().bg(BG)));

    // Reviewers group tab
    let reviewers_label = tab_label(&reviewers_label, active == GroupTab::Reviewers);
    let vx = wx + ww + 3;
    let vw = reviewers_label.width as u16;
    spans.extend(reviewers_label.spans);
    regions.push(ClickRegion {
        rect: Rect::new(vx, area.y, vw, 1),
        target: ClickTarget::GroupTab(GroupTab::Reviewers),
    });

    let used: usize = spans.iter().map(span_width).sum();
    let remaining = (area.width as usize).saturating_sub(used);
    if remaining > 0 {
        spans.push(Span::styled(" ".repeat(remaining), Style::default().bg(BG)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );
    regions
}

// ── Rendering: 3-row tab bar ────────────────────────────────────────────────

pub(crate) fn render_3row_tabs(
    frame: &mut Frame,
    area: Rect,
    tabs: &[TabEntry],
    click_target_fn: impl Fn(&str) -> ClickTarget,
) -> Vec<ClickRegion> {
    let mut regions = Vec::new();
    let total_width = area.width as usize;
    let click_height = area.height.min(SUB_TAB_HEIGHT);
    if total_width == 0 || area.height == 0 {
        return regions;
    }

    let visible = visible_tab_range(tabs, total_width);
    let visible_tabs = &tabs[visible.start..visible.end];
    let right_indicator_width = usize::from(visible.is_overflowing);
    let tab_click_limit = total_width.saturating_sub(right_indicator_width);

    // ── Row 1: labels ──
    let mut label_spans: Vec<Span> = Vec::new();
    let mut x_cursor: usize = TAB_LABEL_LEFT_PAD;
    if visible.is_overflowing {
        let left = if visible.has_left { "‹" } else { " " };
        label_spans.push(Span::styled(left, Style::default().fg(TEXT_DIM).bg(BG)));
        label_spans.push(Span::styled("  ", Style::default().bg(BG)));
        if visible.has_left && visible.start > 0 {
            regions.push(ClickRegion {
                rect: Rect::new(area.x, area.y, 1, click_height),
                target: click_target_fn(&tabs[visible.start - 1].id),
            });
        }
    } else {
        label_spans.push(Span::styled("   ", Style::default().bg(BG)));
    }

    for (i, tab) in visible_tabs.iter().enumerate() {
        let tab_x = x_cursor;
        let display = tab_label_nested(&tab.label, tab.is_selected);
        label_spans.extend(display.spans.clone());
        x_cursor += display.width;

        if tab_x < tab_click_limit {
            let click_width = display.width.min(tab_click_limit - tab_x);
            if click_width > 0 {
                regions.push(ClickRegion {
                    rect: Rect::new(
                        area.x + tab_x as u16,
                        area.y,
                        click_width as u16,
                        click_height,
                    ),
                    target: click_target_fn(&tab.id),
                });
            }
        }

        if i < visible_tabs.len() - 1 {
            label_spans.push(Span::styled("   ", Style::default().bg(BG)));
            x_cursor += 3;
        }
    }

    let label_used: usize = label_spans.iter().map(span_width).sum();
    let lr = total_width
        .saturating_sub(right_indicator_width)
        .saturating_sub(label_used);
    if lr > 0 {
        label_spans.push(Span::styled(" ".repeat(lr), Style::default().bg(BG)));
    }
    if visible.is_overflowing {
        let right = if visible.has_right { "›" } else { " " };
        label_spans.push(Span::styled(right, Style::default().fg(TEXT_DIM).bg(BG)));
        if visible.has_right && visible.end < tabs.len() {
            regions.push(ClickRegion {
                rect: Rect::new(
                    area.x + area.width.saturating_sub(1),
                    area.y,
                    1,
                    click_height,
                ),
                target: click_target_fn(&tabs[visible.end].id),
            });
        }
    }
    let label_line = Line::from(label_spans);

    // ── Row 0: thin dim rule ──
    // Drawn as a full-width divider so nested tab bars have a visible top
    // edge. Without this, stacking the group bar + sub-tabs + member-tabs
    // reads as one undifferentiated void.
    let top_line = Line::from(Span::styled(
        "─".repeat(total_width),
        Style::default()
            .fg(crate::theme::chrome::RULE_SUBTLE)
            .bg(BG),
    ));

    // ── Row 2: brand underline for the active tab ──
    let mut bspans: Vec<Span> = Vec::new();
    bspans.push(Span::styled("   ", Style::default().bg(BG)));
    for (i, tab) in visible_tabs.iter().enumerate() {
        if tab.is_selected {
            let display = tab_label_nested(&tab.label, true);
            let display_width = display.width;
            let left_pad = display_width.saturating_sub(3) / 2;
            let right_pad = display_width.saturating_sub(3 + left_pad);
            if left_pad > 0 {
                bspans.push(Span::styled(" ".repeat(left_pad), Style::default().bg(BG)));
            }
            bspans.extend(
                crate::theme::brand::gradient(
                    crate::theme::brand::PRIMARY_RGB,
                    crate::theme::brand::SECONDARY_RGB,
                    "━━━",
                )
                .spans,
            );
            if right_pad > 0 {
                bspans.push(Span::styled(" ".repeat(right_pad), Style::default().bg(BG)));
            }
        } else {
            let display = tab_label_nested(&tab.label, false);
            bspans.push(Span::styled(
                " ".repeat(display.width),
                Style::default().bg(BG),
            ));
        }
        if i < visible_tabs.len() - 1 {
            bspans.push(Span::styled("   ", Style::default().bg(BG)));
        }
    }
    let bu: usize = bspans.iter().map(span_width).sum();
    let br = total_width
        .saturating_sub(right_indicator_width)
        .saturating_sub(bu);
    if br > 0 {
        bspans.push(Span::styled(" ".repeat(br), Style::default().bg(BG)));
    }
    if visible.is_overflowing {
        bspans.push(Span::styled(" ", Style::default().bg(BG)));
    }
    let bottom_line = Line::from(bspans);

    frame.render_widget(
        Paragraph::new(vec![top_line, label_line, bottom_line]).style(Style::default().bg(BG)),
        area,
    );
    regions
}

const TAB_LABEL_LEFT_PAD: usize = 3;
const TAB_LABEL_GAP: usize = 3;
const TAB_OVERFLOW_INDICATOR_WIDTH: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleTabRange {
    start: usize,
    end: usize,
    has_left: bool,
    has_right: bool,
    is_overflowing: bool,
}

fn visible_tab_range(tabs: &[TabEntry], total_width: usize) -> VisibleTabRange {
    if tabs.is_empty() || total_width == 0 {
        return VisibleTabRange {
            start: 0,
            end: 0,
            has_left: false,
            has_right: false,
            is_overflowing: false,
        };
    }

    let full_width = TAB_LABEL_LEFT_PAD + tab_sequence_width(tabs);
    if full_width <= total_width {
        return VisibleTabRange {
            start: 0,
            end: tabs.len(),
            has_left: false,
            has_right: false,
            is_overflowing: false,
        };
    }

    let sequence_width = total_width
        .saturating_sub(TAB_LABEL_LEFT_PAD)
        .saturating_sub(TAB_OVERFLOW_INDICATOR_WIDTH);
    let selected = tabs.iter().position(|tab| tab.is_selected).unwrap_or(0);
    let (start, end) = visible_tab_window(tabs, selected, sequence_width);
    VisibleTabRange {
        start,
        end,
        has_left: start > 0,
        has_right: end < tabs.len(),
        is_overflowing: true,
    }
}

fn visible_tab_window(tabs: &[TabEntry], selected: usize, max_width: usize) -> (usize, usize) {
    let mut start = selected.min(tabs.len().saturating_sub(1));
    let mut end = start + 1;
    let mut used = nested_tab_width(&tabs[start]);
    if used > max_width {
        return (start, end);
    }

    let mut prefer_left = start > 0;
    loop {
        let mut grew = false;
        if prefer_left {
            grew |= try_grow_tab_window_left(tabs, &mut start, &mut used, max_width);
            grew |= try_grow_tab_window_right(tabs, &mut end, &mut used, max_width);
        } else {
            grew |= try_grow_tab_window_right(tabs, &mut end, &mut used, max_width);
            grew |= try_grow_tab_window_left(tabs, &mut start, &mut used, max_width);
        }
        if !grew {
            break;
        }
        prefer_left = !prefer_left;
    }

    (start, end)
}

fn try_grow_tab_window_left(
    tabs: &[TabEntry],
    start: &mut usize,
    used: &mut usize,
    max_width: usize,
) -> bool {
    if *start == 0 {
        return false;
    }
    let next_width = nested_tab_width(&tabs[*start - 1]);
    let candidate = *used + TAB_LABEL_GAP + next_width;
    if candidate > max_width {
        return false;
    }
    *start -= 1;
    *used = candidate;
    true
}

fn try_grow_tab_window_right(
    tabs: &[TabEntry],
    end: &mut usize,
    used: &mut usize,
    max_width: usize,
) -> bool {
    if *end >= tabs.len() {
        return false;
    }
    let next_width = nested_tab_width(&tabs[*end]);
    let candidate = *used + TAB_LABEL_GAP + next_width;
    if candidate > max_width {
        return false;
    }
    *end += 1;
    *used = candidate;
    true
}

fn tab_sequence_width(tabs: &[TabEntry]) -> usize {
    let labels: usize = tabs.iter().map(nested_tab_width).sum();
    let gaps = tabs.len().saturating_sub(1) * TAB_LABEL_GAP;
    labels + gaps
}

fn nested_tab_width(tab: &TabEntry) -> usize {
    tab_label_nested(&tab.label, tab.is_selected).width
}

fn span_width(span: &Span<'_>) -> usize {
    span.content.width()
}

fn tab_count_label(label: &str, count: usize, hide_zero: bool) -> String {
    if hide_zero && count == 0 {
        label.to_string()
    } else {
        format!("{label} ({count})")
    }
}

struct TabLabel {
    spans: Vec<Span<'static>>,
    width: usize,
}

fn tab_label(label: &str, selected: bool) -> TabLabel {
    let trimmed = label.trim();
    if selected {
        let diamond = format!("{} ", crate::theme::glyph::DIAMOND);
        let spans = vec![
            Span::styled(
                diamond,
                Style::default()
                    .fg(crate::theme::brand::PRIMARY)
                    .bg(BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                trimmed.to_string(),
                Style::default()
                    .fg(crate::theme::chrome::TEXT)
                    .bg(BG)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        let width = spans.iter().map(span_width).sum();
        TabLabel { spans, width }
    } else {
        let spans = vec![Span::styled(
            trimmed.to_string(),
            Style::default().fg(TEXT_DIM).bg(BG),
        )];
        let width = spans.iter().map(span_width).sum();
        TabLabel { spans, width }
    }
}

/// Label for nested 3-row tab bars (sub-tabs, panel tabs, member tabs).
///
/// The gradient underline in the row below carries the active-state cue, so
/// no diamond prefix is drawn here — stacking diamonds and underlines at
/// every nesting level reads as visual noise.
fn tab_label_nested(label: &str, selected: bool) -> TabLabel {
    let trimmed = label.trim();
    let style = if selected {
        Style::default()
            .fg(crate::theme::chrome::TEXT)
            .bg(BG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_DIM).bg(BG)
    };
    let spans = vec![Span::styled(trimmed.to_string(), style)];
    let width = spans.iter().map(span_width).sum();
    TabLabel { spans, width }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::style::Color;
    use ratatui::Terminal;
    use std::time::{Duration, Instant};

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
            Color::White => "white".to_string(),
            Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn group_tabs_snapshot_active_state_without_elapsed_badge() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let _ =
                    render_group_tabs(frame, Rect::new(0, 0, 80, 1), GroupTab::Workers, 2, 1, 0, 0);
            })
            .unwrap();

        assert_snapshot!(
            "group_tabs_active_state",
            snapshot_rows(terminal.backend().buffer(), 0..1)
        );
    }

    #[test]
    fn sub_tabs_snapshot_active_state_and_gradient_underline() {
        let backend = TestBackend::new(60, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let tabs = vec![
            TabEntry {
                id: "worker-a".to_string(),
                label: " worker-a ".to_string(),
                is_selected: true,
            },
            TabEntry {
                id: "worker-b".to_string(),
                label: " worker-b ".to_string(),
                is_selected: false,
            },
        ];

        terminal
            .draw(|frame| {
                let _ = render_3row_tabs(frame, Rect::new(0, 0, 60, 3), &tabs, |id| {
                    ClickTarget::SubTab(id.to_string())
                });
            })
            .unwrap();

        assert_snapshot!(
            "sub_tabs_active_state",
            snapshot_rows(terminal.backend().buffer(), 0..3)
        );
    }

    #[test]
    fn sub_tabs_overflow_keeps_selected_visible() {
        let tabs = (0..12)
            .map(|idx| TabEntry {
                id: format!("worker-{idx:02}"),
                label: format!(" worker-{idx:02} "),
                is_selected: idx == 6,
            })
            .collect::<Vec<_>>();

        let visible = visible_tab_range(&tabs, 50);
        assert_eq!(visible.start, 5);
        assert_eq!(visible.end, 9);
        assert!(visible.has_left);
        assert!(visible.has_right);

        let backend = TestBackend::new(50, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut regions = Vec::new();

        terminal
            .draw(|frame| {
                regions = render_3row_tabs(frame, Rect::new(0, 0, 50, 3), &tabs, |id| {
                    ClickTarget::SubTab(id.to_string())
                });
            })
            .unwrap();

        let rendered = snapshot_rows(terminal.backend().buffer(), 0..3);
        assert!(rendered.contains("‹"));
        assert!(rendered.contains("›"));
        assert!(!rendered.contains("worker-04"));
        assert!(rendered.contains("worker-05"));
        assert!(rendered.contains("worker-06"));
        assert!(rendered.contains("worker-08"));
        assert!(!rendered.contains("worker-09"));
        assert_eq!(regions.len(), 6);
        assert!(regions.iter().any(|region| {
            region.rect.x == 0
                && matches!(&region.target, ClickTarget::SubTab(id) if id == "worker-04")
        }));
        assert!(regions.iter().any(|region| {
            region.rect.x == 49
                && matches!(&region.target, ClickTarget::SubTab(id) if id == "worker-09")
        }));
    }

    #[test]
    fn elapsed_badge_matches_splash_format() {
        let badge = crate::theme::elapsed_badge(Instant::now() - Duration::from_secs(42));
        let content = badge.content.as_ref();
        assert!(matches!(content, "⏱  00:42" | "⏱  00:43"));
        assert_eq!(badge.style.fg, Some(crate::theme::chrome::TEXT_DIM));
    }
}
