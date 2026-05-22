use crate::mux::*;
use crate::pane::panesmith_shim::FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID;
use crate::pty::{Pty, PtyConfig};
use crate::teams::{TeamsManager, TeamsPaths};
use crate::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    Pane, PaneBackend, PaneKind, PaneState, SupervisorCli,
};
use brehon_types::{
    PaneExitedEvent, PaneOutputEvent, RuntimeEvent, RuntimeEventKind, RuntimeEventMeta,
    RuntimePaneKind, RuntimeSource,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn test_mux_remains_send_for_cross_thread_hosts() {
    fn assert_send<T: Send>() {}

    assert_send::<Mux>();
}

#[test]
fn test_pane_backend_ownership_status_api() {
    let mut mux = Mux::new(24, 80);
    mux.add_pane(Pane::director("native", 24, 80).expect("director pane"));
    assert_eq!(
        mux.pane_backend_ownership("native"),
        Some(PaneBackendOwnership::None)
    );

    let config = PtyConfig {
        command: "sh".to_string(),
        args: vec!["-c".to_string(), "cat".to_string()],
        cwd: Some(std::env::temp_dir()),
        env: vec![],
        rows: 24,
        cols: 80,
    };
    let pty = Pty::spawn("ghostty-pane", config).expect("spawn test pty");
    let pane = Pane::with_pty_cli(
        "ghostty-pane",
        PaneKind::Worker,
        pty,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("ghostty pane");
    mux.add_pane(pane);
    assert_eq!(
        mux.pane_backend_ownership("ghostty-pane"),
        Some(PaneBackendOwnership::GhosttyVt)
    );

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());

    let project_root = super::fresh_temp_dir("brehon-mux-backend-ownership");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mux = Mux::factory(config).expect("create mux");

    assert_eq!(
        mux.pane_backend_ownership("codex-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    assert_eq!(
        mux.pane_backend_ownership("reviewer-1"),
        Some(PaneBackendOwnership::Gateway)
    );
    assert_eq!(mux.pane_backend_ownership("missing"), None);
}

#[test]
fn test_factory_uses_isolated_supervisor_and_reviewer_cwds() {
    let project_root = super::fresh_temp_dir("brehon-mux-isolated-cwds");
    let supervisor_cwd = super::setup_fake_linked_worktree(
        &project_root,
        ".brehon/worktrees/supervisor/claude-code",
    );
    let reviewer_cwd =
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/reviewer/reviewer-1");
    let worker_cwd = super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/worker-1");

    let mut worker_cwds = HashMap::new();
    worker_cwds.insert("worker-1".to_string(), worker_cwd.clone());
    let mut reviewer_cwds = HashMap::new();
    reviewer_cwds.insert("reviewer-1".to_string(), reviewer_cwd.clone());

    let config = MuxConfig {
        cwd: project_root.clone(),
        worktree_isolation: true,
        worker_cwds,
        supervisor_cwd: Some(supervisor_cwd.clone()),
        reviewer_cwds,
        workers: 1,
        worker_names: vec!["worker-1".to_string()],
        supervisor_name: "claude-code".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    };

    let mux = Mux::factory(config).expect("create mux");

    let supervisor = mux.get("claude-code").expect("supervisor pane exists");
    assert!(!supervisor.is_gateway_backed());
    assert!(supervisor.accepts_manual_input());
    assert_eq!(
        supervisor
            .pty_spawn_config
            .as_ref()
            .and_then(|config| config.cwd.as_deref())
            .map(std::path::Path::to_string_lossy)
            .map(|path| path.into_owned()),
        Some(supervisor_cwd.to_string_lossy().into_owned())
    );
    let reviewer = mux.get("reviewer-1").expect("reviewer pane exists");
    assert_eq!(
        reviewer
            .gateway_spawn_config()
            .map(|config| config.cwd.as_str()),
        Some(reviewer_cwd.to_string_lossy().as_ref())
    );
}

#[test]
fn test_factory_uses_panesmith_for_supervisor_pty_pane() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-supervisor");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };

    let mux = Mux::factory(config).expect("create mux");
    let supervisor = mux.get("codex-supervisor").expect("supervisor pane exists");

    assert!(supervisor.is_panesmith_managed());
    assert!(supervisor.accepts_manual_input());
    assert!(mux.is_panesmith_managed("codex-supervisor"));
    assert_eq!(
        mux.pane_backend_ownership("codex-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    assert!(mux.panesmith_snapshot("codex-supervisor").is_some());
    assert!(matches!(supervisor.backend, PaneBackend::None));
}

#[test]
fn test_panesmith_supervisor_resize_updates_snapshot() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-resize");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");

    assert!(
        mux.resize_panesmith_pane("codex-supervisor", 12, 44)
            .expect("resize panesmith pane")
    );
    let snapshot = mux
        .panesmith_snapshot("codex-supervisor")
        .expect("panesmith snapshot");
    assert_eq!(snapshot.size.rows, 12);
    assert_eq!(snapshot.size.cols, 44);
}

#[tokio::test]
async fn test_panesmith_supervisor_reset_restarts_panesmith() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-reset");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");
    assert_eq!(
        mux.pane_backend_ownership("codex-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    {
        let supervisor = mux.get_mut("codex-supervisor").expect("supervisor pane");
        supervisor.mark_exited(Some(0));
        assert!(supervisor.has_exited());
        assert!(matches!(
            supervisor.pane_state(),
            Some(PaneState::Dead { .. })
        ));
    }

    mux.reset_supervisor_session("codex-supervisor")
        .await
        .expect("reset supervisor");

    let supervisor = mux.get("codex-supervisor").expect("supervisor pane");
    assert!(supervisor.is_panesmith_managed());
    assert!(!supervisor.has_exited());
    assert_eq!(supervisor.exit_code(), None);
    assert!(matches!(
        supervisor.pane_state(),
        Some(PaneState::Ready { .. })
    ));
    assert!(matches!(supervisor.backend, PaneBackend::None));
    assert_eq!(
        mux.pane_backend_ownership("codex-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    assert!(mux.panesmith_snapshot("codex-supervisor").is_some());
}

#[test]
fn test_panesmith_supervisor_exit_mirrors_to_brehon_state() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-exit");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "shell-supervisor".to_string(),
        supervisor_cli: custom_interactive_agent("shell-supervisor", "sh", &["-c", "exit 7"]),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");
    assert_eq!(
        mux.pane_backend_ownership("shell-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );

    let mut saw_exit_event = false;
    for _ in 0..50 {
        let (_bytes, events) = mux.poll_batch();
        saw_exit_event |= events.iter().any(|event| {
            matches!(
                event,
                MuxEvent::PaneExited {
                    pane_id,
                    exit_code: Some(7)
                } if pane_id == "shell-supervisor"
            )
        });
        if saw_exit_event {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(saw_exit_event, "expected mirrored PaneExited event");
    let supervisor = mux.get("shell-supervisor").expect("supervisor pane");
    assert!(supervisor.has_exited());
    assert_eq!(supervisor.exit_code(), Some(7));
}

#[test]
fn test_factory_falls_back_to_ghostty_when_panesmith_spawn_fails() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-fallback");
    let supervisor_id = FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID;
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: supervisor_id.to_string(),
        supervisor_cli: custom_interactive_agent("shell-supervisor", "sh", &["-c", "cat"]),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux with ghostty fallback");

    let supervisor = mux.get(supervisor_id).expect("supervisor pane");
    assert!(!supervisor.is_panesmith_managed());
    assert!(!mux.is_panesmith_managed(supervisor_id));
    assert!(matches!(supervisor.backend, PaneBackend::Pty(_)));
    assert_eq!(
        mux.pane_backend_ownership(supervisor_id),
        Some(PaneBackendOwnership::GhosttyVt)
    );

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
}

#[test]
fn test_panesmith_supervisor_input_is_mirrored_to_mux_events() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-input");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");
    let rt = tokio::runtime::Runtime::new().expect("create runtime");

    rt.block_on(mux.send_input_to("codex-supervisor", b"hello from panesmith\r"))
        .expect("send input through mux");

    let mut saw_output_event = false;
    let mut saw_snapshot_text = false;
    for _ in 0..50 {
        let (_bytes, events) = mux.poll_batch();
        saw_output_event |= events.iter().any(|event| {
            matches!(event, MuxEvent::PaneOutput { pane_id, .. } if pane_id == "codex-supervisor")
        });
        saw_snapshot_text = mux
            .panesmith_snapshot("codex-supervisor")
            .map(panesmith_snapshot_text)
            .is_some_and(|text| text.contains("hello from panesmith"));
        if saw_output_event && saw_snapshot_text {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(saw_output_event, "expected mirrored PaneOutput event");
    assert!(
        saw_snapshot_text,
        "expected echoed input in Panesmith snapshot"
    );
}

#[test]
fn test_panesmith_supervisor_prompt_delivery_uses_transaction() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-prompt");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");
    let rt = tokio::runtime::Runtime::new().expect("create runtime");

    let attempt = rt
        .block_on(mux.attempt_prompt_delivery(
            "codex-supervisor",
            "hello from panesmith transaction",
            None,
        ))
        .expect("deliver prompt through Panesmith transaction");

    assert!(
        matches!(attempt, PromptDeliveryAttempt::Delivered { .. }),
        "expected delivered prompt, got {attempt:?}"
    );

    let mut saw_snapshot_text = false;
    for _ in 0..50 {
        let (_bytes, _events) = mux.poll_batch();
        saw_snapshot_text = mux
            .panesmith_snapshot("codex-supervisor")
            .map(panesmith_snapshot_text)
            .is_some_and(|text| text.contains("hello from panesmith transaction"));
        if saw_snapshot_text {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        saw_snapshot_text,
        "expected transaction-delivered prompt in Panesmith snapshot"
    );
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

fn custom_interactive_agent(name: &str, command: &str, args: &[&str]) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some(command.to_string()),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::InteractivePty,
            preferred_control_plane: HarnessControlPlane::PtyInjection,
        },
    })
}

#[test]
fn test_factory_spawns_research_pane_with_pool_agent_type() {
    let project_root = super::fresh_temp_dir("brehon-mux-research-pane");
    let mut research_agent_type_map = HashMap::new();
    research_agent_type_map.insert("research-specs".to_string(), "specs".to_string());

    let mux = Mux::factory(MuxConfig {
        cwd: project_root.clone(),
        session_name: Some("research-session".to_string()),
        pane_materialization: AgentPaneMaterialization::PlanOnly,
        workers: 0,
        worker_names: Vec::new(),
        supervisor_name: "supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        research_names: vec!["research-specs".to_string()],
        research_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        research_agent_type_map,
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    })
    .expect("create mux");

    let researcher = mux.get("research-specs").expect("research pane exists");
    assert_eq!(researcher.kind(), &PaneKind::Research);
    assert_eq!(researcher.configured_agent_type(), Some("specs"));
    let env = researcher
        .pty_spawn_config
        .as_ref()
        .map(|config| config.env.clone())
        .unwrap_or_default();
    assert!(
        env.iter()
            .any(|(key, value)| key == "BREHON_AGENT_ROLE" && value == "research")
    );
    assert!(
        env.iter()
            .any(|(key, value)| key == "BREHON_AGENT_TYPE" && value == "specs")
    );
}

#[test]
fn terminal_host_agent_factory_plan_aggregates_pane_launch_contracts() {
    let mut mux = Mux::new(24, 80);
    let mut pty_pane = Pane::new_with_backend_cli(
        "local-pty",
        "local-pty",
        PaneKind::Worker,
        PaneBackend::None,
        24,
        80,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create pty-backed worker shell");
    pty_pane.set_pty_spawn_config(PtyConfig {
        command: "bash".to_string(),
        args: vec!["-lc".to_string(), "echo ready".to_string()],
        cwd: Some(PathBuf::from("/tmp")),
        env: vec![("READY".to_string(), "1".to_string())],
        rows: 24,
        cols: 80,
    });
    mux.add_pane(pty_pane);
    mux.add_pane(Pane::director("director", 24, 80).expect("create director pane"));
    mux.add_pane(
        Pane::worker(
            "worker-1",
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
        )
        .expect("create gateway worker"),
    );

    let plan = mux.terminal_host_agent_factory_plan("session-1");

    assert!(!plan.ready());
    assert_eq!(plan.total_panes, 2);
    assert_eq!(plan.launch_specs.len(), 1);
    assert_eq!(plan.launch_specs[0].spec.pane_id, "local-pty");
    assert_eq!(plan.blocked_panes.len(), 1);
    assert_eq!(plan.blocked_panes[0].pane_id, "worker-1");
    assert_eq!(plan.blocked_panes[0].kind, "worker");
    assert_eq!(
        plan.blocked_panes[0].reason,
        "gateway-backed codex_app_server_ws agent sessions are not terminal-host PTY panes"
    );
}

#[test]
fn terminal_host_runtime_events_mirror_output_and_fence_generation() {
    let mut mux = Mux::new(5, 40);
    let pane = Pane::new_with_backend_cli(
        "worker-1",
        "worker-1",
        PaneKind::Worker,
        PaneBackend::None,
        5,
        40,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create host-owned placeholder pane");
    mux.add_pane(pane);
    mux.sync_terminal_host_pane_generation("worker-1", 2)
        .expect("sync host generation");

    let stale = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 1, RuntimeSource::Headless, 100),
        RuntimeEventKind::PaneOutput(PaneOutputEvent {
            bytes: b"stale output\r\n".to_vec(),
            text: None,
        }),
    );
    assert!(
        !mux.apply_terminal_host_runtime_event(&stale)
            .expect("apply stale event"),
        "stale host output should be ignored"
    );

    let live = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 2, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneOutput(PaneOutputEvent {
            bytes: b"live output\r\n".to_vec(),
            text: None,
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&live)
            .expect("apply live event"),
        "current-generation host output should update the pane"
    );
    let viewport = mux
        .get("worker-1")
        .expect("worker pane")
        .dump_viewport()
        .expect("dump viewport");
    assert!(!viewport.contains("stale output"));
    assert!(viewport.contains("live output"));

    for (timestamp_ms, snapshot) in [
        (102, "\x1b[0m\x1b[H\x1b[2J\x1b[3Jfirst snapshot\x1b[0m"),
        (103, "\x1b[0m\x1b[H\x1b[2J\x1b[3Jsecond snapshot\x1b[0m"),
    ] {
        let redraw = RuntimeEvent::new(
            RuntimeEventMeta::new(
                "session-1",
                "worker-1",
                2,
                RuntimeSource::Headless,
                timestamp_ms,
            ),
            RuntimeEventKind::PaneOutput(PaneOutputEvent {
                bytes: snapshot.as_bytes().to_vec(),
                text: None,
            }),
        );
        assert!(
            mux.apply_terminal_host_runtime_event(&redraw)
                .expect("apply redraw event"),
            "current-generation host redraw should update the pane"
        );
    }
    let viewport = mux
        .get("worker-1")
        .expect("worker pane")
        .dump_viewport()
        .expect("dump viewport");
    assert!(!viewport.contains("first snapshot"));
    assert!(!viewport.contains("live output"));
    assert!(viewport.contains("second snapshot"));

    let exited = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 2, RuntimeSource::Headless, 104),
        RuntimeEventKind::PaneExited(PaneExitedEvent {
            exit_code: Some(0),
            reason: Some("completed".to_string()),
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&exited)
            .expect("apply exit event"),
        "current-generation host exit should update the pane"
    );
    let pane = mux.get("worker-1").expect("worker pane");
    assert!(pane.has_exited());
    assert_eq!(pane.exit_code(), Some(0));

    let respawned = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 3, RuntimeSource::Headless, 105),
        RuntimeEventKind::PaneSpawned(brehon_types::PaneSpawnedEvent {
            kind: RuntimePaneKind::Worker,
            title: Some("worker-1".to_string()),
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&respawned)
            .expect("apply respawn event"),
        "new-generation host spawn should revive a dead placeholder pane"
    );
    let pane = mux.get("worker-1").expect("worker pane");
    assert!(!pane.has_exited());
    assert!(matches!(pane.pane_state(), Some(PaneState::Ready { .. })));
}

#[test]
fn terminal_host_agent_factory_plan_from_config_is_plan_only() {
    let project_root = super::fresh_temp_dir("brehon-mux-terminal-host-plan-only");
    let supervisor_adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "plan-only-supervisor".to_string(),
        command: Some("brehon-test-command-that-must-not-exist".to_string()),
        args: vec!["--sentinel".to_string()],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::InteractivePty,
            preferred_control_plane: HarnessControlPlane::PtyInjection,
        },
    });

    let plan = Mux::terminal_host_agent_factory_plan_from_config(
        MuxConfig {
            cwd: project_root,
            session_name: Some("session-plan".to_string()),
            workers: 1,
            worker_names: vec!["worker-1".to_string()],
            supervisor_name: "supervisor".to_string(),
            supervisor_cli: supervisor_adapter,
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            reviewer_names: vec!["reviewer-1".to_string()],
            reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            advisor_names: vec!["advisor-1".to_string()],
            advisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            include_director: false,
            rows: 30,
            cols: 120,
            ..Default::default()
        },
        "session-plan",
    )
    .expect("build terminal-host plan without spawning agent processes");

    assert!(plan.ready());
    assert_eq!(plan.total_panes, 4);
    assert_eq!(plan.blocked_panes, Vec::new());
    assert_eq!(plan.launch_specs.len(), 4);

    let pane_ids: Vec<_> = plan
        .launch_specs
        .iter()
        .map(|launch| launch.spec.pane_id.as_str())
        .collect();
    assert_eq!(
        pane_ids,
        ["worker-1", "supervisor", "reviewer-1", "advisor-1"]
    );
    assert!(
        plan.launch_specs
            .iter()
            .all(|launch| launch.spec.session_id == "session-plan")
    );
    assert!(
        plan.launch_specs
            .iter()
            .all(|launch| !launch.spec.command.is_empty())
    );

    let supervisor = plan
        .launch_specs
        .iter()
        .find(|launch| launch.spec.kind == RuntimePaneKind::Supervisor)
        .expect("supervisor launch spec exists");
    assert_eq!(
        supervisor.spec.command[0],
        "brehon-test-command-that-must-not-exist"
    );
}

#[test]
fn terminal_host_agent_factory_plan_from_config_uses_pty_for_builtin_gateway_roles() {
    let project_root = super::fresh_temp_dir("brehon-mux-terminal-host-builtin-pty");

    let plan = Mux::terminal_host_agent_factory_plan_from_config(
        MuxConfig {
            cwd: project_root,
            session_name: Some("session-builtins".to_string()),
            workers: 1,
            worker_names: vec!["codex-worker".to_string()],
            supervisor_name: "codex-supervisor".to_string(),
            supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
            reviewer_names: vec!["opencode-reviewer".to_string()],
            reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
            include_director: false,
            rows: 30,
            cols: 120,
            ..Default::default()
        },
        "session-builtins",
    )
    .expect("build terminal-host plan for builtin gateway-capable roles");

    assert!(plan.ready());
    assert_eq!(plan.total_panes, 3);
    assert_eq!(plan.blocked_panes, Vec::new());

    let command_for = |kind| {
        plan.launch_specs
            .iter()
            .find(|launch| launch.spec.kind == kind)
            .and_then(|launch| launch.spec.command.first())
            .map(String::as_str)
    };
    assert_eq!(command_for(RuntimePaneKind::Supervisor), Some("codex"));
    assert_eq!(command_for(RuntimePaneKind::Worker), Some("codex"));
    assert_eq!(command_for(RuntimePaneKind::Reviewer), Some("opencode"));
}

#[test]
fn terminal_host_agent_factory_plan_includes_reviewer_panel_metadata() {
    let project_root = super::fresh_temp_dir("brehon-mux-terminal-host-review-panels");
    let mut reviewer_panel_map = HashMap::new();
    reviewer_panel_map.insert("reviewer-a".to_string(), "primary".to_string());
    reviewer_panel_map.insert("reviewer-b".to_string(), "secondary".to_string());
    let mut reviewer_panel_tab_map = HashMap::new();
    reviewer_panel_tab_map.insert("reviewer-a".to_string(), "Reviewers".to_string());
    reviewer_panel_tab_map.insert("reviewer-b".to_string(), "Reviewers: secondary".to_string());

    let plan = Mux::terminal_host_agent_factory_plan_from_config(
        MuxConfig {
            cwd: project_root,
            session_name: Some("session-review-panels".to_string()),
            workers: 0,
            worker_names: Vec::new(),
            supervisor_name: "supervisor".to_string(),
            supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            reviewer_names: vec!["reviewer-a".to_string(), "reviewer-b".to_string()],
            reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Junie),
            reviewer_panel_map,
            reviewer_panel_tab_map,
            include_director: false,
            rows: 30,
            cols: 120,
            ..Default::default()
        },
        "session-review-panels",
    )
    .expect("build terminal-host plan with reviewer panel metadata");

    let reviewer_b = plan
        .launch_specs
        .iter()
        .find(|launch| launch.spec.pane_id == "reviewer-b")
        .expect("reviewer-b launch spec");
    assert_eq!(
        reviewer_b
            .spec
            .env
            .get("BREHON_REVIEW_PANEL")
            .map(String::as_str),
        Some("secondary")
    );
    assert_eq!(
        reviewer_b
            .spec
            .env
            .get("BREHON_REVIEW_PANEL_TAB")
            .map(String::as_str),
        Some("Reviewers: secondary")
    );
}

#[test]
fn terminal_host_agent_factory_plan_from_config_rejects_session_mismatch() {
    let err = Mux::terminal_host_agent_factory_plan_from_config(
        MuxConfig {
            session_name: Some("config-session".to_string()),
            ..Default::default()
        },
        "requested-session",
    )
    .expect_err("session mismatch should fail before planning");

    assert!(err.to_string().contains("session mismatch"));
}

#[test]
fn test_factory_rejects_missing_supervisor_cwd_when_isolation_enabled() {
    let project_root = super::fresh_temp_dir("brehon-mux-missing-supervisor");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert(
        "worker-1".to_string(),
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/worker-1"),
    );

    let err = match Mux::factory(MuxConfig {
        cwd: project_root,
        worktree_isolation: true,
        worker_cwds,
        workers: 1,
        worker_names: vec!["worker-1".to_string()],
        supervisor_name: "claude-code".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    }) {
        Ok(_) => panic!("missing supervisor cwd should fail"),
        Err(err) => err,
    };

    assert!(err.to_string().contains("supervisor has no isolated cwd"));
}

#[test]
fn test_factory_rejects_missing_reviewer_cwd_when_isolation_enabled() {
    let project_root = super::fresh_temp_dir("brehon-mux-missing-reviewer");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert(
        "worker-1".to_string(),
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/worker-1"),
    );

    let err = match Mux::factory(MuxConfig {
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
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    }) {
        Ok(_) => panic!("missing reviewer cwd should fail"),
        Err(err) => err,
    };

    assert!(
        err.to_string()
            .contains("reviewer 'reviewer-1' has no isolated cwd")
    );
}

#[test]
fn test_factory_rejects_worker_cwd_that_points_at_shared_root() {
    let project_root = super::fresh_temp_dir("brehon-mux-shared-root");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert("worker-1".to_string(), project_root.clone());
    let mut reviewer_cwds = HashMap::new();
    reviewer_cwds.insert(
        "reviewer-1".to_string(),
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/reviewer/reviewer-1"),
    );

    let err = match Mux::factory(MuxConfig {
        cwd: project_root.clone(),
        worktree_isolation: true,
        worker_cwds,
        supervisor_cwd: Some(super::setup_fake_linked_worktree(
            &project_root,
            ".brehon/worktrees/supervisor/claude-code",
        )),
        reviewer_cwds,
        workers: 1,
        worker_names: vec!["worker-1".to_string()],
        supervisor_name: "claude-code".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    }) {
        Ok(_) => panic!("shared-root worker cwd should fail"),
        Err(err) => err,
    };

    assert!(
        err.to_string()
            .contains("worker 'worker-1' resolves to the shared repo root")
    );
}

#[test]
fn test_factory_rejects_worker_cwd_that_is_not_linked_worktree() {
    let project_root = super::fresh_temp_dir("brehon-mux-plain-dir");
    let worker_cwd = project_root.join(".brehon/worktrees/worker-1");
    std::fs::create_dir_all(&worker_cwd).expect("create plain worker dir");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert("worker-1".to_string(), worker_cwd);
    let mut reviewer_cwds = HashMap::new();
    reviewer_cwds.insert(
        "reviewer-1".to_string(),
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/reviewer/reviewer-1"),
    );

    let err = match Mux::factory(MuxConfig {
        cwd: project_root.clone(),
        worktree_isolation: true,
        worker_cwds,
        supervisor_cwd: Some(super::setup_fake_linked_worktree(
            &project_root,
            ".brehon/worktrees/supervisor/claude-code",
        )),
        reviewer_cwds,
        workers: 1,
        worker_names: vec!["worker-1".to_string()],
        supervisor_name: "claude-code".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    }) {
        Ok(_) => panic!("plain worker cwd should fail"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("worker 'worker-1' cwd")
            && err.to_string().contains("linked git worktree")
    );
}

#[tokio::test]
async fn test_deliver_prompt_routes_claude_supervisor_to_teams_inbox() {
    let home = std::env::temp_dir().join(format!("brehon-mux-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("create fake home");

    let mut mux = Mux::new(24, 80);
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
    )
    .expect("create supervisor pane");
    mux.add_pane(pane);

    {
        let pane = mux.get_mut("claude-code").expect("supervisor pane exists");
        pane.set_inbox_nudge_not_before(None);
    }

    mux.deliver_prompt("claude-code", "review complete", None)
        .await
        .expect("deliver prompt");

    let inbox_path = TeamsPaths::for_session_with_home("test-session", home.clone())
        .inbox_for("claude-code")
        .unwrap();
    let payload = std::fs::read_to_string(inbox_path).expect("read supervisor inbox");
    let messages: serde_json::Value = serde_json::from_str(&payload).expect("parse inbox");
    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["text"] == "review complete")
    );
    assert!(
        messages
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["from"] == "director")
    );
    assert!(
        mux.get("claude-code")
            .expect("supervisor pane exists")
            .pending_inbox_nudge()
    );
    let viewport = mux
        .get("claude-code")
        .expect("supervisor pane exists")
        .dump_viewport()
        .expect("dump viewport");
    assert!(
        !viewport.contains("Prompt delivery failed"),
        "successful inbox delivery should not emit an error banner: {viewport}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
