use super::{ScopedEnv, TEST_ENV_LOCK};
use crate::mux::*;
use crate::pane::panesmith_shim::FORCE_PANESMITH_SPAWN_FAILURE_PANE_ID;
use crate::pty::{Pty, PtyConfig};
use crate::teams::{TeamsManager, TeamsPaths};
use crate::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    Pane, PaneBackend, PaneKind, PaneState, PromptInjectionStrategy, SupervisorCli,
};
use brehon_types::{
    PaneExitedEvent, PaneOutputEvent, PaneStateChangedEvent, RuntimeEvent, RuntimeEventKind,
    RuntimeEventMeta, RuntimePaneBlockInfo, RuntimePaneBlockKind, RuntimePaneKind,
    RuntimePaneState, RuntimeSource,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

fn wait_for_agent_health_marker_json(marker_path: &std::path::Path) -> serde_json::Value {
    for _ in 0..50 {
        if let Ok(marker) = std::fs::read_to_string(marker_path)
            && let Ok(marker_json) = serde_json::from_str::<serde_json::Value>(&marker)
        {
            return marker_json;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for agent health marker at {marker_path:?}");
}

fn wait_for_agent_health_marker_exists(marker_path: &std::path::Path) {
    for _ in 0..50 {
        if marker_path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for agent health marker at {marker_path:?}");
}

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
fn test_factory_uses_panesmith_for_all_interactive_pty_roles() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-all-pty-roles");
    let config = MuxConfig {
        cwd: project_root,
        workers: 1,
        worker_names: vec!["claude-worker".to_string()],
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        supervisor_name: "claude-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        reviewer_names: vec!["claude-reviewer".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        advisor_names: vec!["claude-advisor".to_string()],
        advisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        research_names: vec!["claude-research".to_string()],
        research_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        include_director: false,
        rows: 24,
        cols: 140,
        ..Default::default()
    };

    let mut mux = Mux::factory(config).expect("create mux");

    for pane_id in [
        "claude-worker",
        "claude-supervisor",
        "claude-reviewer",
        "claude-advisor",
        "claude-research",
    ] {
        let pane = mux.get(pane_id).expect("pane exists");
        assert!(
            pane.is_panesmith_managed(),
            "{pane_id} should use Panesmith"
        );
        assert!(
            !pane.is_gateway_backed(),
            "{pane_id} should not be gateway-owned"
        );
        assert!(matches!(pane.backend, PaneBackend::None));
        assert_eq!(
            mux.pane_backend_ownership(pane_id),
            Some(PaneBackendOwnership::Panesmith)
        );
        assert!(
            mux.panesmith_snapshot(pane_id).is_some(),
            "{pane_id} should have a Panesmith snapshot"
        );
    }

    let shell_id = mux
        .add_shell("local-shell", std::env::temp_dir(), Some("cat"))
        .expect("add shell");
    let shell = mux.get(&shell_id).expect("shell pane exists");
    assert!(shell.is_panesmith_managed());
    assert_eq!(
        mux.pane_backend_ownership(&shell_id),
        Some(PaneBackendOwnership::Panesmith)
    );

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
}

#[test]
fn test_factory_uses_panesmith_for_agy_interactive_pty_roles() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-agy-roles");
    let config = MuxConfig {
        cwd: project_root,
        workers: 1,
        worker_names: vec!["agy-worker".to_string()],
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Agy),
        supervisor_name: "agy-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Agy),
        reviewer_names: vec!["agy-reviewer".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Agy),
        include_director: false,
        rows: 24,
        cols: 140,
        ..Default::default()
    };

    let mut mux = Mux::factory(config).expect("create mux");

    for pane_id in ["agy-worker", "agy-supervisor", "agy-reviewer"] {
        let pane = mux.get(pane_id).expect("pane exists");
        assert!(
            pane.is_panesmith_managed(),
            "{pane_id} should use Panesmith"
        );
        assert!(
            !pane.is_gateway_backed(),
            "{pane_id} should stay on the interactive PTY path"
        );
        assert_eq!(
            mux.pane_backend_ownership(pane_id),
            Some(PaneBackendOwnership::Panesmith)
        );
    }

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
}

#[test]
fn test_factory_keeps_acp_roles_gateway_owned_under_panesmith_default() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-gateway-non-regression");
    let config = MuxConfig {
        cwd: project_root,
        workers: 1,
        worker_names: vec!["codex-worker".to_string()],
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        supervisor_name: "codex-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        reviewer_names: vec!["codex-reviewer".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        advisor_names: vec!["codex-advisor".to_string()],
        advisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        research_names: vec!["codex-research".to_string()],
        research_cli: AgentAdapter::BuiltIn(SupervisorCli::Codex),
        include_director: false,
        rows: 24,
        cols: 140,
        ..Default::default()
    };

    let mut mux = Mux::factory(config).expect("create mux");

    assert_eq!(
        mux.pane_backend_ownership("codex-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    for pane_id in [
        "codex-worker",
        "codex-reviewer",
        "codex-advisor",
        "codex-research",
    ] {
        let pane = mux.get(pane_id).expect("pane exists");
        assert!(
            pane.is_gateway_backed(),
            "{pane_id} should stay gateway-owned"
        );
        assert!(!pane.is_panesmith_managed());
        assert_eq!(
            mux.pane_backend_ownership(pane_id),
            Some(PaneBackendOwnership::Gateway)
        );
    }

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
}

#[test]
fn test_factory_uses_panesmith_for_custom_interactive_pty_supervisor() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-custom-pty-supervisor");
    let mut mux = Mux::factory(MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "custom-supervisor".to_string(),
        supervisor_cli: custom_interactive_agent("custom-supervisor-agent", "cat", &[]),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    })
    .expect("create mux");

    let supervisor = mux
        .get("custom-supervisor")
        .expect("custom supervisor exists");
    assert!(supervisor.is_panesmith_managed());
    assert!(matches!(supervisor.backend, PaneBackend::None));
    assert_eq!(
        mux.pane_backend_ownership("custom-supervisor"),
        Some(PaneBackendOwnership::Panesmith)
    );
    assert!(mux.panesmith_snapshot("custom-supervisor").is_some());

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
}

#[test]
fn test_factory_uses_panesmith_for_custom_interactive_pty_roles() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-custom-pty-roles");
    let mut mux = Mux::factory(MuxConfig {
        cwd: project_root,
        workers: 1,
        worker_names: vec!["custom-worker".to_string()],
        worker_cli: custom_interactive_agent("custom-worker-agent", "sh", &["-c", "cat"]),
        supervisor_name: "custom-supervisor".to_string(),
        supervisor_cli: custom_interactive_agent("custom-supervisor-agent", "sh", &["-c", "cat"]),
        reviewer_names: vec!["custom-reviewer".to_string()],
        reviewer_cli: custom_interactive_agent("custom-reviewer-agent", "sh", &["-c", "cat"]),
        advisor_names: vec!["custom-advisor".to_string()],
        advisor_cli: custom_interactive_agent("custom-advisor-agent", "sh", &["-c", "cat"]),
        research_names: vec!["custom-research".to_string()],
        research_cli: custom_interactive_agent("custom-research-agent", "sh", &["-c", "cat"]),
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    })
    .expect("create mux");

    for pane_id in [
        "custom-worker",
        "custom-supervisor",
        "custom-reviewer",
        "custom-advisor",
        "custom-research",
    ] {
        let pane = mux.get(pane_id).expect("custom pane exists");
        assert!(
            pane.is_panesmith_managed(),
            "{pane_id} should be Panesmith-managed"
        );
        assert!(
            !pane.is_gateway_backed(),
            "{pane_id} should not be gateway-backed"
        );
        assert!(matches!(pane.backend, PaneBackend::None));
        assert_eq!(
            mux.pane_backend_ownership(pane_id),
            Some(PaneBackendOwnership::Panesmith)
        );
        assert!(
            mux.panesmith_snapshot(pane_id).is_some(),
            "{pane_id} should have a Panesmith snapshot"
        );
    }

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(mux.shutdown_all());
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

#[tokio::test]
async fn test_panesmith_role_resets_keep_interactive_pty_panes_panesmith_owned() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-role-resets");
    let config = MuxConfig {
        cwd: project_root,
        workers: 1,
        worker_names: vec!["claude-worker".to_string()],
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        supervisor_name: "claude-supervisor".to_string(),
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        reviewer_names: vec!["claude-reviewer".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        advisor_names: vec!["claude-advisor".to_string()],
        advisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        research_names: vec!["claude-research".to_string()],
        research_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        include_director: false,
        rows: 24,
        cols: 140,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");

    mux.reset_worker_gateway_session("claude-worker")
        .await
        .expect("reset worker");
    mux.reset_reviewer_session("claude-reviewer")
        .await
        .expect("reset reviewer");
    mux.reset_advisor_session("claude-advisor")
        .await
        .expect("reset advisor");
    mux.reset_research_session("claude-research")
        .await
        .expect("reset research");
    mux.reset_supervisor_session("claude-supervisor")
        .await
        .expect("reset supervisor");

    for pane_id in [
        "claude-worker",
        "claude-reviewer",
        "claude-advisor",
        "claude-research",
        "claude-supervisor",
    ] {
        let pane = mux.get(pane_id).expect("pane exists");
        assert!(
            pane.is_panesmith_managed(),
            "{pane_id} should remain Panesmith-managed after reset"
        );
        assert!(matches!(pane.backend, PaneBackend::None));
        assert_eq!(
            mux.pane_backend_ownership(pane_id),
            Some(PaneBackendOwnership::Panesmith)
        );
        assert!(mux.panesmith_snapshot(pane_id).is_some());
    }

    mux.shutdown_all().await;
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
fn test_factory_fails_when_panesmith_spawn_fails() {
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
    let err = match Mux::factory(config) {
        Ok(_) => panic!("panesmith spawn failure should be fatal"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("legacy PTY fallback is disabled"),
        "unexpected error: {err}"
    );
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

#[test]
fn test_panesmith_supervisor_inbox_recovery_sends_enter_not_prompt_text() {
    let project_root = super::fresh_temp_dir("brehon-mux-panesmith-inbox-recovery");
    let config = MuxConfig {
        cwd: project_root,
        workers: 0,
        supervisor_name: "shell-supervisor".to_string(),
        supervisor_cli: custom_interactive_agent("shell-supervisor", "sh", &["-c", "cat"]),
        include_director: false,
        rows: 24,
        cols: 100,
        ..Default::default()
    };
    let mut mux = Mux::factory(config).expect("create mux");
    let rt = tokio::runtime::Runtime::new().expect("create runtime");

    assert!(mux.is_panesmith_managed("shell-supervisor"));
    {
        let supervisor = mux
            .get_mut("shell-supervisor")
            .expect("supervisor pane exists");
        supervisor
            .append_output(b"\xe2\x9d\xaf \r\n")
            .expect("append empty prompt marker");
        supervisor.set_pending_inbox_nudge(true);
        supervisor.set_pending_inbox_nudge_since(Some(
            std::time::Instant::now() - Duration::from_secs(20),
        ));
        supervisor.set_last_output_at(std::time::Instant::now() - Duration::from_secs(10));
        supervisor.set_focused(false);
    }

    rt.block_on(mux.force_supervisor_inbox_recovery("shell-supervisor"));

    let supervisor = mux.get("shell-supervisor").expect("supervisor pane exists");
    assert!(
        !supervisor.pending_inbox_nudge(),
        "Panesmith Enter recovery should consume the pending inbox nudge"
    );
    let viewport = supervisor.dump_viewport().expect("dump viewport");
    assert!(
        !viewport.contains("Check your unread inbox"),
        "Panesmith recovery must not type synthetic inbox prompts into the mux pane"
    );

    let mut snapshot_text = String::new();
    for _ in 0..50 {
        let (_bytes, _events) = mux.poll_batch();
        snapshot_text = mux
            .panesmith_snapshot("shell-supervisor")
            .map(panesmith_snapshot_text)
            .unwrap_or_default();
        if !snapshot_text.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !snapshot_text.contains("Check your unread inbox"),
        "Panesmith recovery must send Enter only, not a prompt transaction"
    );

    rt.block_on(mux.shutdown_all());
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
            prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::InteractivePty,
            preferred_control_plane: HarnessControlPlane::PtyInjection,
        },
    })
}

fn grok_adapter(name: &str) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some("grok".to_string()),
        args: vec![
            "agent".to_string(),
            "--always-approve".to_string(),
            "stdio".to_string(),
        ],
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
            prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    })
}

fn custom_acp_adapter(name: &str) -> AgentAdapter {
    AgentAdapter::Custom(CustomAgentConfig {
        name: name.to_string(),
        command: Some("my-agent".to_string()),
        args: vec!["--stdio".to_string()],
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
            prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
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
fn terminal_host_blocked_state_event_writes_prompt_blocked_marker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

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

    let blocked = RuntimePaneBlockInfo {
        kind: RuntimePaneBlockKind::PermissionRequest,
        summary: "permission request blocked automatic recovery: allow bash ls".to_string(),
        command_or_tool: Some("allow bash ls".to_string()),
        request_id: Some("perm-1".to_string()),
        task_id: Some("T-owned".to_string()),
        excerpt: None,
    };
    let event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Ready),
            current: RuntimePaneState::Blocked,
            reason: Some("permission request blocked automatic recovery".to_string()),
            blocked: Some(blocked.clone()),
        }),
    );

    assert!(
        mux.apply_terminal_host_runtime_event(&event)
            .expect("apply blocked state event"),
        "host blocked state should update the local pane"
    );

    let marker_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("worker-1.json");
    let marker_json = wait_for_agent_health_marker_json(&marker_path);
    assert_eq!(
        marker_json.get("reason").and_then(|value| value.as_str()),
        Some("prompt_blocked")
    );
    assert_eq!(
        marker_json
            .get("blocked")
            .and_then(|value| value.get("task_id"))
            .and_then(|value| value.as_str()),
        Some("T-owned")
    );
}

#[test]
fn terminal_host_blocked_state_event_without_payload_uses_review_context_task_id() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

    let mut mux = Mux::new(5, 40);
    let pane = Pane::new_with_backend_cli(
        "reviewer-1",
        "reviewer-1",
        PaneKind::Reviewer,
        PaneBackend::None,
        5,
        40,
        AgentAdapter::BuiltIn(SupervisorCli::Claude),
    )
    .expect("create host-owned placeholder pane");
    mux.add_pane(pane);
    mux.get_mut("reviewer-1")
        .expect("reviewer pane")
        .set_review_context(crate::ReviewContextSnapshot {
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

    let event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "reviewer-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Busy),
            current: RuntimePaneState::Blocked,
            reason: Some("terminal approval prompt".to_string()),
            blocked: None,
        }),
    );

    assert!(
        mux.apply_terminal_host_runtime_event(&event)
            .expect("apply blocked state event"),
        "host blocked state should update the local reviewer pane"
    );

    let marker_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("reviewer-1.json");
    let marker_json = wait_for_agent_health_marker_json(&marker_path);
    assert_eq!(
        marker_json
            .get("blocked")
            .and_then(|value| value.get("task_id"))
            .and_then(|value| value.as_str()),
        Some("T-review")
    );
}

#[test]
fn terminal_host_duplicate_blocked_state_event_refreshes_marker_payload() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

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

    let original = RuntimePaneBlockInfo {
        kind: RuntimePaneBlockKind::PermissionRequest,
        summary: "permission request blocked automatic recovery: allow bash ls".to_string(),
        command_or_tool: Some("allow bash ls".to_string()),
        request_id: Some("perm-1".to_string()),
        task_id: Some("T-owned".to_string()),
        excerpt: None,
    };
    mux.get_mut("worker-1")
        .expect("worker pane")
        .set_pane_blocked(original.clone(), std::time::Instant::now());

    let duplicate = RuntimePaneBlockInfo {
        kind: RuntimePaneBlockKind::TerminalPrompt,
        summary: "terminal prompt blocked automatic recovery: Do you want to allow this command?"
            .to_string(),
        command_or_tool: Some("Do you want to allow this command?".to_string()),
        request_id: Some("prompt-2".to_string()),
        task_id: Some("T-other".to_string()),
        excerpt: None,
    };
    let event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Busy),
            current: RuntimePaneState::Blocked,
            reason: Some("duplicate blocked update".to_string()),
            blocked: Some(duplicate.clone()),
        }),
    );

    assert!(
        mux.apply_terminal_host_runtime_event(&event)
            .expect("apply duplicate blocked state event"),
        "duplicate host blocked state should still be acknowledged"
    );

    match mux.get("worker-1").expect("worker pane").pane_state() {
        Some(PaneState::Blocked { info, .. }) => {
            assert_eq!(info, &duplicate);
        }
        other => panic!("expected blocked pane state, got {other:?}"),
    }

    let marker_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("worker-1.json");
    let marker_json = wait_for_agent_health_marker_json(&marker_path);
    assert_eq!(
        marker_json
            .get("blocked")
            .and_then(|value| value.get("kind"))
            .and_then(|value| value.as_str()),
        Some("terminal_prompt")
    );
    assert_eq!(
        marker_json
            .get("blocked")
            .and_then(|value| value.get("request_id"))
            .and_then(|value| value.as_str()),
        Some("prompt-2")
    );
    assert_eq!(
        marker_json
            .get("blocked")
            .and_then(|value| value.get("task_id"))
            .and_then(|value| value.as_str()),
        Some("T-other")
    );
}

#[test]
fn terminal_host_ready_event_clears_prompt_blocked_marker() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

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

    let blocked = RuntimePaneBlockInfo {
        kind: RuntimePaneBlockKind::PermissionRequest,
        summary: "permission request blocked automatic recovery: allow bash ls".to_string(),
        command_or_tool: Some("allow bash ls".to_string()),
        request_id: Some("perm-1".to_string()),
        task_id: Some("T-owned".to_string()),
        excerpt: None,
    };
    let blocked_event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Ready),
            current: RuntimePaneState::Blocked,
            reason: Some("permission request blocked automatic recovery".to_string()),
            blocked: Some(blocked),
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&blocked_event)
            .expect("apply blocked state event")
    );

    let marker_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("worker-1.json");
    wait_for_agent_health_marker_exists(&marker_path);

    let ready_event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 102),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Blocked),
            current: RuntimePaneState::Ready,
            reason: Some("permission resolved".to_string()),
            blocked: None,
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&ready_event)
            .expect("apply ready state event")
    );

    assert!(
        !marker_path.exists(),
        "non-blocked terminal-host state should clear the prompt-blocked marker"
    );
    assert!(matches!(
        mux.get("worker-1").expect("worker pane").pane_state(),
        Some(PaneState::Ready { .. })
    ));
}

#[test]
fn terminal_host_ready_event_preempts_pending_async_blocked_marker_write() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("tempdir");
    let brehon_root = temp.path().join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let _env = ScopedEnv::set(&[("BREHON_ROOT", brehon_root.to_str().unwrap())]);

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

    let blocked = RuntimePaneBlockInfo {
        kind: RuntimePaneBlockKind::PermissionRequest,
        summary: "permission request blocked automatic recovery: allow bash ls".to_string(),
        command_or_tool: Some("allow bash ls".to_string()),
        request_id: Some("perm-1".to_string()),
        task_id: Some("T-owned".to_string()),
        excerpt: None,
    };
    let blocked_event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Ready),
            current: RuntimePaneState::Blocked,
            reason: Some("permission request blocked automatic recovery".to_string()),
            blocked: Some(blocked),
        }),
    );
    let ready_event = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 102),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Blocked),
            current: RuntimePaneState::Ready,
            reason: Some("permission resolved".to_string()),
            blocked: None,
        }),
    );

    assert!(
        mux.apply_terminal_host_runtime_event(&blocked_event)
            .expect("apply blocked state event")
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&ready_event)
            .expect("apply ready state event")
    );

    let marker_path = brehon_root
        .join("runtime")
        .join("agent-health")
        .join("worker-1.json");
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !marker_path.exists(),
        "ready state should suppress any pending async prompt-blocked marker write"
    );
}

#[test]
fn terminal_host_stale_ready_or_busy_event_does_not_revive_dead_pane() {
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

    let exited = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 101),
        RuntimeEventKind::PaneExited(PaneExitedEvent {
            exit_code: Some(1),
            reason: Some("session dropped".to_string()),
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&exited)
            .expect("apply exit event"),
        "pane exit should transition the pane to dead"
    );

    let ready = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 102),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Dead),
            current: RuntimePaneState::Ready,
            reason: Some("late ready".to_string()),
            blocked: None,
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&ready)
            .expect("apply stale ready event"),
        "stale ready event should still be acknowledged"
    );
    assert!(matches!(
        mux.get("worker-1").expect("worker pane").pane_state(),
        Some(PaneState::Dead { .. })
    ));

    let busy = RuntimeEvent::new(
        RuntimeEventMeta::new("session-1", "worker-1", 0, RuntimeSource::Headless, 103),
        RuntimeEventKind::PaneStateChanged(PaneStateChangedEvent {
            previous: Some(RuntimePaneState::Dead),
            current: RuntimePaneState::Busy,
            reason: Some("late busy".to_string()),
            blocked: None,
        }),
    );
    assert!(
        mux.apply_terminal_host_runtime_event(&busy)
            .expect("apply stale busy event"),
        "stale busy event should still be acknowledged"
    );
    let pane = mux.get("worker-1").expect("worker pane");
    assert!(matches!(pane.pane_state(), Some(PaneState::Dead { .. })));
    assert!(pane.has_exited(), "dead pane should stay marked exited");
    assert_eq!(pane.exit_code(), Some(1));
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
            prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
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

fn assert_factory_rejects_shared_root_worker(worker_cli: AgentAdapter) {
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
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        worker_cli,
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
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
fn test_factory_rejects_kimi_worker_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_worker(AgentAdapter::BuiltIn(SupervisorCli::Kimi));
}

#[test]
fn test_factory_rejects_opencode_worker_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_worker(AgentAdapter::BuiltIn(SupervisorCli::OpenCode));
}

#[test]
fn test_factory_rejects_grok_worker_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_worker(grok_adapter("grok-worker"));
}

#[test]
fn test_factory_rejects_custom_acp_worker_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_worker(custom_acp_adapter("custom-worker"));
}

fn assert_factory_rejects_shared_root_reviewer(reviewer_cli: AgentAdapter) {
    let project_root = super::fresh_temp_dir("brehon-mux-shared-root-reviewer");
    let mut worker_cwds = HashMap::new();
    worker_cwds.insert(
        "worker-1".to_string(),
        super::setup_fake_linked_worktree(&project_root, ".brehon/worktrees/worker/worker-1"),
    );
    let mut reviewer_cwds = HashMap::new();
    reviewer_cwds.insert("reviewer-1".to_string(), project_root.clone());

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
        supervisor_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        worker_cli: AgentAdapter::BuiltIn(SupervisorCli::Claude),
        reviewer_names: vec!["reviewer-1".to_string()],
        reviewer_cli,
        include_director: false,
        rows: 24,
        cols: 120,
        ..Default::default()
    }) {
        Ok(_) => panic!("shared-root reviewer cwd should fail"),
        Err(err) => err,
    };

    assert!(
        err.to_string()
            .contains("reviewer 'reviewer-1' resolves to the shared repo root")
    );
}

#[test]
fn test_factory_rejects_kimi_reviewer_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_reviewer(AgentAdapter::BuiltIn(SupervisorCli::Kimi));
}

#[test]
fn test_factory_rejects_opencode_reviewer_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_reviewer(AgentAdapter::BuiltIn(SupervisorCli::OpenCode));
}

#[test]
fn test_factory_rejects_grok_reviewer_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_reviewer(grok_adapter("grok-reviewer"));
}

#[test]
fn test_factory_rejects_custom_acp_reviewer_cwd_at_shared_root() {
    assert_factory_rejects_shared_root_reviewer(custom_acp_adapter("custom-reviewer"));
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
        None,
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
