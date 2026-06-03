use super::{ScopedEnv, TEST_ENV_LOCK};
use crate::mux::*;
use crate::teams::{TeamsManager, TeamsPaths};
use crate::{
    ActivityEntry, ActivityKind, AgentAdapter, DeathReason, Generation, Pane, PaneState,
    SupervisorCli,
};
use brehon_ports::RuntimeEventStream;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogCaptureWriter(self.0.clone())
    }
}

impl io::Write for LogCaptureWriter {
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

fn capture_logs_at(max_level: tracing::Level, run: impl FnOnce()) -> String {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .with_ansi(false)
        .with_writer(LogCapture(captured.clone()))
        .with_max_level(max_level)
        .finish();

    tracing::subscriber::with_default(subscriber, run);

    String::from_utf8(captured.lock().expect("log capture mutex poisoned").clone())
        .expect("captured logs should be utf-8")
}

fn capture_logs(run: impl FnOnce()) -> String {
    capture_logs_at(tracing::Level::WARN, run)
}

fn capture_debug_logs(run: impl FnOnce()) -> String {
    capture_logs_at(tracing::Level::DEBUG, run)
}

fn mux_with_busy_worker_for_output(pane_id: &str) -> (Mux, Generation) {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        pane_id,
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
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

    let generation = mux
        .get(pane_id)
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut(pane_id).expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    (mux, generation)
}

#[test]
fn test_activity_events_drive_pane_state_transitions() {
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
    pane.register_gateway_session_spawn("seed-session".to_string());
    pane.ensure_activity_buffer();
    mux.add_pane(pane);

    let pane_id = "codex-reviewer".to_string();
    let generation = mux
        .get(&pane_id)
        .expect("reviewer pane exists")
        .current_generation();

    let started = ActivityEntry {
        kind: ActivityKind::Operation,
        ingested_at: std::time::Instant::now(),
        tool_id: None,
        tool_name: None,
        status: Some("started".to_string()),
        message: Some("review".to_string()),
        output_chunks: None,
        duration: None,
    };
    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.clone(),
            entry: started,
            generation,
        })
        .expect("queue started activity event");
    let _ = mux.poll_batch();
    assert!(
        mux.get(&pane_id)
            .expect("reviewer pane exists")
            .is_tool_executing()
    );
    assert!(matches!(
        mux.get(&pane_id)
            .expect("reviewer pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    let completed = ActivityEntry {
        kind: ActivityKind::Operation,
        ingested_at: std::time::Instant::now(),
        tool_id: None,
        tool_name: None,
        status: Some("completed".to_string()),
        message: Some("review".to_string()),
        output_chunks: None,
        duration: None,
    };
    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id,
            entry: completed,
            generation: Generation(generation.0),
        })
        .expect("queue completed activity event");
    let _ = mux.poll_batch();
    assert!(
        !mux.get("codex-reviewer")
            .expect("reviewer pane exists")
            .is_tool_executing()
    );
    assert!(matches!(
        mux.get("codex-reviewer")
            .expect("reviewer pane exists")
            .pane_state(),
        Some(PaneState::Ready { .. })
    ));
}

#[test]
fn test_dispatch_deliver_prompt_buffers_gateway_prompt_while_tool_executing() {
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
    pane.register_gateway_session_spawn("reviewer-session".to_string());
    pane.set_tool_executing(true);
    pane.ensure_activity_buffer();
    pane.activity_buffer_mut()
        .expect("activity buffer")
        .start_tool("tool-1".to_string(), "ReadFile".to_string());
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("seed-prompt".to_string()),
        generation: pane.current_generation(),
        delivered_at: std::time::Instant::now(),
        last_activity_at: std::time::Instant::now(),
    });
    mux.add_pane(pane);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    mux.dispatch_deliver_prompt(
        rt.handle(),
        "codex-reviewer",
        "review this change".to_string(),
        None,
    );

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

#[tokio::test]
async fn test_reset_reviewer_gateway_session_clears_runtime_state() {
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
    pane.set_review_context(crate::ReviewContextSnapshot {
        review_id: "REV-1".to_string(),
        task_id: "T-1".to_string(),
        round: 1,
        panel_total: 3,
        panel_done: 1,
        verdict: None,
        score: None,
        findings_summary: None,
        updated_at: std::time::Instant::now(),
    });

    mux.reset_reviewer_session("codex-reviewer")
        .await
        .expect("reset reviewer session");

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.review_context().is_none());
    assert!(pane.gateway_session_id().is_none());
}

#[tokio::test]
async fn test_shutdown_all_clears_gateway_runtime_state() {
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
    pane.set_gateway_event_bridge_started(true);
    pane.set_tool_executing(true);
    pane.set_pending_inbox_nudge(true);
    pane.ensure_activity_buffer();
    pane.activity_buffer_mut()
        .expect("activity buffer")
        .append_output("ready");

    mux.shutdown_all().await;

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    assert!(pane.gateway_session_id().is_none());
    assert!(!pane.gateway_event_bridge_started());
    assert!(!pane.is_tool_executing());
    assert!(!pane.pending_inbox_nudge());
    assert_eq!(
        pane.activity_buffer()
            .expect("activity buffer")
            .entries()
            .count(),
        0
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_reset_reviewer_session_clears_assignment_snapshot_file() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let mut mux = Mux::new(24, 80);
    let pane = Pane::reviewer(
        "codex-reviewer",
        temp.path().to_path_buf(),
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
    mux.set_pane_review_context(
        "codex-reviewer",
        crate::ReviewContextSnapshot {
            review_id: "REV-ctx".to_string(),
            task_id: "T-ctx".to_string(),
            round: 2,
            panel_total: 3,
            panel_done: 1,
            verdict: None,
            score: None,
            findings_summary: Some("pending".to_string()),
            updated_at: std::time::Instant::now(),
        },
    );

    let path = brehon_root
        .join("runtime")
        .join("pane-assignment-context")
        .join("codex-reviewer.json");
    assert!(path.exists(), "set review context should create snapshot");

    mux.reset_reviewer_session("codex-reviewer")
        .await
        .expect("reset reviewer session");

    assert!(
        !path.exists(),
        "reset reviewer session should clear persisted assignment snapshot"
    );
}

#[tokio::test]
async fn test_reset_reviewer_session_restarts_claude_pty_reviewer() {
    use brehon_pty::{Pty, PtyConfig};

    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![(
            "BREHON_SESSION_ID".to_string(),
            "reviewer-session-1".to_string(),
        )],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("claude-reviewer", config.clone()).expect("spawn test reviewer pty");
    let mut pane = Pane::with_pty_cli(
        "claude-reviewer",
        PaneKind::Reviewer,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create claude reviewer pane");
    pane.set_agent_session_id(Some("reviewer-session-1".to_string()));
    pane.set_pty_spawn_config(config);
    pane.set_review_context(crate::ReviewContextSnapshot {
        review_id: "REV-2".to_string(),
        task_id: "T-2".to_string(),
        round: 1,
        panel_total: 3,
        panel_done: 1,
        verdict: None,
        score: None,
        findings_summary: None,
        updated_at: std::time::Instant::now(),
    });
    mux.add_pane(pane);

    mux.reset_reviewer_session("claude-reviewer")
        .await
        .expect("reset claude reviewer session");

    let pane = mux.get("claude-reviewer").expect("reviewer pane exists");
    assert!(pane.review_context().is_none());
    assert!(!pane.has_exited());
    assert!(pane.gateway_session_id().is_none());
    assert_ne!(pane.agent_session_id(), Some("reviewer-session-1"));
}

#[tokio::test]
async fn test_reset_supervisor_session_restarts_claude_pty_supervisor() {
    use brehon_pty::{Pty, PtyConfig};

    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![(
            "BREHON_SESSION_ID".to_string(),
            "supervisor-session-1".to_string(),
        )],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("claude-supervisor", config.clone()).expect("spawn test supervisor pty");
    let mut pane = Pane::with_pty_cli(
        "claude-supervisor",
        PaneKind::Supervisor,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create claude supervisor pane");
    pane.set_agent_session_id(Some("supervisor-session-1".to_string()));
    pane.set_pty_spawn_config(config);
    pane.set_pending_inbox_nudge(true);
    mux.add_pane(pane);

    mux.reset_supervisor_session("claude-supervisor")
        .await
        .expect("reset claude supervisor session");

    let pane = mux
        .get("claude-supervisor")
        .expect("supervisor pane exists");
    assert!(!pane.has_exited());
    assert!(pane.gateway_session_id().is_none());
    assert!(!pane.pending_inbox_nudge());
    assert_ne!(pane.agent_session_id(), Some("supervisor-session-1"));
}

#[tokio::test]
async fn test_reset_worker_gateway_session_preserves_task_context_and_clears_session() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "codex-worker",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.register_gateway_session_spawn("worker-session".to_string());
    let generation = pane.current_generation();
    pane.set_tool_executing(true);
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("stuck-prompt".to_string()),
        generation,
        delivered_at: std::time::Instant::now(),
        last_activity_at: std::time::Instant::now(),
    });
    pane.set_pending_inbox_nudge(true);
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-42".to_string(),
        title: "Example task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.reset_worker_gateway_session("codex-worker")
        .await
        .expect("reset worker session");

    let pane = mux.get("codex-worker").expect("worker pane exists");
    assert!(pane.gateway_session_id().is_none());
    assert!(!pane.is_tool_executing());
    assert!(!pane.pending_inbox_nudge());
    assert!(matches!(pane.pane_state(), Some(PaneState::Ready { .. })));
    let context = pane.task_context().expect("task context preserved");
    assert_eq!(context.task_id, "T-42");
}

#[test]
fn test_permission_request_activity_marks_worker_blocked_with_task_context() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    mux.get_mut("codex-worker")
        .expect("worker pane exists")
        .set_task_context(crate::TaskContextSnapshot {
            task_id: "T-owned".to_string(),
            title: "Owned task".to_string(),
            status: brehon_types::task::TaskStatus::InProgress,
            completion_mode: None,
            merge_target: None,
            parent_id: None,
            epic_branch: None,
            epic_worktree: None,
            blocked_reason: None,
            updated_at: std::time::Instant::now(),
        });

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");

    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.task_id.as_deref(), Some("T-owned"));
            assert_eq!(info.request_id.as_deref(), Some("perm-1"));
            assert_eq!(info.command_or_tool.as_deref(), Some("allow bash ls"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_terminal_prompt_output_marks_reviewer_blocked_with_review_task_context() {
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

    let generation = mux
        .get("codex-reviewer")
        .expect("reviewer pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-reviewer").expect("reviewer pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("review-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_review_context(crate::ReviewContextSnapshot {
        review_id: "REV-1".to_string(),
        task_id: "T-review".to_string(),
        round: 1,
        panel_total: 3,
        panel_done: 1,
        verdict: None,
        score: None,
        findings_summary: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-reviewer".to_string(),
            data: b"Do you want to allow this command? git show HEAD\r\n".to_vec(),
            generation,
        })
        .expect("queue reviewer pane output");

    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-reviewer").expect("reviewer pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::TerminalPrompt
            );
            assert_eq!(info.task_id.as_deref(), Some("T-review"));
            assert_eq!(info.request_id.as_deref(), Some("review-prompt"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_terminal_prompt_output_marks_worker_blocked_with_prompt_context() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"Do you want to allow this command? cargo test\r\n".to_vec(),
            generation,
        })
        .expect("queue pane output");

    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::TerminalPrompt
            );
            assert_eq!(info.task_id.as_deref(), Some("T-output"));
            assert_eq!(info.request_id.as_deref(), Some("busy-prompt"));
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("Do you want to allow this command? cargo test")
            );
            assert!(
                info.excerpt
                    .as_deref()
                    .is_some_and(|excerpt| excerpt.contains("Do you want to allow"))
            );
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_terminal_prompt_excerpt_strips_ansi_escape_sequences() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data:
                b"\x1b[31mred context\x1b[0m\r\nDo you want to allow this command? cargo test\r\n"
                    .to_vec(),
            generation,
        })
        .expect("queue pane output");

    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            let excerpt = info.excerpt.as_deref().expect("excerpt");
            assert!(excerpt.contains("red context"));
            assert!(!excerpt.contains('\u{1b}'));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_unrelated_output_does_not_reblock_after_terminal_prompt_recovery() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });
    pane.append_output(b"permission request: allow bash ls\r\n")
        .expect("append historical permission prompt output");

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission resolution activity");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"Running tests...\r\n".to_vec(),
            generation,
        })
        .expect("queue unrelated pane output");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state, got {other:?}"),
    }
}

#[test]
fn test_permission_request_output_upgrades_to_structured_request_and_restores_on_approval() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"permission request: allow bash ls\r\n".to_vec(),
            generation,
        })
        .expect("queue permission request output");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.command_or_tool.as_deref(), Some("allow bash ls"));
            assert_eq!(info.request_id, None);
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.request_id.as_deref(), Some("perm-1"));
            assert_eq!(info.command_or_tool.as_deref(), Some("allow bash ls"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission approval activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state after approval, got {other:?}"),
    }
}

#[test]
fn test_pty_only_permission_request_restores_on_approval_without_structured_request() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"permission request: allow bash ls\r\n".to_vec(),
            generation,
        })
        .expect("queue permission request output");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission approval activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state after approval, got {other:?}"),
    }
}

#[test]
fn test_pty_only_permission_request_fallback_survives_unrelated_activity_until_resolution() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"permission request: allow bash ls\r\n".to_vec(),
            generation,
        })
        .expect("queue permission request output");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: std::time::Instant::now(),
                tool_id: None,
                tool_name: None,
                status: Some("started".to_string()),
                message: Some("unrelated operation".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue unrelated activity");
    let (_bytes, _events) = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue late permission approval activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state after late approval, got {other:?}"),
    }
}

#[test]
fn test_pty_only_permission_request_fallback_expires_without_resolution() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"permission request: allow bash ls\r\n".to_vec(),
            generation,
        })
        .expect("queue permission request output");
    let (_bytes, _events) = mux.poll_batch();
    mux.get_mut("codex-worker")
        .expect("worker pane exists")
        .expire_permission_resolution_fallback_for_test();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue expired permission approval activity");
    let (_bytes, _events) = mux.poll_batch();

    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));
}

#[test]
fn test_terminal_prompt_permission_resolution_restores_busy_turn_without_structured_request() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"Do you want to allow this command? cargo test\r\n".to_vec(),
            generation,
        })
        .expect("queue terminal prompt output");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-allow".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission resolution activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state after approval, got {other:?}"),
    }
}

#[test]
fn test_generic_terminal_prompt_upgrades_to_structured_request_and_restores_on_approval() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"This command requires approval\r\n".to_vec(),
            generation,
        })
        .expect("queue generic terminal prompt output");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::TerminalPrompt
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("This command requires approval")
            );
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue structured permission request activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.request_id.as_deref(), Some("perm-1"));
            assert_eq!(info.command_or_tool.as_deref(), Some("allow bash ls"));
        }
        other => panic!("expected upgraded blocked pane state, got {other:?}"),
    }

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission approval activity");
    let (_bytes, _events) = mux.poll_batch();

    match mux
        .get("codex-worker")
        .expect("worker pane exists")
        .pane_state()
    {
        Some(PaneState::Busy { prompt_id, .. }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
        }
        other => panic!("expected busy pane state after approval, got {other:?}"),
    }
}

#[tokio::test]
async fn test_permission_request_upgrade_publishes_blocked_state_refresh_event() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"permission request: allow bash ls\r\n".to_vec(),
            generation,
        })
        .expect("queue permission request output");
    let (_bytes, _events) = mux.poll_batch();

    for _ in 0..4 {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), rx.next_event()).await;
    }

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");
    let (_bytes, _events) = mux.poll_batch();

    let mut saw_refresh = false;
    for _ in 0..8 {
        let Ok(Ok(Some(runtime_event))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.next_event()).await
        else {
            continue;
        };

        if matches!(
            runtime_event.kind,
            brehon_types::RuntimeEventKind::PaneStateChanged(ref changed)
                if runtime_event.meta.pane_id == "codex-worker"
                    && changed.previous == Some(brehon_types::RuntimePaneState::Blocked)
                    && changed.current == brehon_types::RuntimePaneState::Blocked
                    && changed
                        .blocked
                        .as_ref()
                        .is_some_and(|blocked| {
                            blocked.kind
                                == brehon_types::RuntimePaneBlockKind::PermissionRequest
                                && blocked.request_id.as_deref() == Some("perm-1")
                        })
        ) {
            saw_refresh = true;
            break;
        }
    }

    assert!(
        saw_refresh,
        "upgrading a PTY-detected permission block should republish blocked details"
    );
}

#[test]
fn test_gateway_delivery_busy_does_not_set_tool_flag_on_blocked_pane() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    mux.get_mut("codex-worker")
        .expect("worker pane exists")
        .set_pane_blocked(
            brehon_types::RuntimePaneBlockInfo {
                kind: brehon_types::RuntimePaneBlockKind::TerminalPrompt,
                summary: "terminal prompt blocked automatic recovery: requires approval"
                    .to_string(),
                command_or_tool: Some("This command requires approval".to_string()),
                request_id: Some("busy-prompt".to_string()),
                task_id: Some("T-output".to_string()),
                excerpt: None,
            },
            std::time::Instant::now(),
        );

    mux.mark_gateway_delivery_busy(
        "codex-worker",
        brehon_types::PromptId::new("gateway-busy"),
        generation,
        std::time::Instant::now(),
    );

    let pane = mux.get("codex-worker").expect("worker pane exists");
    assert!(!pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Blocked { .. })));
}

#[test]
fn test_gateway_turn_operation_keeps_pane_busy_after_tool_completion() {
    let pane_id = "kimi-worker";
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::worker(
        pane_id,
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create kimi worker pane");
    pane.register_gateway_session_spawn("session-1".to_string());
    let generation = pane.current_generation();
    let now = std::time::Instant::now();
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("prompt-1"),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    mux.add_pane(pane);

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: now,
                tool_id: None,
                tool_name: None,
                status: Some("started".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue turn start");
    let _ = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: now,
                tool_id: Some("tool-1".to_string()),
                tool_name: Some("Shell".to_string()),
                status: Some("started".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue tool start");
    let _ = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: now,
                tool_id: Some("tool-1".to_string()),
                tool_name: Some("Shell".to_string()),
                status: Some("completed".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue tool completion");
    let _ = mux.poll_batch();

    assert_eq!(mux.active_gateway_operations.get(pane_id).copied(), Some(1));
    let pane = mux.get(pane_id).expect("pane exists");
    assert!(pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Busy { .. })));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: now,
                tool_id: None,
                tool_name: None,
                status: Some("completed".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue turn completion");
    let _ = mux.poll_batch();

    assert!(!mux.active_gateway_operations.contains_key(pane_id));
    let pane = mux.get(pane_id).expect("pane exists");
    assert!(!pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Ready { .. })));
}

#[test]
fn test_gateway_turn_completion_clears_orphaned_tool_lock() {
    let pane_id = "kimi-worker";
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::worker(
        pane_id,
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create kimi worker pane");
    pane.register_gateway_session_spawn("session-1".to_string());
    let generation = pane.current_generation();
    let now = std::time::Instant::now();
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("prompt-1"),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    mux.add_pane(pane);

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: now,
                tool_id: None,
                tool_name: None,
                status: Some("started".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue turn start");
    let _ = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: now,
                tool_id: Some("tool-orphaned".to_string()),
                tool_name: Some("task".to_string()),
                status: Some("started".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue tool start");
    let _ = mux.poll_batch();

    let pane = mux.get(pane_id).expect("pane exists");
    assert!(pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Busy { .. })));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            generation,
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: now,
                tool_id: None,
                tool_name: None,
                status: Some("completed".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
        })
        .expect("queue turn completion");
    let _ = mux.poll_batch();

    assert!(!mux.active_gateway_operations.contains_key(pane_id));
    let pane = mux.get(pane_id).expect("pane exists");
    assert!(!pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Ready { .. })));
}

#[test]
fn test_terminal_prompt_output_detects_split_keyword_across_chunks() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"This command requires".to_vec(),
            generation,
        })
        .expect("queue first pane output chunk");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b" approval\r\n".to_vec(),
            generation,
        })
        .expect("queue second pane output chunk");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::TerminalPrompt
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("This command requires approval")
            );
            assert_eq!(info.request_id.as_deref(), Some("busy-prompt"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_provider_context_limit_output_blocks_worker() {
    let (mut mux, generation) = mux_with_busy_worker_for_output("minimax-worker");

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "minimax-worker".to_string(),
            data: b"API Error: 400 invalid params, context window exceeds limit (2013)\r\n"
                .to_vec(),
            generation,
        })
        .expect("queue context limit output");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("minimax-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::ProviderContextLimit
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("API Error: 400 invalid params, context window exceeds limit (2013)")
            );
            assert_eq!(info.request_id.as_deref(), Some("busy-prompt"));
            assert_eq!(info.task_id.as_deref(), Some("T-output"));
            assert!(
                info.summary
                    .starts_with("provider context limit blocked automatic recovery:")
            );
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_provider_context_limit_output_detects_split_phrase_across_chunks() {
    let (mut mux, generation) = mux_with_busy_worker_for_output("minimax-worker");

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "minimax-worker".to_string(),
            data: b"API Error: 400 invalid params, context window".to_vec(),
            generation,
        })
        .expect("queue first context limit output chunk");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("minimax-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "minimax-worker".to_string(),
            data: b" exceeds limit (2013)\r\n".to_vec(),
            generation,
        })
        .expect("queue second context limit output chunk");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("minimax-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::ProviderContextLimit
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("API Error: 400 invalid params, context window exceeds limit (2013)")
            );
            assert_eq!(info.request_id.as_deref(), Some("busy-prompt"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_provider_context_limit_output_detects_kimi_model_token_limit() {
    let (mut mux, generation) = mux_with_busy_worker_for_output("kimi-worker");

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "kimi-worker".to_string(),
            data: b"API Error: 400 Invalid request: Your request exceeded model token limit: 262144 (requested: 262328)\r\n".to_vec(),
            generation,
        })
        .expect("queue kimi context limit output");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("kimi-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::ProviderContextLimit
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some(
                    "API Error: 400 Invalid request: Your request exceeded model token limit: 262144"
                )
            );
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_context_usage_status_line_does_not_block_worker() {
    let (mut mux, generation) = mux_with_busy_worker_for_output("minimax-worker");

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "minimax-worker".to_string(),
            data: b"224k/200k (111%)\r\n100% context used\r\n".to_vec(),
            generation,
        })
        .expect("queue context usage status output");
    let (_bytes, _events) = mux.poll_batch();

    assert!(matches!(
        mux.get("minimax-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));
}

#[test]
fn test_regular_context_output_does_not_rescan_stale_prompt_text() {
    let (mut mux, generation) = mux_with_busy_worker_for_output("codex-worker");
    mux.get_mut("codex-worker")
        .expect("worker pane exists")
        .append_output(b"Permission request: cargo test\r\n")
        .expect("seed stale prompt-like viewport text");

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"Review handoff context:\r\nFocused context for the current task\r\n".to_vec(),
            generation,
        })
        .expect("queue ordinary context output");
    let (_bytes, _events) = mux.poll_batch();

    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));
}

#[test]
fn test_terminal_prompt_output_detects_permission_request_when_only_trailing_line_arrives() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });
    pane.append_output(b"Permission request: cargo test\r\n")
        .expect("seed prior permission prompt output");
    pane.terminal_prompt_prefilter_tail = "Permission request: cargo test\r\n".to_string();

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"[y/N]\r\n".to_vec(),
            generation,
        })
        .expect("queue trailing prompt chunk");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.command_or_tool.as_deref(), Some("cargo test"));
            assert_eq!(info.task_id.as_deref(), Some("T-output"));
            assert_eq!(info.request_id, None);
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_auto_approved_gemini_permission_status_does_not_block_pane() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "gemini-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Gemini),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create gemini worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("gemini-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("gemini-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "gemini-worker".to_string(),
            data: b"\x1b[2mAuto-approved Gemini permission request: allow_once\x1b[0m\r\n".to_vec(),
            generation,
        })
        .expect("queue auto-approved permission status");
    let (_bytes, _events) = mux.poll_batch();

    assert!(matches!(
        mux.get("gemini-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));
}

#[test]
fn test_rejected_gemini_permission_status_does_not_block_pane() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "gemini-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Gemini),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create gemini worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("gemini-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("gemini-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "gemini-worker".to_string(),
            data: b"\x1b[31mRejected Gemini permission request\x1b[0m\r\n".to_vec(),
            generation,
        })
        .expect("queue rejected permission status");
    let (_bytes, _events) = mux.poll_batch();

    assert!(matches!(
        mux.get("gemini-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));
}

#[test]
fn test_terminal_prompt_output_detects_split_signal_token_across_chunks() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_task_context(crate::TaskContextSnapshot {
        task_id: "T-output".to_string(),
        title: "Output task".to_string(),
        status: brehon_types::task::TaskStatus::InProgress,
        completion_mode: None,
        merge_target: None,
        parent_id: None,
        epic_branch: None,
        epic_worktree: None,
        blocked_reason: None,
        updated_at: std::time::Instant::now(),
    });

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"Do you want to al".to_vec(),
            generation,
        })
        .expect("queue first pane output chunk");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Busy { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "codex-worker".to_string(),
            data: b"low this command? cargo test\r\n".to_vec(),
            generation,
        })
        .expect("queue second pane output chunk");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::TerminalPrompt
            );
            assert_eq!(
                info.command_or_tool.as_deref(),
                Some("Do you want to allow this command? cargo test")
            );
            assert_eq!(info.request_id.as_deref(), Some("busy-prompt"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_permission_resolution_with_matching_request_restores_busy_turn() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();
    let now = std::time::Instant::now();
    let pane = mux.get_mut("codex-worker").expect("worker pane exists");
    pane.set_pane_state(PaneState::Busy {
        prompt_id: brehon_types::PromptId::new("busy-prompt".to_string()),
        generation,
        delivered_at: now,
        last_activity_at: now,
    });
    pane.set_tool_executing(true);

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Blocked { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission resolution activity");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    assert!(pane.is_tool_executing());
    match pane.pane_state() {
        Some(PaneState::Busy {
            prompt_id,
            generation: busy_generation,
            ..
        }) => {
            assert_eq!(prompt_id.to_string(), "busy-prompt");
            assert_eq!(*busy_generation, generation);
        }
        other => panic!("expected busy pane state, got {other:?}"),
    }
}

#[test]
fn test_permission_resolution_with_non_matching_request_keeps_blocked_state() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    mux.add_pane(pane);

    let generation = mux
        .get("codex-worker")
        .expect("worker pane exists")
        .current_generation();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-1".to_string()),
                tool_name: None,
                status: None,
                message: Some("allow bash ls".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue permission request activity");
    let (_bytes, _events) = mux.poll_batch();

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Permission,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("perm-2".to_string()),
                tool_name: None,
                status: Some("approved".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue mismatched permission resolution activity");
    let (_bytes, _events) = mux.poll_batch();

    let pane = mux.get("codex-worker").expect("worker pane exists");
    match pane.pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(
                info.kind,
                brehon_types::RuntimePaneBlockKind::PermissionRequest
            );
            assert_eq!(info.request_id.as_deref(), Some("perm-1"));
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }
}

#[test]
fn test_set_pane_blocked_warns_when_replacing_existing_block_reason() {
    let mut pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker pane");
    pane.set_pane_blocked(
        brehon_types::RuntimePaneBlockInfo {
            kind: brehon_types::RuntimePaneBlockKind::PermissionRequest,
            summary: "first block".to_string(),
            command_or_tool: Some("first command".to_string()),
            request_id: Some("perm-1".to_string()),
            task_id: Some("T-first".to_string()),
            excerpt: None,
        },
        std::time::Instant::now(),
    );

    let logs = capture_logs(|| {
        pane.set_pane_blocked(
            brehon_types::RuntimePaneBlockInfo {
                kind: brehon_types::RuntimePaneBlockKind::TerminalPrompt,
                summary: "second block".to_string(),
                command_or_tool: Some("second command".to_string()),
                request_id: Some("prompt-2".to_string()),
                task_id: Some("T-second".to_string()),
                excerpt: Some("blocked excerpt".to_string()),
            },
            std::time::Instant::now(),
        );
    });

    assert!(
        logs.contains("Overwriting existing blocked pane state"),
        "expected overwrite warning, got logs: {logs}"
    );
}

#[tokio::test]
async fn test_recycle_bumps_generation_and_flushes_worker_runtime_state() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("codex-worker").expect("worker exists");
        pane.register_gateway_session_spawn("session-1".to_string());
        pane.set_tool_executing(true);
        pane.ensure_activity_buffer();
        pane.activity_buffer_mut()
            .expect("activity buffer")
            .start_tool("tool-1".to_string(), "ReadFile".to_string());
    }
    mux.active_gateway_operations
        .insert("codex-worker".to_string(), 2);
    mux.queue_delayed_prompt(
        "codex-worker",
        "queued stale prompt".to_string(),
        Some("supervisor".to_string()),
        std::time::Instant::now(),
        None,
    );

    let generation = mux
        .recycle("codex-worker", "test recycle for stale worker")
        .await;

    assert_eq!(generation, crate::pane::Generation(2));
    assert!(!mux.active_gateway_operations.contains_key("codex-worker"));
    assert!(mux.pending_delayed_prompts.is_empty());

    let pane = mux.get("codex-worker").expect("worker exists");
    assert_eq!(pane.current_generation(), generation);
    assert!(pane.gateway_session_id().is_none());
    assert!(!pane.is_tool_executing());
    assert!(matches!(pane.pane_state(), Some(PaneState::Ready { .. })));
}

#[tokio::test]
async fn test_recycle_is_idempotent_without_intervening_activity() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    mux.add_pane(pane);
    mux.get_mut("codex-worker")
        .expect("worker exists")
        .register_gateway_session_spawn("worker-session".to_string());
    let baseline_generation = mux
        .get("codex-worker")
        .expect("worker exists")
        .current_generation();

    let first = mux.recycle("codex-worker", "first recycle").await;
    let second = mux
        .recycle("codex-worker", "idempotent replay recycle")
        .await;
    assert_eq!(
        first,
        crate::pane::Generation(baseline_generation.0.saturating_add(1))
    );
    assert_eq!(second, first);
    let pane = mux.get("codex-worker").expect("worker exists");
    assert!(pane.gateway_session_id().is_none());
    let viewport = pane.dump_viewport().expect("dump viewport");
    assert_eq!(
        viewport
            .matches("Brehon reset worker session after a model context error.")
            .count(),
        1,
        "backend ACP session reset should happen once for idempotent replay"
    );

    {
        let pane = mux.get_mut("codex-worker").expect("worker exists");
        pane.record_output_activity();
    }

    let third = mux
        .recycle("codex-worker", "recycle after new activity")
        .await;
    assert_eq!(third, crate::pane::Generation(first.0.saturating_add(1)));
}

#[tokio::test]
async fn test_quarantine_transitions_pane_to_dead_and_rejects_prompt_delivery() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::worker(
        "codex-worker",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    mux.add_pane(pane);

    let reason = DeathReason::Quarantined("manual quarantine".to_string());
    let outcome = mux.quarantine("codex-worker", reason.clone());
    assert_eq!(
        outcome,
        QuarantineOutcome {
            new_reason: reason.clone(),
            was_already_dead: false,
            prior_reason: None,
        }
    );

    assert!(matches!(
        mux.get("codex-worker")
            .expect("worker exists")
            .pane_state(),
        Some(PaneState::Dead {
            reason: state_reason,
            ..
        }) if *state_reason == reason
    ));

    let attempt = mux
        .attempt_prompt_delivery("codex-worker", "follow-up prompt", Some("supervisor"))
        .await
        .expect("attempt prompt delivery");
    assert_eq!(attempt, PromptDeliveryAttempt::Rejected { reason });
}

#[test]
fn test_quarantine_is_idempotent_and_preserves_original_reason() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::director("director", 24, 80).expect("create director pane");
    mux.add_pane(pane);

    let original_reason = DeathReason::Quarantined("first reason".to_string());
    let first = mux.quarantine("director", original_reason.clone());
    assert_eq!(
        first,
        QuarantineOutcome {
            new_reason: original_reason.clone(),
            was_already_dead: false,
            prior_reason: None,
        }
    );

    let second = mux.quarantine("director", DeathReason::SessionDropped);
    assert_eq!(
        second,
        QuarantineOutcome {
            new_reason: original_reason.clone(),
            was_already_dead: true,
            prior_reason: Some(original_reason.clone()),
        }
    );
    assert!(matches!(
        mux.get("director")
            .expect("director exists")
            .pane_state(),
        Some(PaneState::Dead {
            reason: state_reason,
            ..
        }) if *state_reason == original_reason
    ));
}

#[tokio::test]
async fn test_quarantine_dead_state_survives_poll_and_poll_batch() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::director("director", 24, 80).expect("create director pane");
    mux.add_pane(pane);

    let reason = DeathReason::Quarantined("manual quarantine".to_string());
    mux.quarantine("director", reason.clone());

    let _ = mux.poll();
    assert!(matches!(
        mux.get("director")
            .expect("director exists")
            .pane_state(),
        Some(PaneState::Dead {
            reason: state_reason,
            ..
        }) if *state_reason == reason
    ));

    let (_total_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get("director")
            .expect("director exists")
            .pane_state(),
        Some(PaneState::Dead {
            reason: state_reason,
            ..
        }) if *state_reason == reason
    ));

    let attempt = mux
        .attempt_prompt_delivery("director", "should be rejected", None)
        .await
        .expect("attempt prompt delivery");
    assert_eq!(attempt, PromptDeliveryAttempt::Rejected { reason });
}

#[test]
fn test_local_pty_exit_marks_pane_dead_and_idle() {
    use brehon_pty::{Pty, PtyConfig};
    use std::time::Duration;

    let mut mux = Mux::new(24, 80);
    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "exit 7".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("exiting-worker", config).expect("spawn exiting pty");
    let pane = Pane::with_pty_cli(
        "exiting-worker",
        PaneKind::Worker,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Codex),
    )
    .expect("create exiting worker pane");
    mux.add_pane(pane);

    let mut saw_exit = false;
    for _ in 0..500 {
        let (_bytes, events) = mux.poll_batch();
        if events.iter().any(|event| {
            matches!(
                event,
                MuxEvent::PaneExited {
                    pane_id,
                    exit_code
                } if pane_id == "exiting-worker" && *exit_code == Some(7)
            )
        }) {
            saw_exit = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(saw_exit, "expected PTY exit event within 5s");
    let pane = mux.get("exiting-worker").expect("worker exists");
    assert!(pane.has_exited());
    assert_eq!(pane.exit_code(), Some(7));
    assert!(!pane.is_tool_executing());
    assert!(matches!(
        pane.pane_state(),
        Some(PaneState::Dead {
            reason: DeathReason::SessionDropped,
            ..
        })
    ));
}

#[test]
fn test_quarantine_unknown_pane_returns_lenient_outcome() {
    let mut mux = Mux::new(24, 80);
    let reason = DeathReason::Quarantined("missing pane".to_string());

    let outcome = mux.quarantine("missing-pane", reason.clone());
    assert_eq!(
        outcome,
        QuarantineOutcome {
            new_reason: reason,
            was_already_dead: false,
            prior_reason: None,
        }
    );
}

#[tokio::test]
async fn test_reset_worker_gateway_session_rejects_missing_isolated_cwd() {
    let project_root = super::fresh_temp_dir("brehon-mux-reset-missing-cwd");
    let worker_cwd = super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/worker-1");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert("worker-1".to_string(), worker_cwd.clone());

    let mut mux = Mux::factory(MuxConfig {
        cwd: project_root.clone(),
        worktree_isolation: true,
        worker_cwds,
        supervisor_cwd: Some(super::setup_fake_linked_worktree(
            &project_root,
            ".brehon/worktrees/supervisor/claude-code",
        )),
        workers: 1,
        worker_names: vec!["worker-1".to_string()],
        supervisor_name: "claude-code".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    })
    .expect("create mux");

    std::fs::remove_dir_all(&worker_cwd).expect("remove worker worktree");

    let err = mux
        .reset_worker_gateway_session("worker-1")
        .await
        .expect_err("missing isolated cwd should fail reset");

    assert!(err.to_string().contains("isolated cwd"));
    assert!(err.to_string().contains("does not exist"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_deliver_prompt_preserves_explicit_sender_for_claude_teams() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");

    let mut mux = Mux::new(24, 80);
    let teams = TeamsManager::new_for_test("test-session", home.clone());
    teams
        .init_team_config(
            "claude-code",
            &["worker-1".to_string()],
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
        "claude-code",
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
        pane.set_inbox_nudge_not_before(None);
    }

    mux.deliver_prompt(
        "worker-1",
        "You have been assigned task T-1",
        Some("claude-code"),
    )
    .await
    .expect("deliver prompt");

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("worker-1")
        .unwrap();
    let payload = std::fs::read_to_string(inbox_path).expect("read worker inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");

    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "You have been assigned task T-1")
    );
    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["from"] == "claude-code")
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_noise_filtering_parity_tool_brehon_bootstrap() {
    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon_task".to_string(),
        details: None,
    };
    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_noise_filtering_parity_tool_brehon_success() {
    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon_agent".to_string(),
        status: "completed".to_string(),
        details: None,
    };
    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_noise_filtering_parity_tool_failure_visible() {
    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon_task".to_string(),
        status: "failed".to_string(),
        details: None,
    };
    let output = format_acp_session_event(&event).expect("should format");
    let text = String::from_utf8(output).expect("valid utf8");
    assert!(text.contains("failed"));
}

#[test]
fn test_noise_filtering_parity_progress_idle() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "session idle".to_string(),
        percent: None,
    };
    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_noise_filtering_parity_progress_mcp_ready() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "MCP server brehon: ready".to_string(),
        percent: None,
    };
    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_noise_filtering_parity_progress_mcp_failure_visible() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "MCP server tools: failed (connection refused)".to_string(),
        percent: None,
    };
    let output = format_acp_session_event(&event).expect("should format");
    let text = String::from_utf8(output).expect("valid utf8");
    assert!(text.contains("failed"));
}

#[test]
fn test_noise_filtering_parity_operation_response() {
    let event = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        operation: "turn".to_string(),
        success: true,
    };
    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_noise_filtering_parity_operation_failure_visible() {
    let event = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        operation: "turn".to_string(),
        success: false,
    };
    let output = format_acp_session_event(&event).expect("should format");
    let text = String::from_utf8(output).expect("valid utf8");
    assert!(text.contains("failed"));
}

#[test]
fn test_output_preserved_not_filtered() {
    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-1"),
        text: "Working on implementation...\n".to_string(),
    };
    let output = format_acp_session_event(&event).expect("should format");
    let text = String::from_utf8(output).expect("valid utf8");
    assert!(text.contains("Working on implementation"));
}

#[test]
fn test_permission_request_visible() {
    let event = brehon_acp::updates::SessionEvent::PermissionRequest {
        session_id: brehon_types::SessionId::new("s-1"),
        permission_id: "perm-1".to_string(),
        action: "read_file".to_string(),
        details: None,
    };
    let output = format_acp_session_event(&event).expect("should format");
    let text = String::from_utf8(output).expect("valid utf8");
    assert!(text.contains("permission"));
}

#[test]
fn test_activity_filtering_matches_format_acp_for_brehon_bootstrap() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon_agent".to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&event).is_none());
    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_activity_filtering_matches_format_acp_for_hyphenated_brehon_bootstrap() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon-agent".to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&event).is_none());
    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_activity_filtering_matches_format_acp_for_kimi_prefixed_brehon_tool() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: r#"task: {"action":"complete","id":"T-123"}"#.to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&event).is_none());
    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_activity_filtering_matches_format_acp_for_brehon_success() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "t-1".to_string(),
        tool_name: "brehon_verification".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&event).is_none());
    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_activity_entry_preserves_tool_details_for_expansion() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-1"),
        tool_id: "tool-1".to_string(),
        tool_name: "file_change".to_string(),
        details: Some(serde_json::json!({
            "input": {
                "path": "crates/brehon-mux/src/mux/format.rs",
                "operation": "edit"
            }
        })),
    };

    let entry =
        session_event_to_activity_entry(&event).expect("tool details should stay high signal");

    assert_eq!(entry.tool_name, Some("file_change".to_string()));
    let message = entry.message.expect("tool detail message");
    assert!(message.contains("input"));
    assert!(message.contains("crates/brehon-mux/src/mux/format.rs"));
    assert!(!message.contains("tool-1"));
}

#[test]
fn test_activity_filtering_matches_format_acp_for_progress_idle() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "Codex thread status: idle".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
    assert!(session_event_to_activity_entry(&event).is_none());
}

#[test]
fn test_activity_filtering_matches_format_acp_for_output() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-1"),
        text: "Hello world\n".to_string(),
    };

    assert!(format_acp_session_event(&event).is_some());
    let entry = session_event_to_activity_entry(&event).expect("should convert");
    assert_eq!(entry.kind, crate::pane::activity::ActivityKind::Output);
}

#[test]
fn test_activity_entry_output_has_chunks() {
    use crate::mux::session_event_to_activity_entry;

    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-1"),
        text: "line one\nline two\n".to_string(),
    };

    let entry = session_event_to_activity_entry(&event).expect("should convert");
    assert!(entry.output_chunks.is_some());
    let chunks = entry.output_chunks.unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].contains("line one"));
}

#[test]
fn test_non_gateway_panes_no_activity_buffer() {
    let mut mux = Mux::new(24, 80);

    let pane = Pane::director("director", 24, 80).expect("create director pane");
    mux.add_pane(pane);

    let p = mux.get("director").expect("director exists");
    assert!(!p.is_gateway_backed());
    assert!(p.activity_buffer().is_none());
}

#[test]
fn test_gateway_panes_have_activity_buffer() {
    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        "codex-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");

    assert!(pane.is_gateway_backed());
    assert!(pane.gateway_spawn_config().is_some());

    mux.add_pane(pane);

    let p = mux.get_mut("codex-worker").expect("worker exists");
    p.ensure_activity_buffer();
    assert!(p.activity_buffer().is_some());
}

#[test]
fn test_claude_panes_no_activity_buffer() {
    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        "claude-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create claude worker");

    assert!(!pane.is_gateway_backed());

    mux.add_pane(pane);

    let p = mux.get("claude-worker").expect("worker exists");
    assert!(p.activity_buffer().is_none());
}

#[test]
fn test_activity_event_propagates_to_pane() {
    use crate::MuxEvent;
    use crate::pane::activity::ActivityKind;

    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        "codex-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");

    mux.add_pane(pane);

    let entry = crate::pane::activity::ActivityEntry {
        kind: ActivityKind::ToolCall,
        ingested_at: std::time::Instant::now(),
        tool_id: Some("tool-1".to_string()),
        tool_name: Some("bash".to_string()),
        status: Some("started".to_string()),
        message: None,
        output_chunks: None,
        duration: None,
    };

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "codex-worker".to_string(),
            entry: entry.clone(),
            generation: crate::pane::Generation::default(),
        })
        .expect("send activity event");

    let (_bytes, _events) = mux.poll_batch();

    let p = mux.get("codex-worker").expect("worker exists");
    assert!(p.activity_buffer().is_some());
    assert_eq!(p.activity_buffer().unwrap().len(), 1);
}

#[tokio::test]
async fn test_stale_operation_completed_event_is_dropped_after_recycle() {
    use crate::MuxEvent;
    use crate::pane::activity::{ActivityEntry, ActivityKind};

    let pane_id = "codex-worker";
    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        pane_id,
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut(pane_id).expect("worker exists");
        for idx in 1..=5 {
            pane.register_gateway_session_spawn(format!("session-{idx}"));
        }
    }

    let stale_generation = mux
        .get(pane_id)
        .expect("worker exists")
        .current_generation();
    assert_eq!(stale_generation, crate::pane::Generation(5));

    let current_generation = mux.recycle(pane_id, "generation-fence regression").await;
    assert_eq!(current_generation, crate::pane::Generation(6));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: std::time::Instant::now(),
                tool_id: None,
                tool_name: None,
                status: Some("started".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation: current_generation,
        })
        .expect("queue current-generation operation started");

    let (_bytes, started_events) = mux.poll_batch();
    assert!(
        started_events.iter().any(|event| {
            matches!(
                event,
                MuxEvent::ActivityEvent {
                    pane_id,
                    generation,
                    entry,
                    ..
                } if pane_id == "codex-worker"
                    && *generation == current_generation
                    && entry.status.as_deref() == Some("started")
            )
        }),
        "expected current-generation operation start event to be applied"
    );
    assert_eq!(mux.active_gateway_operations.get(pane_id).copied(), Some(1));
    assert!(mux.get(pane_id).expect("worker exists").is_tool_executing());
    assert!(matches!(
        mux.get(pane_id).expect("worker exists").pane_state(),
        Some(PaneState::Busy { .. })
    ));

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: std::time::Instant::now(),
                tool_id: None,
                tool_name: None,
                status: Some("completed".to_string()),
                message: Some("turn".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation: stale_generation,
        })
        .expect("queue stale operation completion");

    let (_bytes, stale_events) = mux.poll_batch();
    assert!(
        stale_events.is_empty(),
        "expected stale generation event to be dropped at ingestion"
    );
    assert_eq!(mux.active_gateway_operations.get(pane_id).copied(), Some(1));
    assert!(mux.get(pane_id).expect("worker exists").is_tool_executing());
    assert!(matches!(
        mux.get(pane_id).expect("worker exists").pane_state(),
        Some(PaneState::Busy { .. })
    ));
}

#[tokio::test]
async fn test_r7_stale_operation_completed_after_recycle_preserves_ready_state() {
    use crate::MuxEvent;
    use crate::pane::activity::{ActivityEntry, ActivityKind};
    use brehon_types::PromptId;

    let pane_id = "codex-worker-r7";
    let mut mux = Mux::new(24, 80);

    let pane = Pane::worker(
        pane_id,
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut(pane_id).expect("worker exists");
        for idx in 1..=4 {
            pane.register_gateway_session_spawn(format!("session-r7-{idx}"));
        }
        let now = std::time::Instant::now();
        pane.set_pane_state(PaneState::Busy {
            prompt_id: PromptId::new("r7-pre-recycle-busy".to_string()),
            generation: pane.current_generation(),
            delivered_at: now - std::time::Duration::from_secs(2),
            last_activity_at: now - std::time::Duration::from_secs(1),
        });
        pane.set_tool_executing(true);
    }

    // The gateway session spawns above advance the pane generation before recycle;
    // capture that pre-recycle generation so the stale completion event can be
    // fenced against the recycled ready state below.
    let stale_generation = mux
        .get(pane_id)
        .expect("worker exists")
        .current_generation();
    assert!(matches!(
        mux.get(pane_id).expect("worker exists").pane_state(),
        Some(PaneState::Busy { generation, .. }) if *generation == stale_generation
    ));

    let current_generation = mux
        .recycle(pane_id, "r7 stale completion after recycle")
        .await;
    let expected_generation = crate::pane::Generation(stale_generation.0.saturating_add(1));
    assert_eq!(current_generation, expected_generation);

    let recycled_since = match mux.get(pane_id).expect("worker exists").pane_state() {
        Some(PaneState::Ready { since }) => *since,
        other => panic!("expected Ready after recycle, got {other:?}"),
    };

    let logs = capture_debug_logs(|| {
        mux.event_tx
            .try_send(MuxEvent::ActivityEvent {
                pane_id: pane_id.to_string(),
                entry: ActivityEntry {
                    kind: ActivityKind::Operation,
                    ingested_at: std::time::Instant::now(),
                    tool_id: None,
                    tool_name: None,
                    status: Some("completed".to_string()),
                    message: Some("turn".to_string()),
                    output_chunks: None,
                    duration: None,
                },
                generation: stale_generation,
            })
            .expect("queue stale operation completion");

        let (_bytes, stale_events) = mux.poll_batch();
        assert!(
            !stale_events.iter().any(|event| {
                matches!(
                    event,
                    MuxEvent::ActivityEvent {
                        pane_id: event_pane_id,
                        generation,
                        entry
                    } if event_pane_id == pane_id
                        && *generation == stale_generation
                        && entry.kind == ActivityKind::Operation
                        && entry.status.as_deref() == Some("completed")
                )
            }),
            "expected stale generation completion event to be dropped"
        );
    });

    let ready_since_after_stale = match mux.get(pane_id).expect("worker exists").pane_state() {
        Some(PaneState::Ready { since }) => *since,
        other => panic!("expected Ready after stale completion drop, got {other:?}"),
    };
    assert!(
        ready_since_after_stale >= recycled_since,
        "ready timestamp moved backwards after stale event drop"
    );
    assert!(!mux.get(pane_id).expect("worker exists").is_tool_executing());
    assert!(!mux.active_gateway_operations.contains_key(pane_id));

    let stale_drop_count = logs
        .lines()
        .filter(|line| {
            line.contains("DEBUG") && line.contains("dropped stale event for old generation")
        })
        .count();
    assert_eq!(
        stale_drop_count, 1,
        "expected one debug stale-drop line, got logs: {logs}"
    );
}

#[tokio::test]
async fn test_acp_event_bridge_tags_activity_with_current_generation() {
    use crate::MuxEvent;
    use crate::pane::Generation;

    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::worker(
        "codex-worker",
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    pane.register_gateway_session_spawn("session-1".to_string());
    let expected_generation = pane.current_generation();
    mux.add_pane(pane);

    let (tx, rx) = tokio::sync::mpsc::channel(8);
    mux.spawn_acp_event_bridge("codex-worker", rx);

    tx.send(brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-generation"),
        tool_id: "tool-1".to_string(),
        tool_name: "ReadFile".to_string(),
        details: None,
    })
    .await
    .expect("send gateway activity event");
    drop(tx);

    let mut observed = None;
    for _ in 0..30 {
        let (_bytes, events) = mux.poll_batch();
        observed = events.into_iter().find_map(|event| match event {
            MuxEvent::ActivityEvent { generation, .. } => Some(generation),
            _ => None,
        });
        if observed.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(observed, Some(expected_generation));
    assert_eq!(observed, Some(Generation(1)));
}

#[test]
fn test_busy_worker_recovers_within_max_turn_duration_after_session_drop() {
    use crate::pane::activity::{ActivityEntry, ActivityKind};
    use brehon_types::PromptId;

    let pane_id = "codex-worker-r4";
    let mut mux = Mux::new(24, 80);
    let mut pane = Pane::worker(
        pane_id,
        std::path::PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create codex worker");
    pane.register_gateway_session_spawn("session-r4".to_string());
    let generation = pane.current_generation();
    mux.add_pane(pane);

    // Simulate ToolCallStarted entering Busy.
    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: pane_id.to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("tool-r4".to_string()),
                tool_name: Some("ReadFile".to_string()),
                status: Some("started".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue tool started activity");
    let (_bytes, _events) = mux.poll_batch();
    assert!(matches!(
        mux.get(pane_id).expect("worker pane exists").pane_state(),
        Some(PaneState::Busy { .. })
    ));

    // Simulate ACP session drop with no completion event.
    mux.get_mut(pane_id)
        .expect("worker pane exists")
        .clear_gateway_session();

    let ticker_tolerance = std::time::Duration::from_secs(1);
    let tick_at = std::time::Instant::now();
    let delivered_at = tick_at - MAX_TURN_DURATION - ticker_tolerance;
    let last_activity_at = tick_at - QUIET_THRESHOLD + std::time::Duration::from_millis(1);
    mux.get_mut(pane_id)
        .expect("worker pane exists")
        .set_pane_state(PaneState::Busy {
            prompt_id: PromptId::new("r4-stuck-busy".to_string()),
            generation,
            delivered_at,
            last_activity_at,
        });

    // A tick just before MAX_TURN_DURATION must not force readiness yet.
    let just_before_timeout =
        delivered_at + MAX_TURN_DURATION - std::time::Duration::from_millis(1);
    assert!(
        !mux.get_mut(pane_id)
            .expect("worker pane exists")
            .tick_state_machine(just_before_timeout)
    );

    let logs = capture_logs(|| {
        assert!(
            mux.get_mut(pane_id)
                .expect("worker pane exists")
                .tick_state_machine(tick_at)
        );
    });

    assert!(matches!(
        mux.get(pane_id)
            .expect("worker pane exists")
            .pane_state(),
        Some(PaneState::Ready { since }) if *since == tick_at
    ));
    assert!(
        logs.contains("WARN"),
        "expected warn-level log line, got logs: {logs}"
    );
    assert!(
        logs.contains(&format!("pane_id={pane_id}"))
            || logs.contains(&format!("pane_id=\"{pane_id}\"")),
        "expected pane_id in warning log, got logs: {logs}"
    );
}
