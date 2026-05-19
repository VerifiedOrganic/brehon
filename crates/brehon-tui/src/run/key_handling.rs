//! Key event helpers: quit detection, key-to-bytes conversion, sub-tab cycling, and pane resize.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use brehon_mux::{Mux, PaneKind};

use super::layout::calculate_layout;
use super::types::{GroupTab, ReviewerPanel, TaskDetailState};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cycle_sub_tab(
    mux: &mut Mux,
    group_tab: GroupTab,
    worker_ids: &[String],
    panels: &[ReviewerPanel],
    selected_worker: &mut usize,
    selected_panel: &mut usize,
    selected_member: &mut [usize],
    direction: i32,
) {
    match group_tab {
        GroupTab::Workers if !worker_ids.is_empty() => {
            let n = worker_ids.len();
            *selected_worker =
                ((*selected_worker as i32 + direction).rem_euclid(n as i32)) as usize;
            mux.focus(&worker_ids[*selected_worker]);
        }
        GroupTab::Reviewers => {
            // Cycle members within the current panel
            if let Some(panel) = panels.get(*selected_panel) {
                if !panel.members.is_empty() {
                    let n = panel.members.len();
                    let mi = selected_member.get(*selected_panel).copied().unwrap_or(0);
                    let new_mi = ((mi as i32 + direction).rem_euclid(n as i32)) as usize;
                    if let Some(sm) = selected_member.get_mut(*selected_panel) {
                        *sm = new_mi;
                    }
                    mux.focus(&panel.members[new_mi]);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn is_quit_key(key: &KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }

    match key.code {
        KeyCode::Char(c) => matches!(c, 'q' | 'Q' | '\\'),
        _ => false,
    }
}

pub(crate) fn should_handle_key_event(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

pub(crate) fn focused_supervisor_captures_keyboard(
    mux: &Mux,
    task_detail: Option<&TaskDetailState>,
) -> bool {
    task_detail.is_none()
        && mux
            .focused()
            .is_some_and(|pane| *pane.kind() == PaneKind::Supervisor)
}

pub(crate) fn resize_panes(
    mux: &mut Mux,
    terminal_size: &Rect,
    worker_ids: &[String],
    reviewer_ids: &[String],
    advisor_ids: &[String],
    supervisor_id: &Option<String>,
    group_tab: GroupTab,
    has_panels: bool,
) {
    let areas = calculate_layout(*terminal_size, group_tab, has_panels);

    let left_inner_h = areas.left_content.height.saturating_sub(2);
    let left_inner_w = areas.left_content.width.saturating_sub(2);
    for id in worker_ids
        .iter()
        .chain(reviewer_ids.iter())
        .chain(advisor_ids.iter())
    {
        if let Some(pane) = mux.get_mut(id) {
            let _ = pane.resize(left_inner_h, left_inner_w);
        }
    }

    if let Some(ref sup_id) = supervisor_id {
        let sup_inner_h = areas.supervisor_area.height.saturating_sub(2);
        let sup_inner_w = areas.supervisor_area.width.saturating_sub(2);
        if let Some(pane) = mux.get_mut(sup_id) {
            let _ = pane.resize(sup_inner_h, sup_inner_w);
        }
    }
}

pub(crate) fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // xterm-style modifier encoding for arrow / Home / End / etc.:
    //   mod = 1 + shift + 2*alt + 4*ctrl
    // Used in `\x1b[1;{mod}{letter}` (cursor keys) and `\x1b[{n};{mod}~`
    // (function-shaped keys). Without this, modified arrow keys collapse
    // to the unmodified form and word-motion bindings in Ink composers
    // silently break.
    let xterm_mod = 1 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8);
    let modified_cursor = |letter: u8| -> Vec<u8> {
        if xterm_mod == 1 {
            vec![0x1b, b'[', letter]
        } else {
            format!("\x1b[1;{}{}", xterm_mod, letter as char).into_bytes()
        }
    };
    let modified_tilde = |n: u8| -> Vec<u8> {
        if xterm_mod == 1 {
            format!("\x1b[{}~", n).into_bytes()
        } else {
            format!("\x1b[{};{}~", n, xterm_mod).into_bytes()
        }
    };

    match key.code {
        // Shift+Enter: emit `\x1b\r` (Meta+Enter convention). Real
        // terminals (Ghostty, iTerm2, Alacritty default) send this for
        // Shift+Enter, and Ink composers bind it to "insert newline".
        // Plain Enter stays as bare `\r` (submit).
        KeyCode::Enter if shift => Some(vec![0x1b, b'\r']),
        KeyCode::Enter => Some(vec![b'\r']),

        // Ctrl+Shift+letter: encode via kitty CSI u so binds like
        // Ctrl+Shift+S stay distinguishable from Ctrl+S. Children that
        // don't request kitty kbd ignore unknown CSI u sequences
        // harmlessly. Plain Ctrl+letter keeps the legacy C0 encoding
        // (byte 1..=26) since every CLI handles that.
        KeyCode::Char(c) if ctrl && shift && c.is_ascii_alphabetic() => {
            // mod 6 = ctrl(4) + shift(1) + 1
            Some(format!("\x1b[{};6u", c.to_ascii_lowercase() as u32).into_bytes())
        }
        KeyCode::Char(c) if ctrl => {
            let b = c.to_ascii_lowercase() as u8;
            if b.is_ascii_lowercase() {
                Some(vec![b - b'a' + 1])
            } else {
                None
            }
        }
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            let mut buf = [0u8; 4];
            v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            Some(v)
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        // Shift+Tab is delivered as BackTab on most platforms.
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(modified_cursor(b'A')),
        KeyCode::Down => Some(modified_cursor(b'B')),
        KeyCode::Right => Some(modified_cursor(b'C')),
        KeyCode::Left => Some(modified_cursor(b'D')),
        KeyCode::Home => Some(modified_cursor(b'H')),
        KeyCode::End => Some(modified_cursor(b'F')),
        KeyCode::PageUp => Some(modified_tilde(5)),
        KeyCode::PageDown => Some(modified_tilde(6)),
        KeyCode::Delete => Some(modified_tilde(3)),
        KeyCode::Insert => Some(modified_tilde(2)),
        KeyCode::F(n @ 1..=12) => Some(
            match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            }
            .as_bytes()
            .to_vec(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn plain_enter_submits() {
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![b'\r'])
        );
    }

    #[test]
    fn shift_enter_emits_meta_cr_for_newline_insertion() {
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(vec![0x1b, b'\r'])
        );
    }

    #[test]
    fn unmodified_arrows_are_legacy_csi() {
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Left, KeyModifiers::NONE)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn modified_arrows_use_xterm_csi_with_modifier() {
        // mod 3 = 1 + alt(2)
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Right, KeyModifiers::ALT)),
            Some(b"\x1b[1;3C".to_vec())
        );
        // mod 5 = 1 + ctrl(4)
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Left, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5D".to_vec())
        );
        // mod 6 = 1 + ctrl(4) + shift(1)
        assert_eq!(
            key_to_bytes(&ev(
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(b"\x1b[1;6A".to_vec())
        );
    }

    #[test]
    fn ctrl_shift_letter_uses_kitty_csi_u() {
        // 's' = 0x73 = 115
        assert_eq!(
            key_to_bytes(&ev(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(b"\x1b[115;6u".to_vec())
        );
        // Uppercase from a SHIFTed key still lowercases to the kitty
        // base code (terminals normalise the keysym, not the glyph).
        assert_eq!(
            key_to_bytes(&ev(
                KeyCode::Char('S'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(b"\x1b[115;6u".to_vec())
        );
    }

    #[test]
    fn ctrl_letter_keeps_legacy_c0_encoding() {
        // Ctrl+C -> 0x03
        assert_eq!(
            key_to_bytes(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn backtab_emits_reverse_tab_csi() {
        assert_eq!(
            key_to_bytes(&ev(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
    }
}
