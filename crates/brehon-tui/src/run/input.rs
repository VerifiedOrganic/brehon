//! Input handling: mouse events, keyboard dispatch, selection, clipboard, SGR mouse parsing.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use base64::{engine::general_purpose::STANDARD, Engine};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use panesmith::TerminalViewport;
use ratatui::layout::{Position, Rect};

use brehon_mux::Mux;

use super::types::*;

/// Map absolute screen coordinates to pane-relative coordinates,
/// accounting for the 1-cell border offset (Block with Borders::ALL).
pub(crate) fn screen_to_pane_pos(
    screen_col: u16,
    screen_row: u16,
    pane_area: Rect,
) -> Option<PanePos> {
    // Inner area is inset by 1 on each side for the border
    let inner_x = pane_area.x + 1;
    let inner_y = pane_area.y + 1;
    let inner_w = pane_area.width.saturating_sub(2);
    let inner_h = pane_area.height.saturating_sub(2);

    if screen_col < inner_x
        || screen_row < inner_y
        || screen_col >= inner_x + inner_w
        || screen_row >= inner_y + inner_h
    {
        return None;
    }

    Some(PanePos {
        col: screen_col - inner_x,
        row: screen_row - inner_y,
    })
}

/// Handle a left-click on a click region: focus pane, switch tabs, toggle epics.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_pane_click(
    click: Position,
    click_regions: &[ClickRegion],
    mux: &mut Mux,
    group_tab: &mut GroupTab,
    selected_worker: &mut usize,
    selected_panel: &mut usize,
    selected_member: &mut [usize],
    worker_ids: &[String],
    all_reviewer_ids: &[String],
    panels: &[ReviewerPanel],
    supervisor_id: &Option<String>,
    active_left_id: &Option<String>,
    expanded_epics: &mut std::collections::HashSet<String>,
    expanded_activity_rows: &mut std::collections::HashSet<(String, String)>,
    task_detail: &mut Option<TaskDetailState>,
    host_owned_terminal_tabs: bool,
    external_terminal_tab_request: &mut Option<String>,
    manual_reset_request: &mut Option<String>,
    runtime_approval_request: &mut Option<(String, String, bool)>,
) -> bool {
    let mut regions_stale = false;
    for region in click_regions {
        if region.rect.contains(click) {
            match &region.target {
                ClickTarget::GroupTab(tab) => {
                    regions_stale = true;
                    if host_owned_terminal_tabs {
                        if let Some(tab_name) = host_owned_group_tab_name(*tab) {
                            *external_terminal_tab_request = Some(tab_name.to_string());
                            break;
                        }
                    }
                    *group_tab = *tab;
                    match tab {
                        GroupTab::Dashboard
                        | GroupTab::Runtime
                        | GroupTab::Advisors
                        | GroupTab::Research => {}
                        GroupTab::Workers => {
                            if let Some(id) = worker_ids.get(*selected_worker) {
                                mux.focus(id);
                            }
                        }
                        GroupTab::Reviewers => {
                            super::reviewer_selection::focus_current_reviewer(
                                mux,
                                panels,
                                *selected_panel,
                                selected_member,
                            );
                        }
                    }
                }
                ClickTarget::SubTab(id) => {
                    regions_stale = true;
                    if let Some(idx) = worker_ids.iter().position(|w| w == id) {
                        *selected_worker = idx;
                        *group_tab = GroupTab::Workers;
                        mux.focus(id);
                    } else if let Some(idx) = panels.iter().position(|p| p.name == *id) {
                        *selected_panel = idx;
                        *group_tab = GroupTab::Reviewers;
                        super::reviewer_selection::focus_current_reviewer(
                            mux,
                            panels,
                            *selected_panel,
                            selected_member,
                        );
                    } else if all_reviewer_ids.contains(id) {
                        if let Some(idx) = all_reviewer_ids.iter().position(|r| r == id) {
                            if let Some(m) = selected_member.first_mut() {
                                *m = idx;
                            }
                        }
                        *group_tab = GroupTab::Reviewers;
                        mux.focus(id);
                    }
                }
                ClickTarget::MemberTab(pane_id) => {
                    regions_stale = true;
                    let current_panel_match = panels
                        .get(*selected_panel)
                        .and_then(|panel| panel.members.iter().position(|member| member == pane_id))
                        .map(|mi| (*selected_panel, mi));
                    let resolved = current_panel_match.or_else(|| {
                        panels.iter().enumerate().find_map(|(pi, panel)| {
                            panel
                                .members
                                .iter()
                                .position(|member| member == pane_id)
                                .map(|mi| (pi, mi))
                        })
                    });
                    if let Some((pi, mi)) = resolved {
                        *selected_panel = pi;
                        if let Some(sm) = selected_member.get_mut(pi) {
                            *sm = mi;
                        }
                    }
                    *group_tab = GroupTab::Reviewers;
                    mux.focus(pane_id);
                }
                ClickTarget::ResetPane(pane_id) => {
                    regions_stale = true;
                    mux.focus(pane_id);
                    *manual_reset_request = Some(pane_id.clone());
                }
                ClickTarget::SupervisorPane => {
                    if let Some(ref sup_id) = supervisor_id {
                        mux.focus(sup_id);
                    }
                }
                ClickTarget::LeftPane => {
                    if let Some(ref left_id) = active_left_id {
                        mux.focus(left_id);
                    }
                }
                ClickTarget::EpicToggle(epic_id) => {
                    regions_stale = true;
                    if !expanded_epics.remove(epic_id) {
                        expanded_epics.insert(epic_id.clone());
                    }
                }
                ClickTarget::TaskDetail(task_id) => {
                    regions_stale = true;
                    *group_tab = GroupTab::Dashboard;
                    *task_detail = Some(TaskDetailState::new(task_id.clone()));
                }
                ClickTarget::ActivityRow { pane_id, entry_key } => {
                    regions_stale = true;
                    mux.focus(pane_id);
                    let key = (pane_id.clone(), entry_key.clone());
                    if !expanded_activity_rows.remove(&key) {
                        expanded_activity_rows.insert(key);
                    }
                }
                ClickTarget::RuntimeApproval {
                    approval_id,
                    session_id,
                    approved,
                } => {
                    regions_stale = true;
                    *group_tab = GroupTab::Dashboard;
                    *runtime_approval_request =
                        Some((approval_id.clone(), session_id.clone(), *approved));
                }
            }
            break;
        }
    }
    regions_stale
}

fn host_owned_group_tab_name(tab: GroupTab) -> Option<&'static str> {
    match tab {
        GroupTab::Workers => Some("Workers"),
        GroupTab::Reviewers => Some("Reviewers"),
        GroupTab::Dashboard | GroupTab::Runtime | GroupTab::Advisors | GroupTab::Research => None,
    }
}

pub(crate) fn scroll_focused_to_bottom(
    mux: &mut Mux,
    structured_scroll_offsets: &mut HashMap<String, usize>,
) {
    if let Some(pane_id) = mux.focused_id().map(str::to_string) {
        structured_scroll_offsets.remove(&pane_id);
    }
    if let Some(focused) = mux.focused_mut() {
        let _ = focused.scroll_to_bottom();
    }
}

pub(crate) fn forward_input_bytes(
    mux: &mut Mux,
    rt: &tokio::runtime::Handle,
    selection: &mut Option<SelectionState>,
    bytes: &[u8],
) {
    *selection = None;
    if let Some(focused) = mux.focused_mut() {
        if focused.display_scroll_offset() > 0 {
            let _ = focused.scroll_to_bottom();
        }
    }
    mux.dispatch_input_focused(rt, bytes.to_vec());
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_mouse_input(
    mouse: MouseEvent,
    click_regions: &[ClickRegion],
    mux: &mut Mux,
    group_tab: &mut GroupTab,
    selected_worker: &mut usize,
    selected_panel: &mut usize,
    selected_member: &mut [usize],
    worker_ids: &[String],
    all_reviewer_ids: &[String],
    panels: &[ReviewerPanel],
    supervisor_id: &Option<String>,
    active_left_id: &Option<String>,
    expanded_epics: &mut std::collections::HashSet<String>,
    expanded_activity_rows: &mut std::collections::HashSet<(String, String)>,
    left_pane_area: Rect,
    supervisor_pane_area: Rect,
    selection: &mut Option<SelectionState>,
    pending_down: &mut Option<(SelectionPane, String, PanePos)>,
    task_detail: &mut Option<TaskDetailState>,
    advisor_room_view: &mut AdvisorRoomViewState,
    research_room_view: &mut ResearchRoomViewState,
    dashboard_agent_list: &mut DashboardAgentListState,
    dashboard_task_list: &mut DashboardTaskListState,
    structured_mode: &HashSet<String>,
    structured_scroll_offsets: &mut HashMap<String, usize>,
    host_owned_terminal_tabs: bool,
    external_terminal_tab_request: &mut Option<String>,
    manual_reset_request: &mut Option<String>,
    runtime_approval_request: &mut Option<(String, String, bool)>,
) -> bool {
    let mut regions_stale = false;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let click = Position::new(mouse.column, mouse.row);
            regions_stale = handle_pane_click(
                click,
                click_regions,
                mux,
                group_tab,
                selected_worker,
                selected_panel,
                selected_member,
                worker_ids,
                all_reviewer_ids,
                panels,
                supervisor_id,
                active_left_id,
                expanded_epics,
                expanded_activity_rows,
                task_detail,
                host_owned_terminal_tabs,
                external_terminal_tab_request,
                manual_reset_request,
                runtime_approval_request,
            );

            let mut started_selection = false;
            if let Some(pos) = screen_to_pane_pos(mouse.column, mouse.row, left_pane_area) {
                if let Some(ref left_id) = active_left_id {
                    let pane_ok = mux
                        .get(left_id)
                        .map(|p| !structured_mode.contains(left_id) || !p.is_gateway_backed())
                        .unwrap_or(true);
                    if pane_ok {
                        *pending_down = Some((SelectionPane::Left, left_id.clone(), pos));
                        started_selection = true;
                    }
                }
            } else if let Some(pos) =
                screen_to_pane_pos(mouse.column, mouse.row, supervisor_pane_area)
            {
                if let Some(ref sup_id) = supervisor_id {
                    let pane_ok = mux
                        .get(sup_id)
                        .map(|p| !structured_mode.contains(sup_id) || !p.is_gateway_backed())
                        .unwrap_or(true);
                    if pane_ok {
                        *pending_down = Some((SelectionPane::Supervisor, sup_id.clone(), pos));
                        started_selection = true;
                    }
                }
            }

            if started_selection {
                *selection = None;
            } else {
                *selection = None;
                *pending_down = None;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some((ref pane, ref pane_id, ref anchor)) = pending_down {
                let area = match pane {
                    SelectionPane::Left => left_pane_area,
                    SelectionPane::Supervisor => supervisor_pane_area,
                };
                let inner_w = area.width.saturating_sub(2);
                let inner_h = area.height.saturating_sub(2);
                let extent =
                    screen_to_pane_pos(mouse.column, mouse.row, area).unwrap_or_else(|| {
                        let clamped_col =
                            mouse.column.max(area.x + 1).min(area.x + inner_w) - (area.x + 1);
                        let clamped_row =
                            mouse.row.max(area.y + 1).min(area.y + inner_h) - (area.y + 1);
                        PanePos {
                            col: clamped_col.min(inner_w.saturating_sub(1)),
                            row: clamped_row.min(inner_h.saturating_sub(1)),
                        }
                    });
                *selection = Some(SelectionState {
                    pane: pane.clone(),
                    pane_id: pane_id.clone(),
                    anchor: anchor.clone(),
                    extent,
                });
                *pending_down = None;
            } else if let Some(ref mut sel) = selection {
                let area = match sel.pane {
                    SelectionPane::Left => left_pane_area,
                    SelectionPane::Supervisor => supervisor_pane_area,
                };
                let inner_w = area.width.saturating_sub(2);
                let inner_h = area.height.saturating_sub(2);
                sel.extent =
                    screen_to_pane_pos(mouse.column, mouse.row, area).unwrap_or_else(|| {
                        let clamped_col =
                            mouse.column.max(area.x + 1).min(area.x + inner_w) - (area.x + 1);
                        let clamped_row =
                            mouse.row.max(area.y + 1).min(area.y + inner_h) - (area.y + 1);
                        PanePos {
                            col: clamped_col.min(inner_w.saturating_sub(1)),
                            row: clamped_row.min(inner_h.saturating_sub(1)),
                        }
                    });
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(ref sel) = selection {
                let pane_ok = mux
                    .get(&sel.pane_id)
                    .map(|p| !structured_mode.contains(&sel.pane_id) || !p.is_gateway_backed())
                    .unwrap_or(true);
                if pane_ok {
                    let text = extract_selection_text(sel, mux);
                    if !text.is_empty() {
                        let mut stdout = io::stdout();
                        let _ = copy_to_clipboard_osc52(&text, &mut stdout);
                        tracing::debug!(
                            pane = %sel.pane_id,
                            len = text.len(),
                            "Copied selection to clipboard via OSC 52"
                        );
                    }
                }
            }
            *pending_down = None;
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                -3
            } else {
                3
            };
            let pos = Position::new(mouse.column, mouse.row);
            let mut scrolled = false;
            let mut scrolled_pane: Option<SelectionPane> = None;
            if task_detail.is_none() && *group_tab == GroupTab::Dashboard {
                if dashboard_agent_list.area.contains(pos) {
                    if delta < 0 {
                        dashboard_agent_list.scroll =
                            dashboard_agent_list.scroll.saturating_sub((-delta) as u16);
                    } else {
                        dashboard_agent_list.scroll = (dashboard_agent_list.scroll + delta as u16)
                            .min(dashboard_agent_list.max_scroll);
                    }
                    scrolled = true;
                } else if dashboard_task_list.area.contains(pos) {
                    if delta < 0 {
                        dashboard_task_list.scroll =
                            dashboard_task_list.scroll.saturating_sub((-delta) as u16);
                    } else {
                        dashboard_task_list.scroll = (dashboard_task_list.scroll + delta as u16)
                            .min(dashboard_task_list.max_scroll);
                    }
                    scrolled = true;
                }
            } else if task_detail.is_none()
                && *group_tab == GroupTab::Advisors
                && advisor_room_view.area.contains(pos)
            {
                if delta < 0 {
                    advisor_room_view.scroll =
                        advisor_room_view.scroll.saturating_sub((-delta) as u16);
                } else {
                    advisor_room_view.scroll =
                        (advisor_room_view.scroll + delta as u16).min(advisor_room_view.max_scroll);
                }
                scrolled = true;
            } else if task_detail.is_none()
                && *group_tab == GroupTab::Research
                && research_room_view.area.contains(pos)
            {
                if delta < 0 {
                    research_room_view.scroll =
                        research_room_view.scroll.saturating_sub((-delta) as u16);
                } else {
                    research_room_view.scroll = (research_room_view.scroll + delta as u16)
                        .min(research_room_view.max_scroll);
                }
                scrolled = true;
            }
            if !scrolled {
                if left_pane_area.contains(pos) {
                    if let Some(ref left_id) = active_left_id {
                        if scroll_pane_view(
                            mux,
                            left_id,
                            delta,
                            structured_mode,
                            structured_scroll_offsets,
                            pane_visible_rows(left_pane_area),
                        ) {
                            scrolled = true;
                            scrolled_pane = Some(SelectionPane::Left);
                        }
                    }
                } else if supervisor_pane_area.contains(pos) {
                    if let Some(ref sup_id) = supervisor_id {
                        if scroll_pane_view(
                            mux,
                            sup_id,
                            delta,
                            structured_mode,
                            structured_scroll_offsets,
                            pane_visible_rows(supervisor_pane_area),
                        ) {
                            scrolled = true;
                            scrolled_pane = Some(SelectionPane::Supervisor);
                        }
                    }
                }
            }
            if !scrolled {
                for region in click_regions {
                    if region.rect.contains(pos) {
                        match &region.target {
                            ClickTarget::SupervisorPane => {
                                if let Some(ref sup_id) = supervisor_id {
                                    if scroll_pane_view(
                                        mux,
                                        sup_id,
                                        delta,
                                        structured_mode,
                                        structured_scroll_offsets,
                                        pane_visible_rows(supervisor_pane_area),
                                    ) {
                                        scrolled = true;
                                        scrolled_pane = Some(SelectionPane::Supervisor);
                                    }
                                }
                            }
                            ClickTarget::LeftPane => {
                                if let Some(ref left_id) = active_left_id {
                                    if scroll_pane_view(
                                        mux,
                                        left_id,
                                        delta,
                                        structured_mode,
                                        structured_scroll_offsets,
                                        pane_visible_rows(left_pane_area),
                                    ) {
                                        scrolled = true;
                                        scrolled_pane = Some(SelectionPane::Left);
                                    }
                                }
                            }
                            _ => {}
                        }
                        if scrolled {
                            break;
                        }
                    }
                }
            }

            if !scrolled {
                if let Some(focused_id) = mux.focused_id().map(str::to_string) {
                    scroll_pane_view(
                        mux,
                        &focused_id,
                        delta,
                        structured_mode,
                        structured_scroll_offsets,
                        focused_pane_visible_rows(
                            &focused_id,
                            active_left_id,
                            supervisor_id,
                            left_pane_area,
                            supervisor_pane_area,
                        ),
                    );
                }
            }

            if let Some(ref sel) = selection {
                if scrolled_pane.as_ref().is_some_and(|pane| *pane == sel.pane) {
                    *selection = None;
                }
            }
        }
        _ => {}
    }
    regions_stale
}

fn scroll_pane_view(
    mux: &mut Mux,
    pane_id: &str,
    delta: i32,
    structured_mode: &HashSet<String>,
    structured_scroll_offsets: &mut HashMap<String, usize>,
    visible_rows: usize,
) -> bool {
    let is_structured_gateway = mux
        .get(pane_id)
        .map(|pane| structured_mode.contains(pane_id) && pane.is_gateway_backed())
        .unwrap_or(false);
    if mux.is_panesmith_managed(pane_id) {
        let _ = mux.refresh_panesmith_scrollback(pane_id);
        let Some(snapshot) = mux.panesmith_snapshot(pane_id) else {
            return false;
        };
        let scrollback = mux.panesmith_scrollback(pane_id);
        let current = structured_scroll_offsets
            .get(pane_id)
            .copied()
            .map(TerminalViewport::scrolled)
            .unwrap_or_default();
        let metrics = current.metrics(snapshot, scrollback, visible_rows.max(1));
        let next = if delta < 0 {
            current.scroll_up(delta.unsigned_abs() as usize, metrics)
        } else {
            current.scroll_down(delta as usize, metrics)
        };
        let next_metrics = next.metrics(snapshot, scrollback, visible_rows.max(1));
        let offset = next_metrics.effective_scroll_offset;
        if offset == 0 {
            structured_scroll_offsets.remove(pane_id);
            mux.clear_panesmith_scrollback(pane_id);
        } else {
            structured_scroll_offsets.insert(pane_id.to_string(), offset);
        }
        return true;
    }

    if is_structured_gateway {
        let offset = structured_scroll_offsets
            .entry(pane_id.to_string())
            .or_default();
        if delta < 0 {
            *offset = offset.saturating_add(delta.unsigned_abs() as usize);
        } else {
            *offset = offset.saturating_sub(delta as usize);
        }
        if *offset == 0 {
            structured_scroll_offsets.remove(pane_id);
        }
        return true;
    }

    if let Some(pane) = mux.get_mut(pane_id) {
        let _ = pane.scroll(delta);
        return true;
    }
    false
}

fn pane_visible_rows(area: Rect) -> usize {
    usize::from(area.height.saturating_sub(2))
}

fn focused_pane_visible_rows(
    focused_id: &str,
    active_left_id: &Option<String>,
    supervisor_id: &Option<String>,
    left_pane_area: Rect,
    supervisor_pane_area: Rect,
) -> usize {
    if active_left_id.as_deref() == Some(focused_id) {
        pane_visible_rows(left_pane_area)
    } else if supervisor_id.as_deref() == Some(focused_id) {
        pane_visible_rows(supervisor_pane_area)
    } else {
        1
    }
}

/// Extract selected text from the terminal pane buffer.
pub(crate) fn extract_selection_text(sel: &SelectionState, mux: &Mux) -> String {
    let pane = match mux.get(&sel.pane_id) {
        Some(p) => p,
        None => return String::new(),
    };
    let (start, end) = sel.ordered();
    let mut lines = Vec::new();

    for row in start.row..=end.row {
        let full_row = pane.dump_row(row).unwrap_or_default();
        let chars: Vec<char> = full_row.chars().collect();
        let c0 = if row == start.row {
            start.col as usize
        } else {
            0
        };
        let c1 = if row == end.row {
            (end.col as usize + 1).min(chars.len())
        } else {
            chars.len()
        };
        let slice: String = chars[c0.min(chars.len())..c1.min(chars.len())]
            .iter()
            .collect();
        lines.push(slice.trim_end().to_string());
    }

    // Remove trailing empty lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Copy text to the system clipboard via OSC 52 escape sequence.
pub(crate) fn copy_to_clipboard_osc52(text: &str, stdout: &mut io::Stdout) -> io::Result<()> {
    let encoded = STANDARD.encode(text);
    write!(stdout, "\x1b]52;c;{}\x07", encoded)?;
    stdout.flush()
}

pub(crate) fn parse_raw_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;

    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3, true) | (4, true) | (5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };

    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if cb & 0b0000_1000 != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    if cb & 0b0001_0000 != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }

    Some((kind, modifiers))
}

pub(crate) fn parse_raw_sgr_mouse_sequence(bytes: &[u8]) -> RawSgrMouseParse {
    if matches!(bytes, b"\x1b" | b"\x1b[" | b"\x1b[<") {
        return RawSgrMouseParse::Incomplete;
    }

    if !bytes.starts_with(b"\x1b[<") {
        return RawSgrMouseParse::Invalid;
    }

    if bytes[3..]
        .iter()
        .any(|byte| !matches!(*byte, b'0'..=b'9' | b';' | b'M' | b'm'))
    {
        return RawSgrMouseParse::Invalid;
    }

    let Some(&suffix) = bytes.last() else {
        return RawSgrMouseParse::Incomplete;
    };
    if suffix != b'M' && suffix != b'm' {
        return RawSgrMouseParse::Incomplete;
    }

    let Ok(body) = std::str::from_utf8(&bytes[3..bytes.len() - 1]) else {
        return RawSgrMouseParse::Invalid;
    };
    let mut split = body.split(';');

    let Some(cb) = split.next().and_then(|part| part.parse::<u8>().ok()) else {
        return RawSgrMouseParse::Invalid;
    };
    let Some(cx) = split.next().and_then(|part| part.parse::<u16>().ok()) else {
        return RawSgrMouseParse::Invalid;
    };
    let Some(cy) = split.next().and_then(|part| part.parse::<u16>().ok()) else {
        return RawSgrMouseParse::Invalid;
    };
    if split.next().is_some() || cx == 0 || cy == 0 {
        return RawSgrMouseParse::Invalid;
    }

    let Some((kind, modifiers)) = parse_raw_mouse_cb(cb) else {
        return RawSgrMouseParse::Invalid;
    };
    let kind = if suffix == b'm' {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    };

    RawSgrMouseParse::Complete(MouseEvent {
        kind,
        column: cx - 1,
        row: cy - 1,
        modifiers,
    })
}

pub(crate) fn key_to_plain_char(key: &KeyEvent) -> Option<char> {
    if !key.modifiers.is_empty() {
        return None;
    }

    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}
