use crate::mux::*;
use crate::teams::{TeamsManager, TeamsPaths};
use crate::{
    ActivityEntry, ActivityKind, AgentAdapter, DeathReason, Generation, Pane, PaneState,
    SupervisorCli,
};
use brehon_ports::{RuntimeCommandPort, RuntimeEventStream};
use brehon_types::{
    PromptDeliveryMode, RuntimeCommand, RuntimeCommandKind, RuntimeCommandStatus,
    RuntimeCommandTarget, RuntimePaneKind, RuntimePaneState, RuntimePolicyContext,
};
use std::path::PathBuf;
use std::sync::Arc;

fn runtime_command(
    command_id: &str,
    pane_id: Option<&str>,
    generation: Option<u64>,
    kind: RuntimeCommandKind,
) -> RuntimeCommand {
    RuntimeCommand {
        command_id: command_id.to_string(),
        target: RuntimeCommandTarget {
            session_id: "session".to_string(),
            pane_id: pane_id.map(str::to_string),
            generation,
        },
        issued_at_ms: 1,
        kind,
    }
}

#[test]
fn test_mux_new() {
    let mux = Mux::new(24, 80);
    assert_eq!(mux.size(), (24, 80));
    assert!(mux.focused().is_none());
}

#[test]
fn test_mux_add_pane() {
    let mut mux = Mux::new(24, 80);
    let pane = Pane::director("test", 24, 80).unwrap();
    mux.add_pane(pane);

    assert!(mux.get("test").is_some());
    assert_eq!(mux.focused_id(), Some("test"));
}

#[test]
fn test_mux_focus_navigation() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 40).unwrap());
    mux.add_pane(Pane::director("pane2", 24, 40).unwrap());

    assert_eq!(mux.focused_id(), Some("pane1"));

    mux.focus_next();
    assert_eq!(mux.focused_id(), Some("pane2"));

    mux.focus_next();
    assert_eq!(mux.focused_id(), Some("pane1")); // Wraps around

    mux.focus_prev();
    assert_eq!(mux.focused_id(), Some("pane2"));
}

#[test]
fn test_pane_count() {
    let mut mux = Mux::new(24, 80);
    assert_eq!(mux.pane_count(), 0);

    mux.add_pane(Pane::director("pane1", 24, 40).unwrap());
    assert_eq!(mux.pane_count(), 1);

    mux.add_pane(Pane::director("pane2", 24, 40).unwrap());
    assert_eq!(mux.pane_count(), 2);

    mux.remove_pane("pane1");
    assert_eq!(mux.pane_count(), 1);
}

#[test]
fn test_remove_pane_focus_transfer() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 40).unwrap());
    mux.add_pane(Pane::director("pane2", 24, 40).unwrap());

    // Focus is on pane1 (first added)
    assert_eq!(mux.focused_id(), Some("pane1"));

    // Remove focused pane, focus should transfer to next
    mux.remove_pane("pane1");
    assert_eq!(mux.focused_id(), Some("pane2"));
    assert_eq!(mux.pane_count(), 1);
}

#[test]
fn test_set_and_clear_pane_review_context() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("reviewer-1", 24, 80).unwrap());

    mux.set_pane_review_context(
        "reviewer-1",
        crate::ReviewContextSnapshot {
            review_id: "R-ctx".to_string(),
            task_id: "T-ctx".to_string(),
            round: 1,
            panel_total: 3,
            panel_done: 2,
            verdict: None,
            score: None,
            findings_summary: Some("pending review".to_string()),
            updated_at: std::time::Instant::now(),
        },
    );

    let pane = mux.get("reviewer-1").expect("pane exists");
    assert_eq!(
        pane.review_context().map(|ctx| ctx.review_id.as_str()),
        Some("R-ctx")
    );
    assert_eq!(pane.review_context().map(|ctx| ctx.panel_done), Some(2));

    mux.clear_pane_review_context("reviewer-1");
    let pane = mux.get("reviewer-1").expect("pane exists");
    assert!(pane.review_context().is_none());
}

#[test]
fn test_poll_batch_applies_queued_pane_output() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "pane1".to_string(),
            data: b"hello from queue\n".to_vec(),
            generation: crate::pane::Generation::default(),
        })
        .expect("queue pane output");

    let (_bytes, events) = mux.poll_batch();
    assert!(
        events.iter().any(
            |event| matches!(event, MuxEvent::PaneOutput { pane_id, .. } if pane_id == "pane1")
        )
    );

    let viewport = mux
        .get("pane1")
        .expect("pane exists")
        .dump_viewport()
        .expect("dump viewport");
    assert!(viewport.contains("hello from queue"));
}

#[test]
fn test_runtime_event_mapping_preserves_pane_identity_and_generation() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let event = MuxEvent::PaneOutput {
        pane_id: "pane1".to_string(),
        data: b"hello".to_vec(),
        generation: crate::pane::Generation(7),
    };

    let runtime_event = mux
        .runtime_event_for_mux_event(&event)
        .expect("pane output maps to runtime event");

    assert_eq!(runtime_event.meta.pane_id, "pane1");
    assert_eq!(runtime_event.meta.generation, 7);
    assert!(matches!(
        runtime_event.kind,
        brehon_types::RuntimeEventKind::PaneOutput(ref output) if output.bytes == b"hello"
    ));
}

#[tokio::test]
async fn test_publish_runtime_pane_spawned_uses_current_generation() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());
    mux.get_mut("pane1")
        .expect("pane exists")
        .register_gateway_session_spawn("session-1".to_string());

    mux.publish_runtime_pane_spawned("pane1");

    let runtime_event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.next_event())
        .await
        .expect("runtime event published")
        .expect("runtime stream")
        .expect("runtime event");

    assert_eq!(runtime_event.meta.pane_id, "pane1");
    assert_eq!(runtime_event.meta.generation, 1);
    assert!(matches!(
        runtime_event.kind,
        brehon_types::RuntimeEventKind::PaneSpawned(ref spawned)
            if spawned.kind == RuntimePaneKind::Director
                && spawned.title.as_deref() == Some("pane1")
    ));
}

#[tokio::test]
async fn test_activity_event_publishes_runtime_pane_state_changed() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "pane1".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::Operation,
                ingested_at: std::time::Instant::now(),
                tool_id: None,
                tool_name: None,
                status: Some("started".to_string()),
                message: Some("operation started".to_string()),
                output_chunks: None,
                duration: None,
            },
            generation: Generation::default(),
        })
        .expect("queue activity event");

    let (_bytes, _events) = mux.poll_batch();

    let mut saw_busy_state = false;
    for _ in 0..6 {
        let Ok(Ok(Some(runtime_event))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.next_event()).await
        else {
            continue;
        };

        if matches!(
            runtime_event.kind,
            brehon_types::RuntimeEventKind::PaneStateChanged(ref changed)
                if runtime_event.meta.pane_id == "pane1"
                    && changed.current == RuntimePaneState::Busy
        ) {
            saw_busy_state = true;
            break;
        }
    }

    assert!(
        saw_busy_state,
        "activity should publish a Busy state change"
    );
}

#[tokio::test]
async fn test_sweeping_stale_activity_lock_publishes_ready_state() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    let mut pane = Pane::director("pane1", 24, 80).unwrap();
    pane.register_gateway_session_spawn("session-1".to_string());
    let generation = pane.current_generation();
    mux.add_pane(pane);

    mux.event_tx
        .try_send(MuxEvent::ActivityEvent {
            pane_id: "pane1".to_string(),
            entry: ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: std::time::Instant::now(),
                tool_id: Some("tool-1".to_string()),
                tool_name: Some("bash".to_string()),
                status: Some("started".to_string()),
                message: None,
                output_chunks: None,
                duration: None,
            },
            generation,
        })
        .expect("queue activity event");
    let (_bytes, _events) = mux.poll_batch();

    assert!(mux.get("pane1").expect("pane exists").is_tool_executing());
    assert!(matches!(
        mux.get("pane1").expect("pane exists").pane_state(),
        Some(PaneState::Busy { .. })
    ));

    std::thread::sleep(std::time::Duration::from_millis(5));
    let cleared = mux.sweep_stale_activity_locks(std::time::Duration::from_millis(1));

    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].0, "pane1");
    assert_eq!(cleared[0].1, vec!["tool-1".to_string()]);
    assert!(!cleared[0].2);
    assert!(!cleared[0].3);
    assert!(!mux.get("pane1").expect("pane exists").is_tool_executing());
    assert!(matches!(
        mux.get("pane1").expect("pane exists").pane_state(),
        Some(PaneState::Ready { .. })
    ));

    let mut saw_ready_state = false;
    for _ in 0..8 {
        let Ok(Ok(Some(runtime_event))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.next_event()).await
        else {
            continue;
        };

        if matches!(
            runtime_event.kind,
            brehon_types::RuntimeEventKind::PaneStateChanged(ref changed)
                if runtime_event.meta.pane_id == "pane1"
                    && changed.current == RuntimePaneState::Ready
                    && changed.reason.as_deref() == Some("stale activity cleared")
        ) {
            saw_ready_state = true;
            break;
        }
    }

    assert!(
        saw_ready_state,
        "sweeping stale activity should publish daemon-visible Ready state"
    );
}

#[tokio::test]
async fn test_mux_runtime_command_closes_existing_pane() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let result = mux.execute_runtime_command(
        &tokio::runtime::Handle::current(),
        runtime_command(
            "cmd-close",
            Some("pane1"),
            Some(0),
            RuntimeCommandKind::ClosePane {
                reason: "test close".to_string(),
            },
        ),
    );

    assert_eq!(result.status, RuntimeCommandStatus::Applied);
    assert!(mux.get("pane1").is_none());
}

#[tokio::test]
async fn test_mux_runtime_command_rejects_stale_generation() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let result = mux.execute_runtime_command(
        &tokio::runtime::Handle::current(),
        runtime_command(
            "cmd-stale",
            Some("pane1"),
            Some(9),
            RuntimeCommandKind::ClosePane {
                reason: "stale close".to_string(),
            },
        ),
    );

    assert_eq!(result.status, RuntimeCommandStatus::Rejected);
    assert!(mux.get("pane1").is_some());
}

#[test]
fn test_mux_runtime_command_attempt_prompt_reports_deferred_when_queue_full() {
    let policy = Arc::new(brehon_policy::BasicPolicyGate::new(
        brehon_policy::RuntimePolicyConfig {
            max_queued_prompts_per_pane: 0,
            ..Default::default()
        },
    ));
    let mut mux = Mux::new(24, 80);
    mux.set_policy_gate(policy);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    let result = mux.execute_runtime_command(
        rt.handle(),
        runtime_command(
            "cmd-prompt-attempt",
            Some("pane1"),
            Some(0),
            RuntimeCommandKind::SendPrompt {
                prompt_id: "prompt-1".to_string(),
                text: "hello".to_string(),
                from: Some("supervisor".to_string()),
                delivery: PromptDeliveryMode::Attempt,
            },
        ),
    );

    assert_eq!(result.status, RuntimeCommandStatus::Deferred);
    assert!(result.message.unwrap_or_default().contains("deferred"));
}

#[test]
fn test_mux_runtime_command_attempt_prompt_bypasses_pending_teams_nudge_cooldown() {
    let mut mux = Mux::new(24, 80);
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");
    let teams = TeamsManager::new_for_test("session", home.clone());
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
    )
    .expect("create reviewer pane");
    mux.add_pane(pane);
    {
        let pane = mux.get_mut("claude-reviewer").expect("pane exists");
        pane.set_pending_inbox_nudge(true);
        pane.set_inbox_nudge_not_before(Some(
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        ));
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let result = mux.execute_runtime_command(
        rt.handle(),
        runtime_command(
            "cmd-prompt-attempt",
            Some("claude-reviewer"),
            Some(0),
            RuntimeCommandKind::SendPrompt {
                prompt_id: "prompt-1".to_string(),
                text: "review approved".to_string(),
                from: Some("review-coordinator".to_string()),
                delivery: PromptDeliveryMode::Attempt,
            },
        ),
    );

    assert_eq!(result.status, RuntimeCommandStatus::Applied);
    assert_eq!(mux.pending_delayed_prompt_count(), 0);
    let inbox_path = TeamsPaths::for_session_with_home("session", home.clone())
        .inbox_for("claude-reviewer")
        .expect("inbox path");
    let payload = std::fs::read_to_string(&inbox_path).expect("read reviewer inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    assert!(
        messages
            .as_array()
            .expect("inbox entries array")
            .iter()
            .any(|message| message["text"] == "review approved"),
        "daemon runtime command delivery must write the durable prompt to Teams inbox"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn test_mux_runtime_command_rejects_unsupported_worker_spawn() {
    let mut mux = Mux::new(24, 80);

    let result = mux.execute_runtime_command(
        &tokio::runtime::Handle::current(),
        runtime_command(
            "cmd-spawn-worker",
            None,
            None,
            RuntimeCommandKind::SpawnPane {
                kind: RuntimePaneKind::Worker,
                pane_id: Some("worker-1".to_string()),
                title: None,
                cwd: None,
                command: Vec::new(),
                env: std::collections::BTreeMap::new(),
                rows: None,
                cols: None,
            },
        ),
    );

    assert_eq!(result.status, RuntimeCommandStatus::Rejected);
    assert!(mux.get("worker-1").is_none());
}

#[tokio::test]
async fn test_mux_runtime_command_port_round_trips_through_receiver() {
    let (port, mut receiver) = MuxRuntimeCommandPort::channel_default();
    let command = runtime_command(
        "cmd-port-close",
        Some("pane1"),
        Some(0),
        RuntimeCommandKind::ClosePane {
            reason: "port close".to_string(),
        },
    );

    let execute = tokio::spawn(async move { port.execute(command).await.expect("execute") });
    let request = receiver.recv().await.expect("command request");
    assert_eq!(request.command().command_id, "cmd-port-close");

    request.complete(brehon_types::RuntimeCommandResult {
        command_id: "cmd-port-close".to_string(),
        status: RuntimeCommandStatus::Applied,
        message: Some("closed".to_string()),
    });

    let result = execute.await.expect("execute task");
    assert_eq!(result.status, RuntimeCommandStatus::Applied);
}

#[tokio::test]
async fn test_daemon_routes_allowed_command_to_mux_receiver() {
    let (port, mut receiver) = MuxRuntimeCommandPort::channel_default();
    let command_port: Arc<dyn RuntimeCommandPort> = Arc::new(port);
    let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
        policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
        command_port: Some(command_port),
        ..Default::default()
    });
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let command = runtime_command(
        "cmd-daemon-close",
        Some("pane1"),
        Some(0),
        RuntimeCommandKind::ClosePane {
            reason: "daemon close".to_string(),
        },
    );
    let route = tokio::spawn(async move {
        daemon
            .route_command(command, RuntimePolicyContext::default())
            .await
            .expect("route command")
    });

    let request = receiver.recv().await.expect("command request");
    let result = mux.execute_runtime_command(
        &tokio::runtime::Handle::current(),
        request.command().clone(),
    );
    request.complete(result);

    let routed = route.await.expect("route task");
    assert_eq!(routed.status, RuntimeCommandStatus::Applied);
    assert!(mux.get("pane1").is_none());
}

#[tokio::test]
async fn test_daemon_approval_resolution_routes_pending_command_to_mux_receiver() {
    let (port, mut receiver) = MuxRuntimeCommandPort::channel_default();
    let command_port: Arc<dyn RuntimeCommandPort> = Arc::new(port);
    let daemon = brehon_daemon::RuntimeDaemon::new(brehon_daemon::RuntimeDaemonConfig {
        policy_gate: Some(Arc::new(brehon_policy::BasicPolicyGate::default())),
        command_port: Some(command_port),
        ..Default::default()
    });
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let result = daemon
        .route_command(
            runtime_command(
                "cmd-approval-close",
                Some("pane1"),
                Some(0),
                RuntimeCommandKind::ClosePane {
                    reason: "approved close".to_string(),
                },
            ),
            RuntimePolicyContext {
                approval_required: true,
                ..RuntimePolicyContext::default()
            },
        )
        .await
        .expect("route command");
    assert_eq!(result.status, RuntimeCommandStatus::Deferred);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    let approval_id = daemon.approval_registry_snapshot().await.approvals[0]
        .approval_id
        .clone();
    let resolve = tokio::spawn(async move {
        daemon
            .route_command(
                runtime_command(
                    "cmd-resolve-approval",
                    None,
                    None,
                    RuntimeCommandKind::ResolveApproval {
                        approval_id,
                        approved: true,
                    },
                ),
                RuntimePolicyContext::default(),
            )
            .await
            .expect("resolve approval")
    });

    let request = receiver.recv().await.expect("approved command request");
    assert_eq!(request.command().command_id, "cmd-approval-close");
    let result = mux.execute_runtime_command(
        &tokio::runtime::Handle::current(),
        request.command().clone(),
    );
    request.complete(result);

    let resolved = resolve.await.expect("resolve task");
    assert_eq!(resolved.status, RuntimeCommandStatus::Applied);
    assert!(mux.get("pane1").is_none());
}

#[tokio::test]
async fn test_poll_batch_publishes_runtime_events_when_sink_is_installed() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    mux.event_tx
        .try_send(MuxEvent::PaneOutput {
            pane_id: "pane1".to_string(),
            data: b"runtime hello\n".to_vec(),
            generation: crate::pane::Generation::default(),
        })
        .expect("queue pane output");

    let (_bytes, _events) = mux.poll_batch();

    let mut saw_output = false;
    for _ in 0..3 {
        let runtime_event =
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.next_event())
                .await
                .expect("runtime event published")
                .expect("runtime stream")
                .expect("runtime event");

        if matches!(
            runtime_event.kind,
            brehon_types::RuntimeEventKind::PaneOutput(ref output)
                if runtime_event.meta.pane_id == "pane1" && output.bytes == b"runtime hello\n"
        ) {
            saw_output = true;
            break;
        }
    }

    assert!(saw_output);
}

#[tokio::test]
async fn test_policy_gate_defers_prompt_delivery_when_queue_is_full() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let policy = std::sync::Arc::new(brehon_policy::BasicPolicyGate::new(
        brehon_policy::RuntimePolicyConfig {
            max_queued_prompts_per_pane: 0,
            ..Default::default()
        },
    ));
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    mux.set_policy_gate(policy);
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());

    let attempt = mux
        .attempt_prompt_delivery("pane1", "hello", None)
        .await
        .expect("attempt prompt delivery");

    assert!(matches!(
        attempt,
        PromptDeliveryAttempt::Queued { ahead_of: 1, .. }
    ));

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.next_event())
        .await
        .expect("policy event published")
        .expect("runtime stream")
        .expect("runtime event");

    assert!(matches!(
        event.kind,
        brehon_types::RuntimeEventKind::PolicyDecision(ref decision)
            if matches!(
                decision.decision,
                brehon_types::RuntimePolicyDecision::Defer { .. }
            )
    ));
}

#[tokio::test]
async fn test_policy_gate_denies_terminal_input_to_dead_pane() {
    let bus = std::sync::Arc::new(brehon_runtime::RuntimeEventBus::new(8));
    let mut rx = bus.subscribe();
    let mut mux = Mux::new(24, 80);
    mux.set_runtime_event_sink(bus);
    mux.set_policy_gate(std::sync::Arc::new(
        brehon_policy::BasicPolicyGate::default(),
    ));
    mux.add_pane(Pane::director("pane1", 24, 80).unwrap());
    mux.quarantine("pane1", DeathReason::Quarantined("manual".to_string()));

    let err = mux
        .send_input_to("pane1", b"x")
        .await
        .expect_err("dead pane input should be denied");

    assert!(err.to_string().contains("Policy denied terminal input"));

    let mut saw_deny = false;
    for _ in 0..4 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.next_event())
            .await
            .expect("runtime event published")
            .expect("runtime stream")
            .expect("runtime event");
        if matches!(
            event.kind,
            brehon_types::RuntimeEventKind::PolicyDecision(ref decision)
                if matches!(
                    decision.decision,
                    brehon_types::RuntimePolicyDecision::Deny { .. }
                )
        ) {
            saw_deny = true;
            break;
        }
    }

    assert!(saw_deny);
}

#[test]
fn test_format_acp_session_event_hides_low_signal_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1"),
        message: "Codex thread status: active".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_codex_idle_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1-idle"),
        message: "Codex thread status: idle".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_session_idle_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-1b"),
        message: "session idle".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_successful_response_lifecycle() {
    let started = brehon_acp::updates::SessionEvent::OperationStarted {
        session_id: brehon_types::SessionId::new("s-response-start"),
        operation: "turn".to_string(),
    };
    let completed = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-response-complete"),
        operation: "turn".to_string(),
        success: true,
    };
    let explicit_response = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-response-explicit"),
        operation: "response".to_string(),
        success: true,
    };

    assert!(format_acp_session_event(&started).is_none());
    assert!(format_acp_session_event(&completed).is_none());
    assert!(format_acp_session_event(&explicit_response).is_none());
}

#[test]
fn test_format_acp_session_event_hides_opencode_lifecycle_noise() {
    let turn_started = brehon_acp::updates::SessionEvent::OperationStarted {
        session_id: brehon_types::SessionId::new("s-opencode-turn-start"),
        operation: "opencode turn".to_string(),
    };
    let turn_completed = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-opencode-turn-complete"),
        operation: "opencode turn".to_string(),
        success: true,
    };
    let step_started = brehon_acp::updates::SessionEvent::OperationStarted {
        session_id: brehon_types::SessionId::new("s-opencode-step-start"),
        operation: "step".to_string(),
    };
    let step_completed = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-opencode-step-complete"),
        operation: "step".to_string(),
        success: true,
    };

    assert!(format_acp_session_event(&turn_started).is_none());
    assert!(format_acp_session_event(&turn_completed).is_none());
    assert!(format_acp_session_event(&step_started).is_none());
    assert!(format_acp_session_event(&step_completed).is_none());
}

#[test]
fn test_format_acp_session_event_keeps_failed_lifecycle_visible() {
    let event = brehon_acp::updates::SessionEvent::OperationCompleted {
        session_id: brehon_types::SessionId::new("s-opencode-turn-failed"),
        operation: "opencode turn".to_string(),
        success: false,
    };

    let rendered = String::from_utf8(format_acp_session_event(&event).expect("formatted event"))
        .expect("utf8 output");

    assert!(rendered.contains("response failed"));
}

#[test]
fn test_format_acp_session_event_surfaces_codex_error_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-codex-error"),
        message:
            "Codex error: The prompt is too long: 203272, model maximum context length: 202752"
                .to_string(),
        percent: None,
    };

    let rendered = format_acp_session_event(&event).expect("rendered codex error");
    let rendered = String::from_utf8_lossy(&rendered);
    assert!(rendered.contains("error: The prompt is too long"));
}

#[test]
fn test_format_acp_session_event_hides_low_signal_tool_success() {
    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool"),
        tool_id: "tool-1".to_string(),
        tool_name: "submit_review".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_brehon_bootstrap_tool_lines() {
    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-started"),
        tool_id: "tool-2".to_string(),
        tool_name: "brehon_agent".to_string(),
        details: None,
    };
    let completed = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool-completed"),
        tool_id: "tool-3".to_string(),
        tool_name: "brehon_task".to_string(),
        status: "completed".to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&started).is_none());
    assert!(format_acp_session_event(&completed).is_none());
}

#[test]
fn test_format_acp_session_event_hides_json_bootstrap_tool_lines() {
    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-json-started"),
        tool_id: "tool-4".to_string(),
        tool_name: r#"{"action":"session_start","name":"reviewer-1"}"#.to_string(),
        details: None,
    };
    let status_query = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-json-status"),
        tool_id: "tool-5".to_string(),
        tool_name: r#"{"status":"InReview"}"#.to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&started).is_none());
    assert!(format_acp_session_event(&status_query).is_none());
}

#[test]
fn test_format_acp_session_event_hides_kimi_prefixed_brehon_tool_lines() {
    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-kimi-started"),
        tool_id: "tool-6".to_string(),
        tool_name: r#"task: {"action":"complete","id":"T-123"}"#.to_string(),
        details: None,
    };
    let agent = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-kimi-agent"),
        tool_id: "tool-7".to_string(),
        tool_name: r#"agent: {"action":"message","target":"claude-supervisor"}"#.to_string(),
        details: None,
    };

    assert!(format_acp_session_event(&started).is_none());
    assert!(format_acp_session_event(&agent).is_none());
}

#[test]
fn test_format_acp_session_event_keeps_low_signal_tool_failures_visible() {
    let event = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool-failed"),
        tool_id: "tool-6".to_string(),
        tool_name: "brehon_task".to_string(),
        status: "failed".to_string(),
        details: None,
    };

    let rendered = String::from_utf8(format_acp_session_event(&event).expect("formatted event"))
        .expect("utf8 output");

    assert!(rendered.contains("tool: brehon_task failed"));
}

#[test]
fn test_normalize_gateway_tool_event_reuses_cached_name_for_kimi_completion() {
    let mut active = std::collections::HashMap::new();
    let started = brehon_acp::updates::SessionEvent::ToolCallStarted {
        session_id: brehon_types::SessionId::new("s-tool-kimi-started"),
        tool_id: "tool-8".to_string(),
        tool_name: r#"task: {"action":"complete","id":"T-123"}"#.to_string(),
        details: None,
    };
    let (started, duplicate) = crate::mux::normalize_gateway_tool_event(started, &mut active);
    assert!(!duplicate);
    match started {
        brehon_acp::updates::SessionEvent::ToolCallStarted { tool_name, .. } => {
            assert_eq!(tool_name, "brehon_task");
        }
        other => panic!("expected started event, got {other:?}"),
    }

    let completed = brehon_acp::updates::SessionEvent::ToolCallCompleted {
        session_id: brehon_types::SessionId::new("s-tool-kimi-completed"),
        tool_id: "tool-8".to_string(),
        tool_name: "tool".to_string(),
        status: "failed".to_string(),
        details: None,
    };
    let (completed, duplicate) = crate::mux::normalize_gateway_tool_event(completed, &mut active);
    assert!(!duplicate);
    match completed {
        brehon_acp::updates::SessionEvent::ToolCallCompleted { tool_name, .. } => {
            assert_eq!(tool_name, "brehon_task");
        }
        other => panic!("expected completed event, got {other:?}"),
    }
}

#[test]
fn test_format_acp_session_event_prettifies_mcp_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2"),
        message: "MCP server brehon: failed (connection refused)".to_string(),
        percent: None,
    };

    let rendered = String::from_utf8(format_acp_session_event(&event).expect("formatted event"))
        .expect("utf8 output");

    assert!(rendered.contains("mcp: brehon: failed (connection refused)"));
    assert!(rendered.contains("brehon: failed (connection refused)"));
    assert!(!rendered.contains("[mcp]"));
    assert!(!rendered.contains("[gateway]"));
}

#[test]
fn test_format_acp_session_event_hides_successful_mcp_startup_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2b"),
        message: "MCP server brehon: ready".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_mcp_starting_progress() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2b-starting"),
        message: "MCP server brehon: starting".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_low_value_codex_mcp_bootstrap_calls() {
    let started = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2d"),
        message: "Codex MCP tool brehon/agent started".to_string(),
        percent: None,
    };
    let session_start = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2e"),
        message: "Codex MCP tool brehon: session_start".to_string(),
        percent: None,
    };
    let whoami = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2f"),
        message: "Codex MCP tool brehon: whoami".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&started).is_none());
    assert!(format_acp_session_event(&session_start).is_none());
    assert!(format_acp_session_event(&whoami).is_none());
}

#[test]
fn test_format_acp_session_event_hides_low_value_codex_approval_banners() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2g"),
        message: "Approved Codex MCP tool call on brehon: session_start".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_hides_unknown_gateway_update_banners() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-2c"),
        message: "ACP update: plan".to_string(),
        percent: None,
    };

    assert!(format_acp_session_event(&event).is_none());
}

#[test]
fn test_format_acp_session_event_preserves_streamed_output_chunks() {
    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-3"),
        text: "Starting work on `--dry-run-json`.".to_string(),
    };

    let rendered = String::from_utf8(format_acp_session_event(&event).expect("formatted event"))
        .expect("utf8 output");

    assert_eq!(rendered, "Starting work on `--dry-run-json`.");
}

#[test]
fn test_format_acp_session_event_normalizes_embedded_newlines() {
    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-4"),
        text: "line one\nline two\n".to_string(),
    };

    let rendered = String::from_utf8(format_acp_session_event(&event).expect("formatted event"))
        .expect("utf8 output");

    assert_eq!(rendered, "line one\r\nline two\r\n");
}

#[test]
fn test_hidden_boundary_prefixes_followup_output_without_newline() {
    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-4b"),
        text: "follow up".to_string(),
    };
    let data = format_acp_session_event(&event).expect("formatted event");

    assert!(Mux::should_prefix_output_after_hidden_boundary(
        &event, &data, false, false,
    ));
}

#[test]
fn test_hidden_boundary_does_not_prefix_output_after_newline() {
    let event = brehon_acp::updates::SessionEvent::Output {
        session_id: brehon_types::SessionId::new("s-4c"),
        text: "follow up".to_string(),
    };
    let data = format_acp_session_event(&event).expect("formatted event");

    assert!(!Mux::should_prefix_output_after_hidden_boundary(
        &event, &data, false, true,
    ));
}

#[test]
fn test_hidden_boundary_does_not_prefix_non_output_events() {
    let event = brehon_acp::updates::SessionEvent::Progress {
        session_id: brehon_types::SessionId::new("s-4d"),
        message: "Codex thread status: active".to_string(),
        percent: None,
    };

    assert!(!Mux::should_prefix_output_after_hidden_boundary(
        &event, b"", false, false,
    ));
}
