use crate::harness::{
    AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane, HarnessTransport,
    SupervisorCli,
};
use crate::mux::AgentPaneMaterialization;
use crate::pane::{AgentTerminalLaunchPlan, Generation, Pane, PaneKind};
use crate::pty::PtyConfig;
use brehon_acp::GatewayProtocol;
use brehon_types::RuntimePaneKind;
use brehon_types::config::SandboxProfile;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn test_strip_literal_cursor_report_echo() {
    let input = b"hello ^[[12;34R world";
    let cleaned = Pane::strip_literal_cursor_reports(input);
    assert_eq!(cleaned.as_ref(), b"hello  world");
}

#[test]
fn test_strip_literal_cursor_report_noop_for_normal_text() {
    let input = b"normal output with [brackets] and numbers 12;34R";
    let cleaned = Pane::strip_literal_cursor_reports(input);
    assert_eq!(cleaned.as_ref(), input);
}

#[test]
fn test_pane_activity_state_defaults_to_busy_until_quiet() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");

    assert!(pane.is_tool_executing());
    assert!(!pane.is_idle(Instant::now(), Duration::from_millis(5)));

    pane.set_last_output_at(Instant::now() - Duration::from_millis(20));
    pane.set_tool_executing(false);
    assert!(pane.is_idle(Instant::now(), Duration::from_millis(5)));
}

#[test]
fn test_codex_worker_is_placeholder_without_manual_input() {
    let pane = Pane::worker(
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
    .expect("create codex worker pane");

    assert_eq!(pane.kind(), &PaneKind::Worker);
    assert_eq!(
        pane.cli_type(),
        &AgentAdapter::BuiltIn(SupervisorCli::Codex)
    );
    assert!(pane.accepts_manual_input());
}

#[test]
fn test_placeholder_worker_normalizes_lf_output_to_crlf() {
    let mut pane = Pane::worker(
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
    .expect("create codex worker pane");

    pane.append_output(b"alpha\nbeta").expect("append output");

    let mut found_alpha = false;
    let mut found_beta = false;
    for row_idx in 0..24 {
        if let Ok(row) = pane.dump_row(row_idx) {
            if row.contains("alpha") {
                found_alpha = true;
            }
            if row.contains("beta") {
                found_beta = true;
            }
        }
    }
    assert!(found_alpha, "should find 'alpha' in pane output");
    assert!(found_beta, "should find 'beta' in pane output");
}

#[test]
fn test_gateway_worker_starts_without_placeholder_banner() {
    let pane = Pane::worker(
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
    .expect("create codex worker pane");

    let viewport = pane.dump_viewport().expect("dump viewport");
    assert!(!viewport.contains("waiting for gateway session"));
    assert!(!viewport.contains("[gateway]"));
}

#[test]
fn terminal_launch_plan_from_pty_config_maps_spawn_spec() {
    let config = PtyConfig {
        command: "claude".to_string(),
        args: vec!["--model".to_string(), "opus".to_string()],
        cwd: Some(PathBuf::from("/tmp/brehon-worker")),
        env: vec![
            ("BETA".to_string(), "2".to_string()),
            ("ALPHA".to_string(), "old".to_string()),
            ("ALPHA".to_string(), "new".to_string()),
        ],
        rows: 31,
        cols: 101,
    };

    let plan = AgentTerminalLaunchPlan::from_pty_config(
        "session-1",
        "worker-1",
        Some("worker-1".to_string()),
        RuntimePaneKind::Worker,
        &config,
    );

    let AgentTerminalLaunchPlan::TerminalHost(launch) = plan else {
        panic!("expected terminal-host launch plan");
    };
    assert_eq!(launch.spec.session_id, "session-1");
    assert_eq!(launch.spec.pane_id, "worker-1");
    assert_eq!(launch.spec.kind, RuntimePaneKind::Worker);
    assert_eq!(launch.spec.title.as_deref(), Some("worker-1"));
    assert_eq!(launch.spec.cwd.as_deref(), Some("/tmp/brehon-worker"));
    assert_eq!(launch.spec.command, vec!["claude", "--model", "opus"]);
    assert_eq!(
        launch.spec.env.get("ALPHA").map(String::as_str),
        Some("new")
    );
    assert_eq!(launch.spec.env.get("BETA").map(String::as_str), Some("2"));
    assert_eq!(launch.spec.rows, 31);
    assert_eq!(launch.spec.cols, 101);

    let spawn = launch.to_runtime_spawn_command();
    let brehon_types::RuntimeCommandKind::SpawnPane {
        kind,
        pane_id,
        title,
        cwd,
        command,
        env,
        rows,
        cols,
    } = spawn
    else {
        panic!("expected spawn command");
    };
    assert_eq!(kind, RuntimePaneKind::Worker);
    assert_eq!(pane_id.as_deref(), Some("worker-1"));
    assert_eq!(title.as_deref(), Some("worker-1"));
    assert_eq!(cwd.as_deref(), Some("/tmp/brehon-worker"));
    assert_eq!(command, vec!["claude", "--model", "opus"]);
    assert_eq!(env.get("ALPHA").map(String::as_str), Some("new"));
    assert_eq!(rows, Some(31));
    assert_eq!(cols, Some(101));
}

#[test]
fn pty_backed_pane_reports_terminal_host_launch_plan() {
    let mut pane = Pane::director("director", 24, 80).expect("create director pane");
    pane.set_pty_spawn_config(PtyConfig {
        command: "bash".to_string(),
        args: vec!["-lc".to_string(), "echo ready".to_string()],
        cwd: Some(PathBuf::from("/tmp")),
        env: vec![("READY".to_string(), "1".to_string())],
        rows: 24,
        cols: 80,
    });

    let plan = pane.terminal_host_launch_plan("session-2");

    let AgentTerminalLaunchPlan::TerminalHost(launch) = plan else {
        panic!("expected terminal-host launch plan");
    };
    assert_eq!(launch.spec.session_id, "session-2");
    assert_eq!(launch.spec.pane_id, "director");
    assert_eq!(launch.spec.kind, RuntimePaneKind::Director);
    assert_eq!(launch.spec.command, vec!["bash", "-lc", "echo ready"]);
}

#[test]
fn gateway_backed_pane_reports_terminal_host_ineligible() {
    let pane = Pane::worker(
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
    .expect("create codex worker pane");

    let plan = pane.terminal_host_launch_plan("session-3");

    assert!(!plan.is_terminal_host_eligible());
    assert!(matches!(
        &plan,
        AgentTerminalLaunchPlan::GatewayBacked {
            protocol: "codex_app_server_ws",
            ..
        }
    ));
    assert_eq!(
        plan.promotion_blocker(),
        Some("gateway-backed codex_app_server_ws agent sessions are not terminal-host PTY panes")
    );
}

#[test]
fn test_gateway_session_spawns_increment_generation_monotonically() {
    let mut pane = Pane::worker(
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
    .expect("create codex worker pane");

    assert_eq!(pane.current_generation(), Generation(0));

    pane.register_gateway_session_spawn("session-1".to_string());
    let first_spawn_generation = pane.current_generation();
    assert_eq!(first_spawn_generation, Generation(1));

    // Phase 3 adds real recycle; for now force a recycle boundary manually.
    pane.clear_gateway_session();

    pane.register_gateway_session_spawn("session-2".to_string());
    let second_spawn_generation = pane.current_generation();
    assert_eq!(second_spawn_generation, Generation(2));
    assert!(second_spawn_generation > first_spawn_generation);
}

#[test]
fn test_status_line_is_forced_onto_new_line_after_streamed_text() {
    let mut pane = Pane::director("test", 24, 80).expect("create pane");

    pane.append_output(b"Starting Brehon worker session...")
        .expect("append streamed text");
    pane.append_output(b"\x1b[2mtool: brehon_agent\x1b[0m\r\n")
        .expect("append status line");

    let row0 = pane.dump_row(0).expect("row 0");
    let row1 = pane.dump_row(1).expect("row 1");

    assert!(row0.contains("Starting Brehon worker session..."));
    assert!(!row0.contains("tool: brehon_agent"));
    assert!(row1.contains("tool: brehon_agent"));
}

#[test]
fn test_builtin_supervisors_use_pty_sessions() {
    for cli in [
        SupervisorCli::Claude,
        SupervisorCli::Codex,
        SupervisorCli::Gemini,
        SupervisorCli::Kimi,
        SupervisorCli::OpenCode,
        SupervisorCli::Junie,
        SupervisorCli::Copilot,
    ] {
        let pane = Pane::supervisor(
            "supervisor",
            PathBuf::from("/tmp"),
            None,
            24,
            80,
            &AgentAdapter::BuiltIn(cli),
            &AgentAdapter::BuiltIn(SupervisorCli::Codex),
            &[],
            None,
            None,
            None,
            &HashMap::new(),
            None,
        )
        .expect("create supervisor pane");

        assert_eq!(pane.kind(), &PaneKind::Supervisor);
        assert!(
            !pane.is_gateway_backed(),
            "{cli:?} supervisor should use PTY"
        );
        assert!(
            pane.pty_spawn_config.is_some(),
            "{cli:?} supervisor should store PTY spawn config"
        );
        assert!(matches!(
            pane.terminal_host_launch_plan("session"),
            AgentTerminalLaunchPlan::TerminalHost(_)
        ));
        assert!(pane.accepts_manual_input());
    }
}

#[test]
fn test_claude_worker_safe_profile_omits_skip_permissions() {
    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        24,
        80,
        None,
        None,
        Some(SandboxProfile::OsDefault),
    )
    .expect("create claude worker pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        !config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "safe profile should NOT include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_worker_unsafe_profile_includes_skip_permissions() {
    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        24,
        80,
        None,
        None,
        Some(SandboxProfile::None),
    )
    .expect("create claude worker pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "unsafe profile should include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_supervisor_safe_profile_omits_skip_permissions() {
    let pane = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        Some(SandboxProfile::OsDefault),
    )
    .expect("create claude supervisor pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        !config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "safe profile should NOT include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_supervisor_unsafe_profile_includes_skip_permissions() {
    let pane = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        &[],
        None,
        None,
        None,
        &HashMap::new(),
        Some(SandboxProfile::None),
    )
    .expect("create claude supervisor pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "unsafe profile should include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_reviewer_safe_profile_omits_skip_permissions() {
    let pane = Pane::reviewer(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        Some(SandboxProfile::OsDefault),
    )
    .expect("create claude reviewer pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        !config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "safe profile should NOT include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_reviewer_unsafe_profile_includes_skip_permissions() {
    let pane = Pane::reviewer(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        Some(SandboxProfile::None),
    )
    .expect("create claude reviewer pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "unsafe profile should include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_advisor_safe_profile_omits_skip_permissions() {
    let pane = Pane::advisor_with_agent_type(
        "advisor-1",
        PathBuf::from("/tmp"),
        None,
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        None,
        None,
        &[],
        Some(SandboxProfile::OsDefault),
    )
    .expect("create claude advisor pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        !config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "safe profile should NOT include --dangerously-skip-permissions"
    );
}

#[test]
fn test_claude_advisor_unsafe_profile_includes_skip_permissions() {
    let pane = Pane::advisor_with_agent_type(
        "advisor-1",
        PathBuf::from("/tmp"),
        None,
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Claude),
        None,
        None,
        None,
        None,
        None,
        &[],
        Some(SandboxProfile::None),
    )
    .expect("create claude advisor pane");

    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert!(
        config
            .args
            .contains(&"--dangerously-skip-permissions".to_string()),
        "unsafe profile should include --dangerously-skip-permissions"
    );
}

#[test]
fn test_gateway_reviewer_preserves_configured_agent_type_alias() {
    let pane = Pane::reviewer_with_agent_type(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        None,
        None,
        None,
        Some("safety-codex"),
        None,
        &[],
        None,
    )
    .expect("create aliased codex reviewer pane");

    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "safety-codex")
    );
    assert_eq!(pane.configured_agent_type(), Some("safety-codex"));
}

#[test]
fn test_gemini_worker_uses_stdio_gateway_protocol() {
    let pane = Pane::worker(
        "worker-1",
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

    assert!(pane.is_gateway_backed());
    assert_eq!(
        pane.gateway_spawn_config().map(|config| config.protocol),
        Some(GatewayProtocol::GeminiAcpStdio)
    );
}

#[test]
fn test_opencode_worker_uses_server_gateway_protocol() {
    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create opencode worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::OpenCodeServer);
    assert_eq!(config.command.as_deref(), Some("opencode"));
    assert_eq!(config.args.first().map(String::as_str), Some("serve"));
    assert!(config.args.contains(&"--port".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, _)| k == "BREHON_OPENCODE_SERVER_URL")
    );
}

#[test]
fn test_kimi_worker_uses_acp_stdio_gateway_protocol() {
    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        Some("kimi-for-coding"),
        None,
        24,
        80,
        None,
        Some("high"),
        None,
    )
    .expect("create kimi worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert_eq!(config.command.as_deref(), Some("kimi"));
    assert_eq!(
        config.args,
        vec![
            "--work-dir".to_string(),
            "/tmp".to_string(),
            "acp".to_string(),
        ]
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "KIMI_CLI_NO_AUTO_UPDATE" && v == "true")
    );
    assert!(config.env.iter().any(|(k, _)| k == "KIMI_SHARE_DIR"));
    assert!(
        config
            .env
            .iter()
            .any(|(k, _)| k == "BREHON_ACP_MCP_SERVERS_JSON")
    );
}

#[test]
fn test_kimi_reviewer_uses_acp_stdio_gateway_protocol() {
    let pane = Pane::reviewer_with_agent_type(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Kimi),
        Some("kimi-for-coding"),
        None,
        None,
        None,
        Some("off"),
        &[],
        None,
    )
    .expect("create kimi reviewer pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert_eq!(config.command.as_deref(), Some("kimi"));
    assert_eq!(
        config.args,
        vec![
            "--work-dir".to_string(),
            "/tmp".to_string(),
            "acp".to_string(),
        ]
    );
}

#[test]
fn test_opencode_reviewer_uses_server_gateway_protocol() {
    let pane = Pane::reviewer(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::OpenCode),
        None,
        None,
        None,
        None,
    )
    .expect("create opencode reviewer pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::OpenCodeServer);
    assert_eq!(config.command.as_deref(), Some("opencode"));
    assert_eq!(config.args.first().map(String::as_str), Some("serve"));
    assert!(config.args.contains(&"--port".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, _)| k == "BREHON_OPENCODE_SERVER_URL")
    );
}

#[test]
fn test_copilot_worker_uses_stdio_acp_gateway_protocol() {
    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &AgentAdapter::BuiltIn(SupervisorCli::Copilot),
        Some("gpt-5"),
        None,
        24,
        80,
        None,
        Some("high"),
        None,
    )
    .expect("create copilot worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_copilot_gateway_config(config);
    assert!(config.args.contains(&"--model".to_string()));
    assert!(config.args.contains(&"gpt-5".to_string()));
}

#[test]
fn test_copilot_reviewer_uses_stdio_acp_gateway_protocol() {
    let pane = Pane::reviewer(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Copilot),
        None,
        None,
        None,
        None,
    )
    .expect("create copilot reviewer pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_copilot_gateway_config(config);
}

#[test]
fn test_codex_advisor_uses_advisor_role_metadata() {
    let pane = Pane::advisor_with_agent_type(
        "advisor-1",
        PathBuf::from("/tmp"),
        Some("brehon-test"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Codex),
        Some("gpt-5.3"),
        None,
        None,
        Some("codex-worker"),
        Some("medium"),
        &[],
        None,
    )
    .expect("create codex advisor pane");

    assert_eq!(pane.kind(), &PaneKind::Advisor);
    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::CodexAppServerWs);
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "advisor")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "codex-worker")
    );
}

#[test]
fn test_copilot_supervisor_uses_interactive_pty() {
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let pane = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &AgentAdapter::BuiltIn(SupervisorCli::Copilot),
        &worker_adapter,
        &worker_names,
        None,
        None,
        None,
        &worker_cli_map,
        None,
    )
    .expect("create copilot supervisor pane");

    assert!(!pane.is_gateway_backed());
    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("pty config should exist");
    assert_copilot_pty_config(config);
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_FACTORY_WORKER_NAMES" && v == "worker-1")
    );
}

#[test]
fn test_custom_acp_worker_uses_stdio_gateway_protocol() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "goose-worker".to_string(),
        command: Some("goose".to_string()),
        args: vec!["acp".to_string(), "--stdio".to_string()],
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
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create custom acp worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert_eq!(config.command.as_deref(), Some("goose"));
    assert_eq!(config.args, vec!["acp".to_string(), "--stdio".to_string()]);
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "worker-1")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "goose-worker")
    );
}

#[test]
fn test_grok_acp_worker_receives_brehon_mcp_server() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "grok-worker".to_string(),
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
            tool_prefix: std::borrow::Cow::Borrowed("brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let brehon_root = PathBuf::from("/tmp/.brehon");
    let pane = Pane::worker_with_agent_type(
        "worker-1",
        PathBuf::from("/tmp"),
        Some("session-a"),
        Some(&brehon_root),
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
        &[],
        None,
    )
    .expect("create grok worker pane");

    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["--cwd", "/tmp"])
    );
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["--sandbox", "workspace"])
    );
    let mcp_servers = config
        .env
        .iter()
        .find_map(|(key, value)| (key == "BREHON_ACP_MCP_SERVERS_JSON").then_some(value))
        .expect("grok acp mcp servers env");
    let parsed: serde_json::Value = serde_json::from_str(mcp_servers).unwrap();

    assert_eq!(parsed[0]["name"], "brehon");
    assert_eq!(parsed[0]["type"], "stdio");
    assert_eq!(parsed[0]["args"], serde_json::json!(["serve"]));
    assert!(
        parsed[0]["command"]
            .as_str()
            .is_some_and(|command| !command.is_empty())
    );
    assert_eq!(
        parsed[0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "BREHON_AGENT_NAME")
            .unwrap()["value"],
        "worker-1"
    );
    assert_eq!(
        parsed[0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "BREHON_SESSION_NAME")
            .unwrap()["value"],
        "session-a"
    );
}

#[test]
fn test_custom_codex_app_server_worker_uses_codex_ws_gateway_protocol() {
    let cwd =
        std::env::temp_dir().join(format!("brehon-custom-codex-pane-{}", uuid::Uuid::new_v4()));
    let brehon_root = cwd.join(".brehon");
    let instructions_dir = brehon_root.join("instructions");
    std::fs::create_dir_all(&instructions_dir).expect("create instructions dir");
    std::fs::write(
        instructions_dir.join("codex-worker-instructions.md"),
        "worker instructions\n",
    )
    .expect("write worker instructions");

    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "codex-ollama-worker".to_string(),
        command: Some("codex".to_string()),
        args: vec![
            "-c".to_string(),
            "model_provider=\"ollama_cloud\"".to_string(),
            "app-server".to_string(),
        ],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: false,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let pane = Pane::worker(
        "worker-1",
        cwd.clone(),
        Some(&brehon_root),
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create custom codex app-server worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::CodexAppServerWs);
    assert_eq!(config.command.as_deref(), Some("codex"));
    assert!(config.args.contains(&"app-server".to_string()));
    assert!(
        config
            .args
            .contains(&"model_provider=\"ollama_cloud\"".to_string())
    );
    assert!(
        config
            .args
            .iter()
            .any(|arg| arg.contains("model_instructions_file=")),
        "custom Codex lane should carry the standard Brehon instructions"
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "codex-ollama-worker")
    );
    assert!(
        config.env.iter().any(|(k, _)| k == "CODEX_HOME"),
        "custom Codex lane should bootstrap a local CODEX_HOME"
    );

    let _ = std::fs::remove_dir_all(cwd);
}

#[test]
fn test_custom_codex_app_server_worker_accepts_long_form_safe_bootstrap() {
    let cwd = std::env::temp_dir().join(format!(
        "brehon-custom-codex-pane-long-safe-{}",
        uuid::Uuid::new_v4()
    ));
    let brehon_root = cwd.join(".brehon");
    let instructions_dir = brehon_root.join("instructions");
    std::fs::create_dir_all(&instructions_dir).expect("create instructions dir");
    std::fs::write(
        instructions_dir.join("codex-worker-instructions.md"),
        "worker instructions\n",
    )
    .expect("write worker instructions");

    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "codex-ollama-worker".to_string(),
        command: Some("codex".to_string()),
        args: vec![
            "--ask-for-approval".to_string(),
            "never".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "app-server".to_string(),
        ],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: false,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let pane = Pane::worker(
        "worker-1",
        cwd.clone(),
        Some(&brehon_root),
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create custom codex app-server worker pane with long safe bootstrap");

    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["--ask-for-approval", "never"])
    );
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["--sandbox", "workspace-write"])
    );

    let _ = std::fs::remove_dir_all(cwd);
}

#[test]
fn test_custom_codex_app_server_worker_accepts_short_form_safe_bootstrap() {
    let cwd = std::env::temp_dir().join(format!(
        "brehon-custom-codex-pane-short-safe-{}",
        uuid::Uuid::new_v4()
    ));
    let brehon_root = cwd.join(".brehon");
    let instructions_dir = brehon_root.join("instructions");
    std::fs::create_dir_all(&instructions_dir).expect("create instructions dir");
    std::fs::write(
        instructions_dir.join("codex-worker-instructions.md"),
        "worker instructions\n",
    )
    .expect("write worker instructions");

    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "codex-ollama-worker".to_string(),
        command: Some("codex".to_string()),
        args: vec![
            "-a".to_string(),
            "never".to_string(),
            "-s".to_string(),
            "workspace-write".to_string(),
            "app-server".to_string(),
        ],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: false,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let pane = Pane::worker(
        "worker-1",
        cwd.clone(),
        Some(&brehon_root),
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create custom codex app-server worker pane with short safe bootstrap");

    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["-a", "never"])
    );
    assert!(
        config
            .args
            .windows(2)
            .any(|window| window == ["-s", "workspace-write"])
    );

    let _ = std::fs::remove_dir_all(cwd);
}

#[test]
fn test_custom_codex_app_server_worker_requires_instructions_bootstrap() {
    let cwd = std::env::temp_dir().join(format!(
        "brehon-custom-codex-pane-missing-instructions-{}",
        uuid::Uuid::new_v4()
    ));
    let brehon_root = cwd.join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");

    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "codex-ollama-worker".to_string(),
        command: Some("codex".to_string()),
        args: vec![
            "-c".to_string(),
            "model_provider=\"ollama_cloud\"".to_string(),
            "app-server".to_string(),
        ],
        base_url: None,
        api_key_env: None,
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: false,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let err = Pane::worker(
        "worker-1",
        cwd.clone(),
        Some(&brehon_root),
        "supervisor",
        &adapter,
        None,
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .err()
    .expect("missing instructions should fail fast");

    assert!(
        err.to_string().contains("codex-worker-instructions.md"),
        "{err}"
    );

    let _ = std::fs::remove_dir_all(cwd);
}

#[test]
fn test_custom_acp_reviewer_uses_stdio_gateway_protocol() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "goose-reviewer".to_string(),
        command: Some("goose".to_string()),
        args: vec!["acp".to_string()],
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
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });

    let pane = Pane::reviewer(
        "reviewer-1",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        None,
        None,
        None,
        None,
    )
    .expect("create custom acp reviewer pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert_eq!(config.command.as_deref(), Some("goose"));
    assert_eq!(config.args, vec!["acp".to_string()]);
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "reviewer-1")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "reviewer")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "goose-reviewer")
    );
}

#[test]
fn test_custom_acp_supervisor_is_rejected_without_pty_contract() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "goose-supervisor".to_string(),
        command: Some("goose".to_string()),
        args: vec!["acp".to_string()],
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
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::Acp,
        },
    });
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let err = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &worker_names,
        None,
        None,
        None,
        &worker_cli_map,
        None,
    )
    .err()
    .expect("custom ACP supervisor should fail without PTY contract");

    assert!(
        err.to_string()
            .contains("must be configured as an interactive PTY supervisor"),
        "{err}"
    );
}

#[test]
fn test_custom_pty_supervisor_uses_pty_launch_contract() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "pty-supervisor".to_string(),
        command: Some("sh".to_string()),
        args: vec!["-lc".to_string(), "cat".to_string()],
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
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let pane = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &worker_names,
        None,
        None,
        None,
        &worker_cli_map,
        None,
    )
    .expect("create custom pty supervisor pane");

    assert!(!pane.is_gateway_backed());
    assert!(matches!(
        pane.terminal_host_launch_plan("session"),
        AgentTerminalLaunchPlan::TerminalHost(_)
    ));
    let config = pane
        .pty_spawn_config
        .as_ref()
        .expect("PTY config should exist");
    assert_eq!(config.command, "sh");
    assert_eq!(config.args, vec!["-lc".to_string(), "cat".to_string()]);
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_TYPE" && v == "pty-supervisor")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_FACTORY_SUPERVISOR_CLI" && v == "pty-supervisor")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_FACTORY_WORKER_NAMES" && v == "worker-1")
    );
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_FACTORY_WORKER_POOL" && v == r#"{"worker-1":"codex"}"#)
    );
}

#[test]
fn test_custom_acp_sidecar_supervisor_has_pty_and_gateway_contract() {
    let cwd =
        std::env::temp_dir().join(format!("brehon-acp-sidecar-pane-{}", uuid::Uuid::new_v4()));
    let brehon_root = cwd.join(".brehon");
    std::fs::create_dir_all(&brehon_root).expect("create brehon root");
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "native-supervisor".to_string(),
        command: Some("sh".to_string()),
        args: vec!["-lc".to_string(), "cat".to_string()],
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
            preferred_control_plane: HarnessControlPlane::AcpSidecar,
        },
    });
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let pane = Pane::supervisor_with_agent_type_materialized(
        "supervisor",
        cwd,
        Some("runtime-session"),
        Some(&brehon_root),
        24,
        80,
        &adapter,
        &worker_adapter,
        &worker_names,
        None,
        None,
        None,
        &worker_cli_map,
        None,
        &HashMap::new(),
        None,
        &[],
        AgentPaneMaterialization::PlanOnly,
        None,
    )
    .expect("create custom ACP sidecar supervisor pane");

    let pty_config = pane
        .pty_spawn_config
        .as_ref()
        .expect("PTY config should exist");
    let gateway_config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(gateway_config.protocol, GatewayProtocol::AcpUnixSocket);
    assert!(gateway_config.command.is_none());
    assert_eq!(gateway_config.sidecar_connect_timeout_ms, Some(5_000));
    let socket_path = gateway_config
        .sidecar_socket_path
        .as_deref()
        .expect("socket path should exist");
    let ready_path = gateway_config
        .sidecar_ready_path
        .as_deref()
        .expect("ready path should exist");
    assert!(socket_path.ends_with("/acp.sock"), "{socket_path}");
    assert!(ready_path.ends_with("/acp.ready.json"), "{ready_path}");
    assert!(
        pty_config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_NATIVE_AGENT_SOCKET" && v == socket_path)
    );
    assert!(
        pty_config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_NATIVE_AGENT_READY_FILE" && v == ready_path)
    );
    assert!(
        gateway_config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_NATIVE_AGENT_SOCKET" && v == socket_path)
    );
    assert!(matches!(
        pane.terminal_host_launch_plan("session"),
        AgentTerminalLaunchPlan::TerminalHost(_)
    ));
}

#[test]
fn test_custom_acp_sidecar_supervisor_rejects_non_interactive_transport() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "native-supervisor".to_string(),
        command: Some("native-agent".to_string()),
        args: vec![],
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
            transport: HarnessTransport::AppServer,
            preferred_control_plane: HarnessControlPlane::AcpSidecar,
        },
    });
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let err = Pane::supervisor_with_agent_type_materialized(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &worker_names,
        None,
        None,
        None,
        &worker_cli_map,
        None,
        &HashMap::new(),
        None,
        &[],
        AgentPaneMaterialization::PlanOnly,
        None,
    )
    .err()
    .expect("custom ACP sidecar supervisor should reject app-server transport");

    assert!(
        err.to_string()
            .contains("transport=app_server control_plane=acp_sidecar"),
        "{err}"
    );
}

#[test]
fn test_custom_openai_worker_uses_managed_api_gateway_protocol() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "ollama-direct".to_string(),
        command: None,
        args: vec![],
        base_url: Some("https://ollama.example/v1".to_string()),
        api_key_env: Some("OLLAMA_API_KEY".to_string()),
        headers: vec![("x-provider".to_string(), "ollama-cloud".to_string())],
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::ManagedApi,
            preferred_control_plane: HarnessControlPlane::OpenAiCompatible,
        },
    });

    let pane = Pane::worker(
        "worker-1",
        PathBuf::from("/tmp"),
        None,
        "supervisor",
        &adapter,
        Some("gpt-5.4-mini"),
        None,
        24,
        80,
        None,
        None,
        None,
    )
    .expect("create direct api worker pane");

    assert!(pane.is_gateway_backed());
    let config = pane
        .gateway_spawn_config()
        .expect("gateway config should exist");
    assert_eq!(config.protocol, GatewayProtocol::OpenAiCompatibleChat);
    assert!(config.command.is_none());
    assert_eq!(
        config.base_url.as_deref(),
        Some("https://ollama.example/v1")
    );
    assert_eq!(config.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
    assert_eq!(
        config.headers,
        vec![("x-provider".to_string(), "ollama-cloud".to_string())]
    );
    assert_eq!(config.model.as_deref(), Some("gpt-5.4-mini"));
}

#[test]
fn test_custom_openai_supervisor_is_rejected_without_pty_command() {
    let adapter = AgentAdapter::Custom(CustomAgentConfig {
        name: "openai-supervisor".to_string(),
        command: None,
        args: vec![],
        base_url: Some("https://api.openai.example/v1".to_string()),
        api_key_env: Some("OPENAI_API_KEY".to_string()),
        headers: Vec::new(),
        capabilities: HarnessCapabilities {
            supports_hooks: false,
            supports_subagents: false,
            supports_textbox_submit: true,
            supports_teams: false,
            one_shot: false,
            uses_ink_prompt: false,
            tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
            transport: HarnessTransport::ManagedApi,
            preferred_control_plane: HarnessControlPlane::OpenAiCompatible,
        },
    });
    let worker_names = vec!["worker-1".to_string()];
    let worker_adapter = AgentAdapter::BuiltIn(SupervisorCli::Codex);
    let mut worker_cli_map = HashMap::new();
    worker_cli_map.insert("worker-1".to_string(), worker_adapter.clone());

    let err = Pane::supervisor(
        "supervisor",
        PathBuf::from("/tmp"),
        None,
        24,
        80,
        &adapter,
        &worker_adapter,
        &worker_names,
        Some("gpt-5.4"),
        None,
        None,
        &worker_cli_map,
        None,
    )
    .err()
    .expect("custom API supervisor should fail without PTY command");

    assert!(
        err.to_string()
            .contains("must provide an interactive PTY launch command"),
        "{err}"
    );
}

fn assert_copilot_gateway_config(config: &crate::pane::types::GatewaySpawnConfig) {
    assert_eq!(config.protocol, GatewayProtocol::AcpStdio);
    assert!(matches!(
        config.command.as_deref(),
        Some("copilot") | Some("gh")
    ));
    assert!(config.args.contains(&"--acp".to_string()));
    assert!(config.args.contains(&"--stdio".to_string()));
    assert!(config.args.contains(&"--allow-all".to_string()));
    assert!(config.args.contains(&"--no-ask-user".to_string()));
    assert!(config.args.contains(&"--no-auto-update".to_string()));
    assert!(config.args.contains(&"--config-dir".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "COPILOT_AUTO_UPDATE" && v == "false")
    );
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_HOME"));
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_CACHE_HOME"));
}

fn assert_copilot_pty_config(config: &brehon_pty::PtyConfig) {
    assert!(matches!(config.command.as_str(), "copilot" | "gh"));
    assert!(!config.args.contains(&"--acp".to_string()));
    assert!(!config.args.contains(&"--stdio".to_string()));
    assert!(config.args.contains(&"--allow-all".to_string()));
    assert!(config.args.contains(&"--no-ask-user".to_string()));
    assert!(config.args.contains(&"--no-auto-update".to_string()));
    assert!(config.args.contains(&"--config-dir".to_string()));
    assert!(
        config
            .env
            .iter()
            .any(|(k, v)| k == "COPILOT_AUTO_UPDATE" && v == "false")
    );
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_HOME"));
    assert!(config.env.iter().any(|(k, _)| k == "COPILOT_CACHE_HOME"));
}
