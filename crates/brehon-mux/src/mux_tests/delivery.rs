use crate::mux::*;
use crate::teams::{TeamsManager, TeamsPaths};
use crate::{AgentAdapter, Pane, SupervisorCli};
use brehon_pty::{Pty, PtyConfig};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct InfoLogCapture(Arc<Mutex<Vec<u8>>>);

struct InfoLogCaptureWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for InfoLogCapture {
    type Writer = InfoLogCaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        InfoLogCaptureWriter(self.0.clone())
    }
}

impl io::Write for InfoLogCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log capture mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_info_logs(run: impl FnOnce()) -> String {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .with_ansi(false)
        .with_writer(InfoLogCapture(captured.clone()))
        .with_max_level(tracing::Level::INFO)
        .finish();

    tracing::subscriber::with_default(subscriber, run);

    String::from_utf8(captured.lock().expect("log capture mutex poisoned").clone())
        .expect("captured logs should be utf-8")
}

fn panesmith_snapshot_text(snapshot: &::panesmith::OwnedPaneSnapshot) -> String {
    snapshot
        .surface
        .rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.text.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_flush_pending_inbox_nudges_waits_for_empty_claude_prompt() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(mux.deliver_prompt("claude-code", "review complete", None))
        .expect("deliver prompt");

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.append_output(b"\xe2\x9d\xaf draft message\r\n")
            .expect("append draft prompt");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(2));
        pane.set_focused(false);
    }

    mux.flush_pending_inbox_nudges(rt.handle());
    assert!(
        mux.get("claude-code")
            .expect("pane exists")
            .pending_inbox_nudge()
    );

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.append_output(b"\xe2\x9d\xaf \r\n")
            .expect("append empty prompt");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(2));
        pane.set_focused(false);
    }

    mux.flush_pending_inbox_nudges(rt.handle());

    // Wait for the non-blocking PTY nudge task to complete.
    for _ in 0..50 {
        if !mux
            .get("claude-code")
            .expect("pane exists")
            .pending_inbox_nudge()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        !mux.get("claude-code")
            .expect("pane exists")
            .pending_inbox_nudge()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_flush_pending_inbox_nudges_does_not_inject_claude_mcp_manager() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "reviewer-1",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("reviewer-1", config).expect("spawn test pty");
    let pane = Pane::with_pty_cli(
        "reviewer-1",
        PaneKind::Reviewer,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create reviewer pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("reviewer-1").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(mux.deliver_prompt("reviewer-1", "review complete", None))
        .expect("deliver prompt");

    {
        let pane = mux.get_mut("reviewer-1").expect("pane exists");
        pane.append_output(b"\xe2\x9d\xaf \r\n")
            .expect("append empty prompt");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(2));
        pane.set_focused(false);
    }

    mux.flush_pending_inbox_nudges(rt.handle());

    // Retry briefly to allow the non-blocking PTY nudge task to complete.
    for _ in 0..50 {
        if !mux
            .get("reviewer-1")
            .expect("pane exists")
            .pending_inbox_nudge()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = mux.poll_batch();

    let pane = mux.get("reviewer-1").expect("pane exists");
    assert!(!pane.pending_inbox_nudge());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("/mcp"),
        "unexpected MCP manager command in viewport: {viewport}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// Build a Claude supervisor pane wired to a real PTY (`sh -c cat`) for
/// recovery-path tests. The PTY simply echoes whatever is written to it,
/// which lets us verify the recovery state machine's keystroke choices via
/// the pane's terminal buffer.
fn setup_claude_supervisor(home_label: &str) -> (Mux, PathBuf, tokio::runtime::Runtime) {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!(
        "brehon-mux-home-{}-{}",
        home_label,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("claude-code", config).expect("spawn test supervisor pty");
    let pane = Pane::with_pty_cli(
        "claude-code",
        PaneKind::Supervisor,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    (mux, home, rt)
}

#[test]
fn test_flush_pending_inbox_nudges_escalates_stuck_supervisor_draft() {
    let (mut mux, home, rt) = setup_claude_supervisor("escalate-draft");

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    rt.block_on(mux.deliver_prompt("claude-code", "review complete", None))
        .expect("deliver prompt");

    // Stage 1: stale Draft state. Recovery should send Ctrl-C to clear the
    // draft, leave `pending_inbox_nudge` set (we have not yet submitted the
    // inbox nudge), and arm a cooldown so the very next tick cannot
    // re-fire while Claude is still redrawing.
    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.append_output(b"\xe2\x9d\xaf draft message\r\n")
            .expect("append draft prompt");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(10));
        pane.set_pending_inbox_nudge_since(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(20),
        ));
        pane.set_focused(false);
    }

    mux.flush_pending_inbox_nudges(rt.handle());

    {
        let pane = mux.get("claude-code").expect("pane exists");
        assert!(
            pane.pending_inbox_nudge(),
            "first tick should NOT yet clear the flag — draft must be cleared first"
        );
        assert!(
            pane.inbox_nudge_not_before().is_some(),
            "first tick should arm cooldown so recovery cannot re-fire immediately"
        );
    }

    // Stage 2: simulate Claude redrawing an empty prompt after Ctrl-C, then
    // bypass the cooldown to drive the next tick. Recovery should now
    // observe Empty state, send a plain Enter inbox nudge, and clear the
    // pending flag without typing synthetic prompt text into the input box.
    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        // Push the existing draft row out of view by writing a screenful of
        // newlines, then place an empty Claude prompt marker as the new last
        // visible row.
        let mut redraw = Vec::new();
        for _ in 0..30 {
            redraw.extend_from_slice(b"\r\n");
        }
        redraw.extend_from_slice(b"\xe2\x9d\xaf \r\n");
        pane.append_output(&redraw).expect("append empty prompt");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(10));
    }

    mux.flush_pending_inbox_nudges(rt.handle());

    assert!(
        !mux.get("claude-code")
            .expect("pane exists")
            .pending_inbox_nudge(),
        "after Empty state observed, recovery should nudge and clear the flag"
    );
    let viewport = mux
        .get("claude-code")
        .expect("pane exists")
        .dump_viewport()
        .expect("dump viewport");
    assert!(
        !viewport.contains("Check your unread inbox"),
        "recovery must not type synthetic inbox prompts into Claude input"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_force_supervisor_inbox_recovery_arms_cooldown_on_every_pass() {
    // Regression guard for the "messages stack up unsent in Claude's input
    // box" bug: even when the recovery cannot deliver the prompt this tick,
    // it MUST set `inbox_nudge_not_before` so the loop cannot retype the
    // recovery control sequence into a non-empty Ink buffer five times in a row.
    let (mut mux, home, rt) = setup_claude_supervisor("cooldown");

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
        pane.append_output(b"\xe2\x9d\xaf already-typed draft\r\n")
            .expect("append draft");
        pane.set_pending_inbox_nudge(true);
    }

    rt.block_on(mux.force_supervisor_inbox_recovery("claude-code"));

    let pane = mux.get("claude-code").expect("pane exists");
    assert!(
        pane.inbox_nudge_not_before().is_some(),
        "recovery must arm cooldown to prevent tight retry stacking unsent text"
    );
    assert!(
        pane.pending_inbox_nudge(),
        "Draft branch must NOT clear the flag — message has not been submitted yet"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_force_supervisor_inbox_recovery_sends_enter_not_prompt_text_when_empty() {
    let (mut mux, home, rt) = setup_claude_supervisor("empty-nudge");

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
        pane.append_output(b"\xe2\x9d\xaf \r\n")
            .expect("append empty prompt");
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(10));
        pane.set_pending_inbox_nudge(true);
        pane.set_pending_inbox_nudge_since(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(20),
        ));
        pane.set_focused(false);
    }

    rt.block_on(mux.force_supervisor_inbox_recovery("claude-code"));

    let pane = mux.get("claude-code").expect("pane exists");
    assert!(
        !pane.pending_inbox_nudge(),
        "empty prompt recovery should consume the pending inbox nudge"
    );
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("Check your unread inbox"),
        "empty prompt recovery must only send Enter, never type recovery text"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_panesmith_teams_inbox_nudge_uses_snapshot_prompt_state() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-teams-nudge");
    let mut mux = Mux::factory(MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["claude-reviewer".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    })
    .expect("create mux");
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    mux.set_teams(TeamsManager::new_for_test("test-session", home.clone()));
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    assert!(mux.is_panesmith_managed("claude-reviewer"));
    rt.block_on(mux.send_input_to("claude-reviewer", b"\xe2\x9d\xaf \r\n"))
        .expect("seed empty prompt marker through Panesmith");

    let mut saw_empty_prompt = false;
    for _ in 0..50 {
        let (_bytes, _events) = mux.poll_batch();
        saw_empty_prompt = mux
            .panesmith_snapshot("claude-reviewer")
            .map(panesmith_snapshot_text)
            .is_some_and(|text| text.contains('\u{276F}'));
        if saw_empty_prompt {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        saw_empty_prompt,
        "expected prompt marker in Panesmith snapshot"
    );

    {
        let pane = mux
            .get_mut("claude-reviewer")
            .expect("reviewer pane exists");
        pane.set_inbox_nudge_not_before(None);
        pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(10));
        pane.set_pending_inbox_nudge(true);
        pane.set_pending_inbox_nudge_since(Some(
            std::time::Instant::now() - std::time::Duration::from_secs(20),
        ));
        pane.set_focused(false);
    }

    mux.flush_pending_inbox_nudges(rt.handle());

    assert!(
        !mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .pending_inbox_nudge(),
        "Panesmith Teams inbox nudge should use snapshot prompt state and clear the pending flag"
    );

    rt.block_on(mux.shutdown_all());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_queue_startup_prompt_delays_claude_teams_inbox_delivery() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    mux.queue_startup_prompt("claude-code", "Factory supervisor startup".to_string());

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-code")
        .unwrap();
    let payload = std::fs::read_to_string(&inbox_path).expect("read supervisor inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    assert!(
        !messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "Factory supervisor startup")
    );
    assert_eq!(mux.pending_delayed_prompt_count(), 1);
    assert_eq!(
        mux.get("claude-code")
            .expect("supervisor pane exists")
            .delayed_prompt_in_flight()
            .expect("startup prompt queued")
            .generation,
        mux.get("claude-code")
            .expect("supervisor pane exists")
            .current_generation()
    );
    assert!(
        !mux.get("claude-code")
            .expect("supervisor pane exists")
            .pending_inbox_nudge()
    );

    {
        let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
        pane.set_inbox_nudge_not_before(None);
    }
    mux.get_mut("claude-code")
        .expect("supervisor pane exists")
        .delayed_prompt_in_flight_mut()
        .expect("startup prompt queued")
        .inject_after = std::time::Instant::now() - std::time::Duration::from_millis(1);
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    mux.flush_pending_startup_prompts(rt.handle());

    // Retry briefly to allow the non-blocking Teams delivery task to complete.
    let mut messages = serde_json::Value::Null;
    for _ in 0..20 {
        let payload = std::fs::read_to_string(&inbox_path).expect("read supervisor inbox");
        messages = serde_json::from_str(&payload).expect("parse inbox");
        if messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "Factory supervisor startup")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "Factory supervisor startup")
    );
    assert!(
        mux.get("claude-code")
            .expect("supervisor pane exists")
            .pending_inbox_nudge()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_queue_startup_prompt_staggers_delivery_times() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane-1", 24, 80).expect("pane-1"));
    mux.add_pane(Pane::director("pane-2", 24, 80).expect("pane-2"));

    mux.queue_startup_prompt("pane-1", "startup one".to_string());
    mux.queue_startup_prompt("pane-2", "startup two".to_string());

    assert_eq!(mux.pending_delayed_prompt_count(), 2);
    let pane_1_inject_after = mux
        .get("pane-1")
        .expect("pane-1")
        .delayed_prompt_in_flight()
        .expect("pane-1 queued startup prompt")
        .inject_after;
    let pane_2_inject_after = mux
        .get("pane-2")
        .expect("pane-2")
        .delayed_prompt_in_flight()
        .expect("pane-2 queued startup prompt")
        .inject_after;
    assert!(pane_2_inject_after > pane_1_inject_after);
}

#[test]
fn test_kimi_like_output_only_startup_prompt_recovers_and_accepts_followup_within_60s() {
    use crate::PaneState;

    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("kimi-worker", config).expect("spawn test pty");
    let pane = Pane::with_pty_cli(
        "kimi-worker",
        crate::PaneKind::Worker,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Kimi),
    )
    .expect("create kimi-like worker pane");
    mux.add_pane(pane);

    mux.queue_startup_prompt("kimi-worker", "startup prompt".to_string());
    mux.get_mut("kimi-worker")
        .expect("worker pane exists")
        .delayed_prompt_in_flight_mut()
        .expect("startup prompt queued")
        .inject_after = std::time::Instant::now() - std::time::Duration::from_millis(1);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    mux.flush_pending_startup_prompts(rt.handle());

    let delivered_at = match mux
        .get("kimi-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { delivered_at, .. }) => *delivered_at,
        other => panic!("expected busy pane state after startup prompt dispatch, got {other:?}"),
    };

    mux.get_mut("kimi-worker")
        .expect("worker pane exists")
        .append_output(b"thinking...\r\n")
        .expect("append output-only activity");

    let ready_at = delivered_at + crate::mux::QUIET_THRESHOLD + std::time::Duration::from_millis(1);
    assert!(
        ready_at.saturating_duration_since(delivered_at) < std::time::Duration::from_secs(60),
        "follow-up delivery should occur within the 60s recovery budget"
    );
    mux.tick_pane_state_machine_at(rt.handle(), ready_at);

    assert!(matches!(
        mux.get("kimi-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Ready { since }) if *since == ready_at
    ));

    rt.block_on(mux.deliver_prompt("kimi-worker", "follow-up prompt", None))
        .expect("deliver follow-up prompt");

    let second_inject_after = ready_at + std::time::Duration::from_millis(1);
    mux.queue_delayed_prompt(
        "kimi-worker",
        "second queued follow-up".to_string(),
        None,
        second_inject_after,
        None,
    );

    let second_dispatch_at = second_inject_after + std::time::Duration::from_millis(1);
    mux.tick_pane_state_machine_at(rt.handle(), second_dispatch_at);
    assert_eq!(
        mux.pending_delayed_prompt_count(),
        0,
        "second prompt should dispatch immediately after the pane returns Ready",
    );
    assert!(matches!(
        mux.get("kimi-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    let second_ready_at =
        second_dispatch_at + crate::mux::QUIET_THRESHOLD + std::time::Duration::from_millis(1);
    mux.tick_pane_state_machine_at(rt.handle(), second_ready_at);
    assert!(matches!(
        mux.get("kimi-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Ready { since }) if *since == second_ready_at
    ));
}

#[test]
fn test_fifo_drains_one_prompt_per_busy_ready_transition_in_order() {
    use crate::pane::PaneState;
    use brehon_types::PromptId;
    use std::time::{Duration, Instant};

    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    let inject_after = Instant::now() - Duration::from_millis(1);
    for idx in 1..=4 {
        mux.queue_delayed_prompt(
            "claude-code",
            format!("queued prompt {idx}"),
            None,
            inject_after,
            None,
        );
    }
    assert_eq!(mux.pending_delayed_prompt_count(), 4);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    for idx in 1..=4 {
        let now = Instant::now();
        let generation = mux
            .get("claude-code")
            .expect("supervisor pane exists")
            .current_generation();
        {
            let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
            pane.set_tool_executing(true);
            pane.set_pane_state(PaneState::Busy {
                prompt_id: PromptId::new(format!("busy-{idx}")),
                generation,
                delivered_at: now - crate::mux::QUIET_THRESHOLD,
                last_activity_at: now - crate::mux::QUIET_THRESHOLD,
            });
        }

        mux.tick_pane_state_machine_at(rt.handle(), now);
        assert_eq!(mux.pending_delayed_prompt_count(), 4 - idx as usize);
    }

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-code")
        .unwrap();
    let payload = std::fs::read_to_string(&inbox_path).expect("read supervisor inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    let delivered: Vec<String> = messages
        .as_array()
        .expect("inbox entries array")
        .iter()
        .filter_map(|message| {
            message["text"]
                .as_str()
                .map(str::to_string)
                .filter(|text| text.starts_with("queued prompt "))
        })
        .collect();

    assert_eq!(
        delivered,
        vec![
            "queued prompt 1".to_string(),
            "queued prompt 2".to_string(),
            "queued prompt 3".to_string(),
            "queued prompt 4".to_string(),
        ]
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_state_machine_dispatch_drops_stale_waiting_prompts_after_recycle() {
    use crate::pane::PaneState;
    use std::time::{Duration, Instant};

    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    let inject_after = Instant::now() - Duration::from_millis(1);
    let stale_in_flight_prompt_id = match mux.queue_delayed_prompt(
        "claude-code",
        "stale queued prompt in-flight".to_string(),
        None,
        inject_after,
        None,
    ) {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => prompt_id,
        other => panic!("expected stale in-flight prompt queueing, got {other:?}"),
    };
    let stale_waiting_prompt_id_1 = match mux.queue_delayed_prompt(
        "claude-code",
        "stale queued prompt waiting 1".to_string(),
        None,
        inject_after,
        None,
    ) {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => prompt_id,
        other => panic!("expected stale waiting prompt queueing, got {other:?}"),
    };
    let stale_waiting_prompt_id_2 = match mux.queue_delayed_prompt(
        "claude-code",
        "stale queued prompt waiting 2".to_string(),
        None,
        inject_after,
        None,
    ) {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => prompt_id,
        other => panic!("expected stale waiting prompt queueing, got {other:?}"),
    };
    assert_eq!(mux.pending_delayed_prompt_count(), 3);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _new_generation = rt
        .block_on(mux.recycle("claude-code", "test stale queued dispatch drop"))
        .0;

    let fresh_prompt_id = match mux.queue_delayed_prompt(
        "claude-code",
        "fresh queued prompt".to_string(),
        None,
        inject_after,
        None,
    ) {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => prompt_id,
        other => panic!("expected fresh prompt queueing, got {other:?}"),
    };

    {
        let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
        let dropped = pane
            .take_ready_delayed_prompt(Instant::now())
            .expect("stale in-flight prompt should exist before waiting drain");
        assert_eq!(dropped.prompt_id, stale_in_flight_prompt_id);
        pane.set_pane_state(PaneState::Ready {
            since: Instant::now(),
        });
    }

    let logs = capture_info_logs(|| {
        mux.tick_pane_state_machine_at(rt.handle(), Instant::now());
    });

    assert!(
        mux.pending_delayed_prompt_count() <= 1,
        "after dropping stale waiting prompts, at most one fresh prompt may remain queued"
    );
    assert!(
        logs.contains("dropped stale queued prompt after recycle"),
        "expected stale queued prompt drop log, got: {logs}"
    );
    assert!(
        logs.contains(&stale_waiting_prompt_id_1.to_string()),
        "expected first stale waiting prompt id in logs, got: {logs}"
    );
    assert!(
        logs.contains(&stale_waiting_prompt_id_2.to_string()),
        "expected second stale waiting prompt id in logs, got: {logs}"
    );
    assert!(
        !logs.contains(&fresh_prompt_id.to_string()),
        "fresh prompt should not be dropped as stale"
    );

    let pane = mux.get("claude-code").expect("supervisor pane exists");
    let queued_prompts: Vec<String> = pane
        .delayed_prompt_in_flight()
        .iter()
        .map(|queued| queued.prompt.clone())
        .chain(
            pane.delayed_prompt_waiting()
                .iter()
                .map(|queued| queued.prompt.clone()),
        )
        .collect();
    assert!(
        queued_prompts.is_empty() || queued_prompts == vec!["fresh queued prompt".to_string()],
        "stale waiting prompts should be dropped; only fresh prompt may remain queued"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_r2_four_back_to_back_worker_prompts_deliver_fifo_without_duplicate_retry() {
    use crate::pane::PaneState;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "worker-1",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "claude-supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create worker pane");
    mux.add_pane(pane);
    assert!(
        mux.panes
            .get("worker-1")
            .unwrap()
            .cli_type()
            .capabilities()
            .supports_teams
    );

    let queued_at = Instant::now() - Duration::from_millis(1);
    let mut max_retry_slots = 0usize;
    for idx in 1..=4 {
        mux.queue_delayed_prompt(
            "worker-1",
            format!("queued prompt {idx}"),
            Some("claude-supervisor".to_string()),
            queued_at,
            None,
        );
        let retry_slots = mux
            .get("worker-1")
            .expect("worker pane exists")
            .delayed_prompt_in_flight()
            .iter()
            .count();
        max_retry_slots = max_retry_slots.max(retry_slots);
        assert!(
            retry_slots <= 1,
            "expected at most one retry timer slot while queueing"
        );
    }
    assert_eq!(mux.pending_delayed_prompt_count(), 4);

    // Force one deferred attempt, then ensure it requeues at the head without
    // duplicating the prompt or creating parallel retry slots.
    {
        let pane = mux.get_mut("worker-1").expect("worker pane exists");
        pane.set_inbox_nudge_not_before(Some(Instant::now() + Duration::from_secs(30)));
        pane.set_pane_state(PaneState::Ready {
            since: Instant::now(),
        });
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    mux.tick_pane_state_machine_at(rt.handle(), Instant::now());

    {
        let pane = mux.get("worker-1").expect("worker pane exists");
        let waiting: Vec<String> = pane
            .delayed_prompt_waiting()
            .iter()
            .map(|queued| queued.prompt.clone())
            .collect();
        assert_eq!(
            pane.delayed_prompt_in_flight()
                .map(|queued| queued.prompt.as_str()),
            Some("queued prompt 1")
        );
        assert_eq!(
            waiting,
            vec![
                "queued prompt 2".to_string(),
                "queued prompt 3".to_string(),
                "queued prompt 4".to_string(),
            ]
        );
        assert_eq!(
            mux.pending_delayed_prompt_count(),
            4,
            "deferred delivery should requeue without duplicates"
        );
        let retry_slots = pane.delayed_prompt_in_flight().iter().count();
        max_retry_slots = max_retry_slots.max(retry_slots);
        assert!(
            retry_slots <= 1,
            "deferred retry should keep a single in-flight timer slot"
        );
    }

    {
        let pane = mux.get_mut("worker-1").expect("worker pane exists");
        pane.set_inbox_nudge_not_before(None);
        pane.delayed_prompt_in_flight_mut()
            .expect("in-flight queued prompt")
            .inject_after = Instant::now() - Duration::from_millis(1);
    }

    let mut tick_at = Instant::now();
    for idx in 1..=4 {
        if idx > 1 {
            tick_at = tick_at + crate::mux::QUIET_THRESHOLD + Duration::from_millis(1);
        }
        mux.tick_pane_state_machine_at(rt.handle(), tick_at);

        let pane = mux.get("worker-1").expect("worker pane exists");
        let retry_slots = pane.delayed_prompt_in_flight().iter().count();
        max_retry_slots = max_retry_slots.max(retry_slots);
        assert!(
            retry_slots <= 1,
            "queue drain should never arm parallel retry timers"
        );
        assert_eq!(mux.pending_delayed_prompt_count(), 4 - idx as usize);
    }

    assert!(
        max_retry_slots <= 1,
        "maximum retry slots observed for pane should be at most one"
    );

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("worker-1")
        .unwrap();
    let payload = std::fs::read_to_string(&inbox_path).expect("read worker inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    let delivered: Vec<String> = messages
        .as_array()
        .expect("inbox entries array")
        .iter()
        .filter_map(|message| {
            message["text"]
                .as_str()
                .map(str::to_string)
                .filter(|text| text.starts_with("queued prompt "))
        })
        .collect();

    assert_eq!(delivered.len(), 4, "expected exactly four deliveries");
    let unique: HashSet<&String> = delivered.iter().collect();
    assert_eq!(unique.len(), delivered.len(), "deliveries must be unique");
    assert_eq!(
        delivered,
        vec![
            "queued prompt 1".to_string(),
            "queued prompt 2".to_string(),
            "queued prompt 3".to_string(),
            "queued prompt 4".to_string(),
        ]
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_deliver_prompt_delays_claude_teams_inbox_until_settle_deadline() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-reviewer",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::reviewer(
        "claude-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        None,
    )
    .expect("create reviewer pane");
    mux.add_pane(pane);

    {
        let pane = mux
            .get_mut("claude-reviewer")
            .expect("reviewer pane exists");
        pane.set_inbox_nudge_not_before(Some(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        ));
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(mux.deliver_prompt("claude-reviewer", "review request", None))
        .expect("queue delayed prompt");

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-reviewer")
        .unwrap();
    let payload = std::fs::read_to_string(&inbox_path).expect("read reviewer inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    assert!(
        !messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "review request")
    );
    assert_eq!(mux.pending_delayed_prompt_count(), 1);
    assert_eq!(
        mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .delayed_prompt_in_flight()
            .expect("delayed prompt queued")
            .generation,
        mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .current_generation()
    );
    assert!(
        !mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .pending_inbox_nudge()
    );

    {
        let pane = mux
            .get_mut("claude-reviewer")
            .expect("reviewer pane exists");
        pane.set_inbox_nudge_not_before(None);
    }
    mux.get_mut("claude-reviewer")
        .expect("reviewer pane exists")
        .delayed_prompt_in_flight_mut()
        .expect("delayed prompt queued")
        .inject_after = std::time::Instant::now() - std::time::Duration::from_millis(1);
    mux.flush_pending_startup_prompts(rt.handle());

    // Retry briefly to allow the non-blocking Teams delivery task to complete.
    let mut messages = serde_json::Value::Null;
    for _ in 0..20 {
        let payload = std::fs::read_to_string(&inbox_path).expect("read reviewer inbox");
        messages = serde_json::from_str(&payload).expect("parse inbox");
        if messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "review request")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "review request")
    );
    assert!(
        mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .pending_inbox_nudge()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_pending_teams_nudge_cooldown_does_not_block_inbox_writes() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-reviewer",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::reviewer(
        "claude-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        None,
    )
    .expect("create reviewer pane");
    mux.add_pane(pane);

    {
        let pane = mux
            .get_mut("claude-reviewer")
            .expect("reviewer pane exists");
        pane.set_pending_inbox_nudge(true);
        pane.set_inbox_nudge_not_before(Some(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        ));
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let attempt = rt
        .block_on(mux.attempt_prompt_delivery(
            "claude-reviewer",
            "review approved",
            Some("review-coordinator"),
        ))
        .expect("attempt prompt delivery");
    assert!(
        matches!(attempt, PromptDeliveryAttempt::Delivered { .. }),
        "pending nudge cooldown must not defer safe Teams inbox writes, got {attempt:?}"
    );
    assert_eq!(mux.pending_delayed_prompt_count(), 0);

    mux.dispatch_deliver_prompt(
        rt.handle(),
        "claude-reviewer",
        "dispatch follow-up".to_string(),
        Some("review-coordinator".to_string()),
    );

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-reviewer")
        .unwrap();
    let mut messages = serde_json::Value::Null;
    for _ in 0..20 {
        let payload = std::fs::read_to_string(&inbox_path).expect("read reviewer inbox");
        messages = serde_json::from_str(&payload).expect("parse inbox");
        if messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "dispatch follow-up")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let messages = messages.as_array().expect("inbox entries array");
    assert!(
        messages
            .iter()
            .any(|message| message["text"] == "review approved")
    );
    assert!(
        messages
            .iter()
            .any(|message| message["text"] == "dispatch follow-up")
    );
    assert_eq!(mux.pending_delayed_prompt_count(), 0);
    assert!(
        mux.get("claude-reviewer")
            .expect("reviewer pane exists")
            .pending_inbox_nudge()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_manual_enter_clears_pending_inbox_nudge_on_empty_supervisor_prompt() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    mux.deliver_prompt("claude-code", "review complete", None)
        .await
        .expect("deliver prompt");

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.append_output(b"\xe2\x9d\xaf \r\n")
            .expect("append empty prompt");
    }

    mux.send_input_to("claude-code", b"\r")
        .await
        .expect("manual enter");

    assert!(
        !mux.get("claude-code")
            .expect("pane exists")
            .pending_inbox_nudge()
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_nonblocking_teams_inbox_write_failure_surfaces_to_delivery_state() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::supervisor(
        "claude-code",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &[],
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        None,
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("claude-code").expect("pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-code")
        .unwrap();
    std::fs::write(&inbox_path, "this is not json {{[").expect("write corrupt inbox");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let logs = capture_info_logs(|| {
        mux.dispatch_deliver_prompt(
            rt.handle(),
            "claude-code",
            "review complete".to_string(),
            None,
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let (_bytes, _events) = mux.poll_batch();
            let pane = mux.get("claude-code").expect("pane exists");
            let viewport = pane.dump_viewport().expect("dump viewport");
            if !pane.pending_inbox_nudge() && viewport.contains("Prompt delivery failed") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for async Teams failure to surface"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    let pane = mux.get("claude-code").expect("pane exists");
    assert!(
        !pane.pending_inbox_nudge(),
        "failed Teams delivery should clear the pending inbox nudge"
    );
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        viewport.contains("Prompt delivery failed"),
        "failed Teams delivery should surface in the pane viewport"
    );
    assert!(
        logs.contains("ERROR"),
        "expected error-level Teams delivery log, got: {logs}"
    );
    assert!(
        logs.contains("team=test-session"),
        "expected structured team field in logs, got: {logs}"
    );
    assert!(
        logs.contains("agent=claude-code"),
        "expected structured agent field in logs, got: {logs}"
    );
    assert!(
        logs.contains("error=Teams inbox write failed:"),
        "expected structured error field in logs, got: {logs}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_deliver_prompt_routes_pty_only_agents_via_injection() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "junie-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Junie),
        None,
        None,
        None,
        None,
    )
    .expect("create junie reviewer pane");
    mux.add_pane(pane);

    assert!(
        !mux.get("junie-reviewer")
            .expect("reviewer pane exists")
            .is_gateway_backed()
    );

    mux.deliver_prompt("junie-reviewer", "check this review", None)
        .await
        .expect("deliver prompt");
}

#[tokio::test]
async fn test_deliver_prompt_clears_stale_gateway_session_before_respawn_attempt() {
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    let mut spawn_config = pane
        .take_gateway_spawn_config()
        .expect("gateway spawn config");
    spawn_config.command = Some("/definitely-missing-codex".to_string());
    pane.restore_gateway_spawn_config(spawn_config);
    mux.add_pane(pane);

    let pane = mux.get_mut("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.gateway_spawn_config().is_some());
    pane.register_gateway_session_spawn("missing-session".to_string());

    let err = mux
        .deliver_prompt("codex-reviewer", "review this change", None)
        .await
        .expect_err("gateway delivery should fail when the fresh spawn is missing");

    assert!(err.to_string().contains("Failed to spawn gateway session"));

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.gateway_session_id().is_none());
    assert!(pane.gateway_spawn_config().is_some());

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("reset a degraded gateway session"));
    assert!(viewport.contains("Gateway spawn failed"));
}

#[test]
fn test_begin_async_gateway_prompt_delivery_clears_stale_session_before_respawn_attempt() {
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    let mut spawn_config = pane
        .take_gateway_spawn_config()
        .expect("gateway spawn config");
    spawn_config.command = Some("/definitely-missing-codex".to_string());
    pane.restore_gateway_spawn_config(spawn_config);
    mux.add_pane(pane);

    let pane = mux.get_mut("codex-reviewer").expect("reviewer pane exists");
    pane.register_gateway_session_spawn("missing-session".to_string());

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let err = rt
        .block_on(mux.begin_async_gateway_prompt_delivery(
            rt.handle(),
            "codex-reviewer",
            "review this change",
        ))
        .expect_err("async gateway delivery should fail when the fresh spawn is missing");

    assert!(err.to_string().contains("Failed to spawn gateway session"));

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.gateway_session_id().is_none());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("reset a degraded gateway session"));
    assert!(viewport.contains("Gateway spawn failed"));
}

#[tokio::test]
async fn test_deliver_prompt_buffers_second_codex_pty_prompt_until_first_submits() {
    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("codex-supervisor", config).expect("spawn test pty");
    let mut pane = Pane::with_pty_cli(
        "codex-supervisor",
        PaneKind::Supervisor,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Codex),
    )
    .expect("create codex supervisor pane");
    pane.set_last_output_at(std::time::Instant::now() - std::time::Duration::from_secs(2));
    mux.add_pane(pane);

    mux.deliver_prompt("codex-supervisor", "first supervisor message", None)
        .await
        .expect("inject first prompt");
    assert!(
        mux.get("codex-supervisor")
            .expect("pane exists")
            .has_pending_ink_submit()
    );

    mux.deliver_prompt("codex-supervisor", "second supervisor message", None)
        .await
        .expect("buffer second prompt");

    assert_eq!(mux.pending_delayed_prompt_count(), 1);
    let queued = mux
        .get("codex-supervisor")
        .expect("pane exists")
        .delayed_prompt_in_flight()
        .expect("second prompt should be queued");
    assert_eq!(queued.prompt, "second supervisor message");
    assert_eq!(
        queued.generation,
        mux.get("codex-supervisor")
            .expect("pane exists")
            .current_generation()
    );
}

#[tokio::test]
async fn test_deliver_prompt_buffers_gateway_prompt_while_tool_executing() {
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    pane.set_tool_executing(true);
    pane.ensure_activity_buffer();
    pane.activity_buffer_mut()
        .expect("activity buffer")
        .start_tool("tool-1".to_string(), "ReadFile".to_string());
    pane.set_pane_state(crate::PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("seed-prompt".to_string()),
        generation: pane.current_generation(),
        delivered_at: std::time::Instant::now(),
        last_activity_at: std::time::Instant::now(),
    });
    mux.add_pane(pane);

    mux.deliver_prompt("codex-reviewer", "review this change", None)
        .await
        .expect("gateway prompt should be queued while the pane is busy");

    assert_eq!(mux.pending_delayed_prompt_count(), 1);
    let queued = mux
        .get("codex-reviewer")
        .expect("reviewer pane exists")
        .delayed_prompt_in_flight()
        .expect("gateway prompt queued");
    assert_eq!(queued.prompt, "review this change");
    assert_eq!(
        queued.generation,
        mux.get("codex-reviewer")
            .expect("reviewer pane exists")
            .current_generation()
    );
}

#[test]
fn test_fresh_gateway_placeholder_busy_does_not_count_as_live_turn() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    assert!(
        !pane.is_tool_executing(),
        "gateway panes start idle so the first prompt is not blocked by a stale \"busy\" flag"
    );
    mux.add_pane(pane);

    assert!(
        !mux.pane_has_live_gateway_turn("codex-reviewer"),
        "startup placeholder state must not block the first gateway prompt",
    );
}

#[test]
fn test_attempt_prompt_delivery_returns_already_present_for_duplicate_queued_prompt() {
    use std::time::{Duration, Instant};

    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "worker-1",
            &[],
            PathBuf::from("/tmp").as_path(),
            &std::collections::HashMap::new(),
            "lead",
        )
        .expect("init team config");
    mux.set_teams(teams);

    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "claude-supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create worker pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("worker-1").expect("worker pane exists");
        pane.set_inbox_nudge_not_before(Some(Instant::now() + Duration::from_secs(30)));
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let first_prompt_id = match rt
        .block_on(mux.attempt_prompt_delivery(
            "worker-1",
            "duplicate prompt",
            Some("claude-supervisor"),
        ))
        .expect("attempt prompt delivery")
    {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => prompt_id,
        other => panic!("expected queued delivery attempt, got {other:?}"),
    };

    let inject_after = Instant::now() + Duration::from_secs(30);
    match mux.queue_delayed_prompt(
        "worker-1",
        "duplicate prompt".to_string(),
        Some("claude-supervisor".to_string()),
        inject_after,
        Some(first_prompt_id.clone()),
    ) {
        PromptDeliveryAttempt::Queued { prompt_id, .. } => {
            assert_eq!(prompt_id, first_prompt_id);
        }
        other => panic!("expected delayed prompt to enqueue, got {other:?}"),
    }

    let queue_len_before = mux.pending_delayed_prompt_count();
    assert_eq!(
        queue_len_before, 1,
        "expected one queued prompt before duplicate attempt"
    );

    let duplicate_attempt = rt
        .block_on(mux.attempt_prompt_delivery(
            "worker-1",
            "duplicate prompt",
            Some("claude-supervisor"),
        ))
        .expect("duplicate attempt prompt delivery");

    match duplicate_attempt {
        PromptDeliveryAttempt::AlreadyPresent {
            prompt_id,
            position,
        } => {
            assert_eq!(prompt_id, first_prompt_id);
            assert_eq!(position, PromptQueuePosition::InFlight);
        }
        other => panic!("expected duplicate attempt to be AlreadyPresent, got {other:?}"),
    }
    assert_eq!(
        mux.pending_delayed_prompt_count(),
        queue_len_before,
        "duplicate attempt must not grow pane prompt queue"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_finalize_async_gateway_prompt_delivery_clears_session_on_recoverable_error() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    mux.add_pane(pane);

    let pane = mux.get_mut("codex-reviewer").expect("reviewer pane exists");
    pane.register_gateway_session_spawn("reviewer-session".to_string());

    mux.finalize_async_gateway_prompt_delivery(
        "codex-reviewer",
        "review this change",
        None,
        Err(AsyncGatewayPromptDeliveryError {
            error: "temporary timeout".to_string(),
        }),
    );

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.gateway_session_id().is_none());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("Prompt delivery failed"));
}

#[test]
fn test_async_gateway_prompt_delivery_completion_appends_notice() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    mux.add_pane(pane);

    mux.event_tx
        .try_send(MuxEvent::AsyncGatewayPromptDeliveryCompleted {
            pane_id: "codex-reviewer".to_string(),
            prompt: "review this change".to_string(),
            from: None,
            generation: crate::Generation::default(),
            result: Ok(PromptDeliveryAttempt::Delivered {
                prompt_id: brehon_types::PromptId::new("prompt-delivered"),
                generation: crate::Generation::default(),
            }),
        })
        .expect("queue async completion");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(viewport.contains("prompt delivered"));
}

#[tokio::test]
async fn test_async_gateway_prompt_delivery_completion_drops_stale_generation_after_recycle() {
    let pane_id = "codex-reviewer";
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::reviewer(
        pane_id,
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    pane.register_gateway_session_spawn("reviewer-session-1".to_string());
    mux.add_pane(pane);

    let stale_generation = mux
        .get(pane_id)
        .expect("reviewer pane exists")
        .current_generation();
    let current_generation = mux
        .recycle(pane_id, "drop stale async gateway completion")
        .await;
    assert_ne!(current_generation, stale_generation);
    assert!(matches!(
        mux.get(pane_id).expect("reviewer pane exists").pane_state(),
        Some(crate::PaneState::Ready { .. })
    ));
    let _ = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::AsyncGatewayPromptDeliveryCompleted {
            pane_id: pane_id.to_string(),
            prompt: "review this change".to_string(),
            from: None,
            generation: stale_generation,
            result: Ok(PromptDeliveryAttempt::Delivered {
                prompt_id: brehon_types::PromptId::new("stale-prompt-delivered"),
                generation: stale_generation,
            }),
        })
        .expect("queue stale async completion");
    let (_bytes, stale_events) = mux.poll_batch();
    assert!(
        stale_events.is_empty(),
        "stale async completion should not be forwarded to poll_batch consumers"
    );
    let pane = mux.get(pane_id).expect("reviewer pane exists");
    assert!(matches!(
        pane.pane_state(),
        Some(crate::PaneState::Ready { .. })
    ));
    assert!(!pane.is_tool_executing());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("prompt delivered"),
        "stale async completion should not append a delivery notice"
    );
}

#[tokio::test]
async fn test_async_gateway_prompt_delivery_non_delivered_arms_drop_stale_generation_after_recycle()
{
    let pane_id = "codex-reviewer";
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::reviewer(
        pane_id,
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    pane.register_gateway_session_spawn("reviewer-session-1".to_string());
    mux.add_pane(pane);

    let stale_generation = mux
        .get(pane_id)
        .expect("reviewer pane exists")
        .current_generation();
    let current_generation = mux
        .recycle(pane_id, "drop stale async gateway non-delivered completion")
        .await;
    assert_ne!(current_generation, stale_generation);
    let _ = mux.poll_batch();

    for result in [
        Ok(PromptDeliveryAttempt::Queued {
            prompt_id: brehon_types::PromptId::new("stale-prompt-queued"),
            ahead_of: 1,
        }),
        Ok(PromptDeliveryAttempt::AlreadyPresent {
            prompt_id: brehon_types::PromptId::new("stale-prompt-present"),
            position: PromptQueuePosition::InFlight,
        }),
        Ok(PromptDeliveryAttempt::Rejected {
            reason: crate::pane::DeathReason::SessionDropped,
        }),
        Err(AsyncGatewayPromptDeliveryError {
            error: "prompt already in progress".to_string(),
        }),
    ] {
        mux.event_tx
            .try_send(MuxEvent::AsyncGatewayPromptDeliveryCompleted {
                pane_id: pane_id.to_string(),
                prompt: "review this change".to_string(),
                from: None,
                generation: stale_generation,
                result,
            })
            .expect("queue stale async completion");
        let (_bytes, stale_events) = mux.poll_batch();
        assert!(
            stale_events.is_empty(),
            "stale async completion should not be forwarded to poll_batch consumers"
        );
    }

    let pane = mux.get(pane_id).expect("reviewer pane exists");
    assert!(matches!(
        pane.pane_state(),
        Some(crate::PaneState::Ready { .. })
    ));
    assert!(!pane.is_tool_executing());
    assert_eq!(mux.pending_delayed_prompt_count(), 0);
    assert!(pane.delayed_prompt_in_flight().is_none());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("prompt delivered"),
        "stale async completion should not append a delivery notice"
    );
    assert!(
        !viewport.contains("Prompt delivery failed"),
        "stale async completion should not surface a failure notice"
    );
}

#[test]
fn test_async_gateway_prompt_delivery_busy_error_requeues_prompt() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        None,
    )
    .expect("create codex reviewer pane");
    mux.add_pane(pane);

    mux.event_tx
        .try_send(MuxEvent::AsyncGatewayPromptDeliveryCompleted {
            pane_id: "codex-reviewer".to_string(),
            prompt: "review this change".to_string(),
            from: None,
            generation: crate::Generation::default(),
            result: Ok(PromptDeliveryAttempt::Queued {
                prompt_id: brehon_types::PromptId::new("prompt-queued"),
                ahead_of: 1,
            }),
        })
        .expect("queue async completion");
    let (_bytes, _events) = mux.poll_batch();

    assert_eq!(mux.pending_delayed_prompt_count(), 1);
    let queued = mux
        .get("codex-reviewer")
        .expect("reviewer pane exists")
        .delayed_prompt_in_flight()
        .expect("prompt requeued");
    assert_eq!(queued.prompt, "review this change");
    assert_eq!(
        queued.generation,
        mux.get("codex-reviewer")
            .expect("reviewer pane exists")
            .current_generation()
    );

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.is_tool_executing());
    assert!(matches!(
        pane.pane_state(),
        Some(crate::PaneState::Busy { .. })
    ));
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("Prompt delivery failed"),
        "busy async deliveries should retry instead of surfacing as failures"
    );
}

#[test]
fn test_state_machine_dispatch_does_not_write_synthetic_queued_prompt_id() {
    use std::time::{Duration, Instant};

    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("kimi-worker", config).expect("spawn test pty");
    let pane = Pane::with_pty_cli(
        "kimi-worker",
        crate::PaneKind::Worker,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Kimi),
    )
    .expect("create kimi-like worker pane");
    mux.add_pane(pane);

    let queued_prompt_id = brehon_types::PromptId::new("queued-turn-id");
    let inject_after = Instant::now() - Duration::from_millis(1);
    let queued = mux.queue_delayed_prompt(
        "kimi-worker",
        "queued prompt".to_string(),
        None,
        inject_after,
        Some(queued_prompt_id),
    );
    assert!(matches!(queued, PromptDeliveryAttempt::Queued { .. }));

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    mux.tick_pane_state_machine_at(rt.handle(), Instant::now());

    let pane = mux.get("kimi-worker").expect("worker pane exists");
    let busy_prompt_id = match pane.pane_state() {
        Some(crate::PaneState::Busy { prompt_id, .. }) => prompt_id.to_string(),
        other => panic!("expected Busy pane state after queued dispatch, got {other:?}"),
    };
    assert!(
        !busy_prompt_id.starts_with("queued:kimi-worker:"),
        "queued dispatch should preserve a real delivery prompt id, got {busy_prompt_id}"
    );
}

#[test]
fn test_per_pane_prompt_queue_keeps_single_in_flight_under_random_interleavings() {
    fn lcg_next(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *seed
    }

    fn make_busy_gateway_pane(name: &str) -> Pane {
        let mut pane = Pane::reviewer(
            name,
            PathBuf::from("/tmp"),
            None,
            24,
            80,
            &AgentAdapter::BuiltIn(SupervisorCli::Codex),
            None,
            None,
            None,
            None,
        )
        .expect("create busy gateway pane");
        pane.set_tool_executing(true);
        pane.ensure_activity_buffer();
        pane.activity_buffer_mut()
            .expect("activity buffer")
            .start_tool(format!("busy-{name}"), "ReadFile".to_string());
        pane.set_pane_state(crate::PaneState::Busy {
            prompt_id: brehon_types::PromptId::new(format!("busy-prompt-{name}")),
            generation: pane.current_generation(),
            delivered_at: std::time::Instant::now(),
            last_activity_at: std::time::Instant::now(),
        });
        pane
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    for scenario in 0..16u64 {
        let mut mux = Mux::new(24, 80);
        mux.add_pane(make_busy_gateway_pane("codex-reviewer-a"));
        mux.add_pane(make_busy_gateway_pane("codex-reviewer-b"));

        let mut seed = scenario.wrapping_add(0x9E37_79B9_7F4A_7C15);
        for step in 0..96u64 {
            let target = if lcg_next(&mut seed).is_multiple_of(2) {
                "codex-reviewer-a"
            } else {
                "codex-reviewer-b"
            };
            match lcg_next(&mut seed) % 4 {
                0 => {
                    let result = rt.block_on(mux.deliver_prompt(
                        target,
                        &format!("prompt-{scenario}-{step}"),
                        None,
                    ));
                    if let Err(err) = result {
                        assert!(
                            err.to_string().contains("prompt queue depth exceeded"),
                            "unexpected deliver_prompt error: {err}"
                        );
                    }
                }
                1 => mux.dispatch_deliver_prompt(
                    rt.handle(),
                    target,
                    format!("dispatch-{scenario}-{step}"),
                    None,
                ),
                2 => {
                    mux.queue_startup_prompt(target, format!("startup-{scenario}-{step}"));
                }
                _ => {
                    if let Some(pane) = mux.get_mut(target)
                        && let Some(in_flight) = pane.delayed_prompt_in_flight_mut()
                    {
                        in_flight.inject_after =
                            std::time::Instant::now() - std::time::Duration::from_millis(1);
                    }
                    mux.flush_pending_startup_prompts(rt.handle());
                }
            }

            for pane_id in ["codex-reviewer-a", "codex-reviewer-b"] {
                let pane = mux.get(pane_id).expect("pane exists");
                assert!(
                    pane.delayed_prompt_in_flight().iter().count() <= 1,
                    "pane {pane_id} should never hold multiple in-flight prompts"
                );
                assert!(
                    pane.delayed_prompt_waiting().len() <= 8,
                    "pane {pane_id} waiting queue should honor depth cap"
                );
            }
        }
    }
}
