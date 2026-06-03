use crate::harness::SupervisorCli;
use crate::pane::Pane;
use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color, widgets::Paragraph};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn test_supervisor_idle_filler_output_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(b"Waiting.\nStanding by.\n")
        .expect("append output");
    let row0 = pane.dump_row(0).expect("row 0");
    assert!(!row0.contains("Waiting."));
    assert!(!row0.contains("Standing by."));
}

#[test]
fn test_supervisor_real_output_survives_idle_filler_filter() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(b"Assigned worker-1.\nWaiting.\n")
        .expect("append output");
    let row0 = pane.dump_row(0).expect("row 0");
    let row1 = pane.dump_row(1).expect("row 1");

    assert!(row0.contains("Assigned worker-1."));
    assert!(!row0.contains("Waiting."));
    assert!(!row1.contains("Waiting."));
}

#[test]
fn test_empty_supervisor_prompt_is_ready_for_safe_inbox_nudge() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.set_inbox_nudge_not_before(None);
    pane.append_output("❯ ".as_bytes())
        .expect("append prompt line");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(60));

    assert!(
        pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_secs(5)),
        "unfocused quiet supervisor with empty prompt should be nudgeable"
    );

    pane.set_focused(true);
    assert!(
        !pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_secs(5)),
        "focused supervisor should never receive an automatic inbox nudge"
    );
}

#[test]
fn test_nonempty_supervisor_prompt_is_not_ready_for_inbox_nudge() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.set_inbox_nudge_not_before(None);
    pane.append_output("❯ draft message".as_bytes())
        .expect("append prompt line");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(60));

    assert!(
        !pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_secs(5)),
        "non-empty supervisor prompt should not be auto-nudged"
    );
    assert!(
        !pane.should_clear_pending_inbox_nudge_on_manual_input(b"\r"),
        "manual Enter should not consume queued inbox state from a non-empty prompt"
    );
}

#[test]
fn test_empty_supervisor_prompt_manual_enter_can_clear_pending_inbox_nudge() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.set_inbox_nudge_not_before(None);
    pane.set_pending_inbox_nudge(true);
    pane.append_output("❯ ".as_bytes())
        .expect("append empty prompt line");

    assert!(pane.should_clear_pending_inbox_nudge_on_manual_input(b"\r"));
}

/// Verifies the F7 generation-cache invariant: every parser feed,
/// resize, scroll, and explicit append must bump `render_generation`,
/// because the TUI widget keys its row cache on that counter and a
/// missed bump would render stale text after a state change.
#[test]
fn test_render_generation_bumps_on_visible_state_mutations() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-gen",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    let g0 = pane.render_generation();

    pane.feed(b"hello").expect("feed");
    let g1 = pane.render_generation();
    assert_ne!(g0, g1, "feed must bump generation");

    // Empty feed is a no-op and must not bump.
    pane.feed(b"").expect("empty feed");
    assert_eq!(g1, pane.render_generation(), "empty feed must not bump");

    pane.append_output(b"world\r\n").expect("append");
    let g2 = pane.render_generation();
    assert_ne!(g1, g2, "append_output must bump generation");

    pane.resize(20, 60).expect("resize");
    let g3 = pane.render_generation();
    assert_ne!(g2, g3, "resize must bump generation");

    // Need enough scrollback to actually move; push lines first.
    for i in 0..40 {
        pane.append_output(format!("line-{i}\r\n").as_bytes())
            .expect("append scroll line");
    }
    let g_pre_scroll = pane.render_generation();
    pane.scroll(-5).expect("scroll");
    let g_post_scroll = pane.render_generation();
    assert_ne!(
        g_pre_scroll, g_post_scroll,
        "scroll that moves the viewport must bump generation"
    );
}

#[test]
fn test_claude_supervisor_viewport_handles_status_redraws() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    // Feed two redraws with an erase-line in between. After F7 the
    // supervisor renders from ghostty_vt (same path as worker/reviewer
    // panes), so check that the visible viewport reflects the
    // post-redraw text.
    pane.feed(b"* Vibing... first\r\x1b[2K* Vibing... second")
        .expect("feed redraw");

    let contents = pane.dump_viewport().expect("dump viewport");
    assert!(!contents.contains("first"));
    assert!(contents.contains("second"));
}

#[test]
fn test_claude_supervisor_scrollback_tracks_display_scroll() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        6,
        40,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    for idx in 0..14 {
        pane.append_output(format!("line-{idx:02}\r\n").as_bytes())
            .expect("append output");
    }

    assert_eq!(pane.display_scroll_offset(), 0);
    let bottom_top = pane.dump_row(0).expect("bottom row");

    pane.scroll(-3).expect("scroll up");
    assert!(pane.display_scroll_offset() > 0);
    let scrolled_top = pane.dump_row(0).expect("scrolled row");
    assert_ne!(scrolled_top, bottom_top);

    pane.scroll_to_bottom().expect("scroll bottom");
    assert_eq!(pane.display_scroll_offset(), 0);
    assert_eq!(pane.dump_row(0).expect("bottom row restored"), bottom_top);

    pane.scroll_to_top().expect("scroll top");
    assert!(pane.display_scroll_offset() > 0);
}

#[test]
fn test_supervisor_empty_task_ready_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "⏺ brehon - task (MCP)(action:\n                    \"ready\")\n  ⎿  {\n       \"count\": 0,\n       \"tasks\": []\n     }\n\n⏺ Supervisor online.\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("\"count\": 0"));
    assert!(!viewport.contains("\"tasks\": []"));
    assert!(!viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_non_empty_task_ready_block_survives_filter() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "⏺ brehon - task (MCP)(action:\n                    \"ready\")\n  ⎿  {\n       \"count\": 1,\n       \"tasks\": [{\"task_id\":\"T-1\"}]\n     }\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("\"count\": 1"));
    assert!(viewport.contains("brehon - task (MCP)(action:"));
}

#[test]
fn test_supervisor_whoami_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "mcp__brehon__agent action=whoami returned:\n  ⎿ {\n      \"agent_name\": \"claude-code\",\n      \"role\": \"supervisor\",\n      \"session_id\": \"sess-1\",\n      \"supervisor\": null\n    }\n\n⏺ Supervisor online.\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("mcp__brehon__agent action=whoami"));
    assert!(!viewport.contains("\"session_id\": \"sess-1\""));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_split_whoami_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "mcp__brehon__agent action=whoami returned:\n  ⎿ {\n      \"agent_name\": \"claude-code\",\n"
                .as_bytes(),
        )
        .expect("append first chunk");

    let interim = pane.dump_viewport().expect("dump interim viewport");
    assert!(!interim.contains("mcp__brehon__agent action=whoami"));
    assert!(!interim.contains("\"agent_name\": \"claude-code\""));

    pane.append_output(
            "      \"role\": \"supervisor\",\n      \"session_id\": \"sess-1\",\n      \"supervisor\": null\n    }\n\nSupervisor online.\n"
                .as_bytes(),
        )
        .expect("append second chunk");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("mcp__brehon__agent action=whoami"));
    assert!(!viewport.contains("\"session_id\": \"sess-1\""));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_split_empty_task_ready_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
        "⏺ brehon - task (MCP)(action:\n                    \"ready\")\n  ⎿  {\n".as_bytes(),
    )
    .expect("append first chunk");

    let interim = pane.dump_viewport().expect("dump interim viewport");
    assert!(!interim.contains("brehon - task (MCP)(action:"));

    pane.append_output(
        "       \"count\": 0,\n       \"tasks\": []\n     }\n\nSupervisor online.\n".as_bytes(),
    )
    .expect("append second chunk");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("\"count\": 0"));
    assert!(!viewport.contains("\"tasks\": []"));
    assert!(!viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_split_non_empty_task_ready_block_survives_filter() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
        "⏺ brehon - task (MCP)(action:\n                    \"ready\")\n  ⎿  {\n".as_bytes(),
    )
    .expect("append first chunk");

    pane.append_output(
            "       \"count\": 1,\n       \"tasks\": [{\"task_id\":\"T-1\"}]\n     }\n\nSupervisor online.\n"
                .as_bytes(),
        )
        .expect("append second chunk");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("\"count\": 1"));
    assert!(viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_colored_empty_task_ready_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "\x1b[35m⏺ brehon - task (MCP)(action:\x1b[0m\n                    \"ready\")\n  ⎿  \x1b[2m{\x1b[0m\n       \"count\": 0,\n       \"tasks\": []\n     }\n\nSupervisor online.\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("\"count\": 0"));
    assert!(!viewport.contains("\"tasks\": []"));
    assert!(!viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_empty_epic_list_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "⏺ brehon - task (MCP)(action:\n                    \"list\")\n  ⎿  {\n       \"tasks\": [],\n       \"count\": 0,\n       \"filter\": {\n         \"task_type\": \"epic\",\n         \"status\": null,\n         \"include_closed\": false\n       }\n     }\n\nSupervisor online.\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("\"task_type\": \"epic\""));
    assert!(!viewport.contains("\"count\": 0"));
    assert!(!viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_supervisor_non_empty_epic_list_block_survives_filter() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "⏺ brehon - task (MCP)(action:\n                    \"list\")\n  ⎿  {\n       \"tasks\": [{\"task_id\":\"E-1\",\"task_type\":\"epic\"}],\n       \"count\": 1,\n       \"filter\": {\n         \"task_type\": \"epic\",\n         \"status\": null,\n         \"include_closed\": false\n       }\n     }\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("\"task_type\": \"epic\""));
    assert!(viewport.contains("\"count\": 1"));
    assert!(viewport.contains("brehon - task (MCP)(action:"));
}

#[test]
fn test_supervisor_split_empty_epic_list_block_is_suppressed() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "supervisor-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "⏺ brehon - task (MCP)(action:\n                    \"list\")\n  ⎿  {\n       \"tasks\": [],\n"
                .as_bytes(),
        )
        .expect("append first chunk");

    let interim = pane.dump_viewport().expect("dump interim viewport");
    assert!(!interim.contains("brehon - task (MCP)(action:"));

    pane.append_output(
            "       \"count\": 0,\n       \"filter\": {\n         \"task_type\": \"epic\",\n         \"status\": null,\n         \"include_closed\": false\n       }\n     }\n\nSupervisor online.\n"
                .as_bytes(),
        )
        .expect("append second chunk");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("\"task_type\": \"epic\""));
    assert!(!viewport.contains("\"count\": 0"));
    assert!(!viewport.contains("brehon - task (MCP)(action:"));
    assert!(viewport.contains("Supervisor online."));
}

#[test]
fn test_viewport_preserves_claude_inbox_and_footer_chrome() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
            "Real supervisor content.\r\n\
             \u{1b}[2minbox: queued message from review-coordinator; press Enter at an empty prompt to pick it up\u{1b}[0m\r\n\
             Press up to edit queued messages\r\n\
             \u{25b8} bypass permissions on (shift+tab to cycle)\r\n"
                .as_bytes(),
        )
        .expect("append output");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("Real supervisor content."));
    assert!(viewport.contains("queued message from review-coordinator"));
    assert!(viewport.contains("Press up to edit queued messages"));
    assert!(viewport.contains("bypass permissions on"));
}

#[test]
fn test_viewport_as_lines_preserves_styles_without_compacting_chrome_rows() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-supervisor",
        PathBuf::from("/tmp"),
        None,
        6,
        40,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(
        b"kept line\r\nPress up to edit queued messages\r\n\x1b[38;2;255;0;0mred line\x1b[0m",
    )
    .expect("append output");

    let lines = pane.viewport_as_lines().expect("viewport lines");
    let mut terminal = Terminal::new(TestBackend::new(40, 6)).expect("terminal");
    terminal
        .draw(|frame| {
            frame.render_widget(Paragraph::new(lines.clone()), Rect::new(0, 0, 40, 6));
        })
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let cell = buffer.cell((0, 2)).expect("styled cell");
    assert_eq!(cell.symbol(), "r");
    assert_eq!(cell.fg, Color::Rgb(255, 0, 0));
}

#[test]
fn test_display_cursor_position_preserves_source_rows() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-supervisor",
        PathBuf::from("/tmp"),
        None,
        6,
        40,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.append_output(b"kept line\r\nPress up to edit queued messages\r\ncursor test")
        .expect("append output");

    let (raw_col, raw_row) = pane.cursor_position();
    let (display_col, display_row) = pane
        .display_cursor_position()
        .expect("display cursor")
        .expect("visible cursor");

    assert_eq!(display_col, raw_col);
    assert_eq!(raw_row, 3);
    assert_eq!(display_row, 3);
}

#[test]
fn test_claude_prompt_marker_detection_requires_visible_prompt() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.set_inbox_nudge_not_before(None);
    pane.append_output(b"work in progress\r\n")
        .expect("append output");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(2));
    assert!(!pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_millis(800)));

    pane.append_output(b"\xe2\x9d\xaf \r\n")
        .expect("append prompt");
    pane.set_last_output_at(Instant::now() - Duration::from_secs(2));
    assert!(pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_millis(800)));
}

#[test]
fn test_claude_prompt_marker_detection_respects_quiet_period() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    pane.set_inbox_nudge_not_before(None);
    pane.append_output(b"\xe2\x9d\xaf \r\n")
        .expect("append prompt");
    pane.set_last_output_at(Instant::now());
    assert!(!pane.is_ready_for_inbox_nudge(Instant::now(), Duration::from_millis(800)));
}

#[test]
fn test_claude_inbox_nudge_waits_for_startup_settle_deadline() {
    let adapter = super::builtin(SupervisorCli::Claude);
    let worker_adapter = super::builtin(SupervisorCli::Claude);
    let mut pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        None,
    )
    .expect("create supervisor pane");

    let now = Instant::now();
    pane.append_output(b"\xe2\x9d\xaf \r\n")
        .expect("append prompt");
    pane.set_last_output_at(now - Duration::from_secs(2));
    pane.set_inbox_nudge_not_before(Some(now + Duration::from_secs(3)));
    assert!(!pane.is_ready_for_inbox_nudge(now, Duration::from_millis(800)));

    pane.set_inbox_nudge_not_before(Some(now - Duration::from_millis(1)));
    assert!(pane.is_ready_for_inbox_nudge(now, Duration::from_millis(800)));
}
