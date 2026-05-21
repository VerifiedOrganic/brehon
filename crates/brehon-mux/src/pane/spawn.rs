//! Pane constructors and process spawn helpers.

use crate::error::{Error, Result};
use crate::harness::{AgentAdapter, SupervisorCli};
use crate::mux::AgentPaneMaterialization;
use crate::pane::types::{GatewaySpawnConfig, Pane, PaneBackend, PaneKind};
use crate::pty::{Pty, PtyConfig, TeamsSpawnConfig};
use brehon_acp::GatewayProtocol;
use ghostty_vt::{Rgb, Terminal};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACP_SIDECAR_CONNECT_TIMEOUT_MS: u64 = 5_000;

pub(crate) fn uses_ink_echo_injection(cli_type: &AgentAdapter) -> bool {
    matches!(
        cli_type.as_builtin(),
        Some(SupervisorCli::Codex | SupervisorCli::OpenCode | SupervisorCli::Junie)
    )
}

pub(crate) fn uses_delayed_submit_injection(cli_type: &AgentAdapter) -> bool {
    // Only used as fallback when ACP delivery is unavailable.
    matches!(cli_type.as_builtin(), Some(SupervisorCli::Gemini))
}

fn gateway_protocol_for(cli_type: &AgentAdapter) -> GatewayProtocol {
    match cli_type {
        AgentAdapter::BuiltIn(SupervisorCli::Codex) => GatewayProtocol::CodexAppServerWs,
        AgentAdapter::BuiltIn(SupervisorCli::Gemini) => GatewayProtocol::GeminiAcpStdio,
        AgentAdapter::BuiltIn(SupervisorCli::Kimi) => GatewayProtocol::AcpStdio,
        AgentAdapter::BuiltIn(SupervisorCli::OpenCode) => GatewayProtocol::OpenCodeServer,
        AgentAdapter::Custom(custom) if is_custom_codex_app_server(custom) => {
            GatewayProtocol::CodexAppServerWs
        }
        AgentAdapter::Custom(custom)
            if custom.capabilities.preferred_control_plane
                == crate::harness::HarnessControlPlane::AcpSidecar =>
        {
            GatewayProtocol::AcpUnixSocket
        }
        AgentAdapter::Custom(custom)
            if custom.capabilities.preferred_control_plane
                == crate::harness::HarnessControlPlane::OpenAiCompatible =>
        {
            GatewayProtocol::OpenAiCompatibleChat
        }
        _ => GatewayProtocol::AcpStdio,
    }
}

#[cfg(any(test, feature = "test-pty-fallback"))]
pub(crate) fn spawn_config_for_pty_spawn(config: &PtyConfig) -> PtyConfig {
    let mut config = config.clone();
    apply_test_pty_spawn_fallback(&mut config);
    config
}

#[cfg(not(any(test, feature = "test-pty-fallback")))]
pub(crate) fn spawn_config_for_pty_spawn(config: &PtyConfig) -> PtyConfig {
    config.clone()
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn apply_test_pty_spawn_fallback(config: &mut PtyConfig) {
    if !is_test_pty_fallback_candidate(&config.command) || command_exists(&config.command) {
        return;
    }

    config.command = "sh".to_string();
    config.args = vec!["-c".to_string(), "cat".to_string()];
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn is_test_pty_fallback_candidate(command: &str) -> bool {
    matches!(
        command,
        "claude" | "codex" | "copilot" | "gemini" | "gh" | "junie" | "kimi" | "opencode" | "agy"
    )
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn command_exists(command: &str) -> bool {
    let path = std::path::Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }

    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

fn allocate_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| Error::pty(format!("Failed to allocate loopback port: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| Error::pty(format!("Failed to read allocated loopback port: {err}")))?
        .port();
    drop(listener);
    Ok(port)
}

fn is_custom_codex_app_server(custom: &crate::harness::CustomAgentConfig) -> bool {
    custom.command.as_deref() == Some("codex") && custom.args.iter().any(|arg| arg == "app-server")
}

fn custom_supervisor_requires_pty_error(
    adapter: &AgentAdapter,
    custom: &crate::harness::CustomAgentConfig,
) -> Option<String> {
    use crate::harness::{HarnessControlPlane, HarnessTransport};

    let capabilities = adapter.capabilities();
    if custom.command.as_deref().is_none() {
        return Some(format!(
            "Custom supervisor agent '{}' must provide an interactive PTY launch command; gateway/API-only supervisors are not supported",
            adapter.name()
        ));
    }
    let has_valid_supervisor_contract = match capabilities.preferred_control_plane {
        HarnessControlPlane::NativeHooks | HarnessControlPlane::PtyInjection => matches!(
            capabilities.transport,
            HarnessTransport::NativeHooks | HarnessTransport::InteractivePty
        ),
        HarnessControlPlane::AcpSidecar => {
            matches!(capabilities.transport, HarnessTransport::InteractivePty)
        }
        _ => false,
    };
    if !has_valid_supervisor_contract {
        return Some(format!(
            "Custom supervisor agent '{}' must be configured as an interactive PTY supervisor; got transport={} control_plane={}",
            adapter.name(),
            capabilities.transport,
            capabilities.preferred_control_plane
        ));
    }
    None
}

pub(crate) fn config_env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find_map(|(env_key, value)| (env_key == key).then(|| value.clone()))
}

fn set_config_env_value(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn acp_sidecar_contract_paths(
    config: &PtyConfig,
    brehon_root: Option<&PathBuf>,
    pane_name: &str,
) -> Result<(String, String)> {
    let root = brehon_root
        .cloned()
        .or_else(|| config_env_value(&config.env, "BREHON_ROOT").map(PathBuf::from))
        .ok_or_else(|| {
            Error::pty(format!(
                "Custom ACP sidecar supervisor '{pane_name}' requires BREHON_ROOT to publish its socket contract"
            ))
        })?;
    let session_id = config_env_value(&config.env, "BREHON_SESSION_ID").ok_or_else(|| {
        Error::pty(format!(
            "Custom ACP sidecar supervisor '{pane_name}' is missing BREHON_SESSION_ID"
        ))
    })?;
    let agent_name =
        config_env_value(&config.env, "BREHON_AGENT_NAME").unwrap_or_else(|| pane_name.to_string());
    let sidecar_dir = root
        .join("runtime")
        .join("sessions")
        .join(session_id)
        .join("agents")
        .join(agent_name);
    std::fs::create_dir_all(&sidecar_dir)?;
    let socket_path = sidecar_dir.join("acp.sock");
    let ready_path = sidecar_dir.join("acp.ready.json");
    Ok((
        socket_path.to_string_lossy().to_string(),
        ready_path.to_string_lossy().to_string(),
    ))
}

fn validate_codex_gateway_bootstrap(
    config: &PtyConfig,
    brehon_root: Option<&PathBuf>,
) -> Result<()> {
    if config.command != "codex" || !config.args.iter().any(|arg| arg == "app-server") {
        return Ok(());
    }

    if !config
        .env
        .iter()
        .any(|(key, value)| key == "CODEX_HOME" && !value.trim().is_empty())
    {
        return Err(Error::pty(
            "Codex app-server launch is missing CODEX_HOME bootstrap. Refusing to start a half-configured Codex session."
                .to_string(),
        ));
    }

    if !config
        .args
        .windows(2)
        .any(|window| window == ["--disable", "personality"])
    {
        return Err(Error::pty(
            "Codex app-server launch is missing '--disable personality'. Refusing to start without the standard Brehon Codex bootstrap."
                .to_string(),
        ));
    }

    if !config
        .args
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox")
    {
        return Err(Error::pty(
            "Codex app-server launch is missing the standard approval/sandbox bootstrap flags. Refusing to start."
                .to_string(),
        ));
    }

    let Some(brehon_root) = brehon_root else {
        return Ok(());
    };
    let role = config_env_value(&config.env, "BREHON_AGENT_ROLE").unwrap_or_default();
    let instructions_filename = match role.as_str() {
        "supervisor" => "codex-supervisor-instructions.md",
        "reviewer" => "codex-reviewer-instructions.md",
        "advisor" => "codex-advisor-instructions.md",
        _ => "codex-worker-instructions.md",
    };
    let instructions_path = brehon_root.join("instructions").join(instructions_filename);
    if !instructions_path.exists() {
        return Err(Error::pty(format!(
            "Codex app-server launch for role '{role}' requires '{}', but it does not exist. Refusing to start a degraded Codex session.",
            instructions_path.display()
        )));
    }
    let instructions_path_str = instructions_path.to_string_lossy();
    if !config.args.iter().any(|arg| {
        arg.contains("model_instructions_file=") && arg.contains(instructions_path_str.as_ref())
    }) {
        return Err(Error::pty(format!(
            "Codex app-server launch for role '{role}' is missing model_instructions_file='{}'. Refusing to start a degraded Codex session.",
            instructions_path.display()
        )));
    }

    Ok(())
}

fn apply_configured_agent_type(config: &mut PtyConfig, configured_agent_type: Option<&str>) {
    let Some(configured_agent_type) = configured_agent_type
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    set_config_env_value(&mut config.env, "BREHON_AGENT_TYPE", configured_agent_type);
}

fn merge_launcher_env(target_env: &mut Vec<(String, String)>, launcher_env: &[(String, String)]) {
    for (key, value) in launcher_env {
        if (key.starts_with("BREHON_") && key != "BREHON_ROLE_SYSTEM_PROMPT")
            || key == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"
        {
            continue;
        }
        set_config_env_value(target_env, key, value);
    }
}

fn apply_runtime_model_metadata(
    env: &mut Vec<(String, String)>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        set_config_env_value(env, "BREHON_AGENT_MODEL", model);
    }
    if let Some(reasoning_effort) = reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        set_config_env_value(env, "BREHON_REASONING_EFFORT", reasoning_effort);
    }
}

fn apply_runtime_session_name(env: &mut Vec<(String, String)>, session_name: Option<&str>) {
    let Some(session_name) = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        // None/empty is never valid in production — every agent spawn must
        // carry a resolved session name so the spawned MCP subprocess can
        // stamp prompt-queue writes with the active runtime session. A silent
        // no-op here causes prompt entries to be tagged with a mismatched
        // scope and swept before delivery. Log loudly so regressions surface
        // immediately instead of manifesting as undelivered messages.
        tracing::error!(
            "apply_runtime_session_name called with empty/None session_name — \
             spawned agent MCP children cannot stamp prompt entries with the \
             active runtime session. Fix the caller to thread the live session \
             name through."
        );
        return;
    };
    set_config_env_value(env, "BREHON_SESSION_NAME", session_name);
}

#[allow(clippy::too_many_arguments)]
fn build_gateway_metadata_env(
    name: &str,
    role: &str,
    session_name: Option<&str>,
    agent_type: Option<&str>,
    cwd: &std::path::Path,
    brehon_root: Option<&PathBuf>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("BREHON_AGENT_NAME".to_string(), name.to_string()),
        ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
        (
            "BREHON_AGENT_TYPE".to_string(),
            agent_type.unwrap_or(name).to_string(),
        ),
        (
            "BREHON_SESSION_ID".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ),
        (
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
    ];

    if let Some(root) = brehon_root {
        env.push((
            "BREHON_ROOT".to_string(),
            root.to_string_lossy().to_string(),
        ));
        if root.file_name().and_then(|name| name.to_str()) == Some(".brehon")
            && let Some(project_root) = root.parent().filter(|path| !path.as_os_str().is_empty())
        {
            env.push((
                "BREHON_PROJECT_ROOT".to_string(),
                project_root.to_string_lossy().to_string(),
            ));
        }
    }
    env.push((
        "BREHON_WORKSPACE_ROOT".to_string(),
        cwd.to_string_lossy().to_string(),
    ));
    if let Some(supervisor_name) = supervisor_name {
        env.push((
            "BREHON_SUPERVISOR_NAME".to_string(),
            supervisor_name.to_string(),
        ));
    }
    if let Some(factory_worker_cli) = factory_worker_cli {
        env.push((
            "BREHON_FACTORY_WORKER_CLI".to_string(),
            factory_worker_cli.to_string(),
        ));
    }
    apply_runtime_session_name(&mut env, session_name);

    env
}

// Disabled: Gemini exits 0 when receiving a single Ctrl-C at an empty prompt.
// Long-term fix is migrating Gemini to native ACP delivery.
pub(crate) fn uses_pre_submit_interrupt_reset(_cli_type: &AgentAdapter) -> bool {
    false
}

/// Delay between the two Ctrl-C pulses in the double-tap interrupt reset.
pub(crate) const PRE_SUBMIT_INTER_INTERRUPT_DELAY: Duration = Duration::from_millis(80);
/// Settle time after the second Ctrl-C, giving the CLI time to redraw.
pub(crate) const PRE_SUBMIT_SETTLE_DELAY: Duration = Duration::from_millis(150);

impl Pane {
    /// Create a new pane with a specific backend.
    fn new_with_backend(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: PaneKind,
        backend: PaneBackend,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::new_with_backend_cli(
            id,
            title,
            kind,
            backend,
            rows,
            cols,
            AgentAdapter::BuiltIn(SupervisorCli::Claude),
        )
    }

    pub(crate) fn new_with_backend_cli(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: PaneKind,
        backend: PaneBackend,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
    ) -> Result<Self> {
        let id = id.into();
        let terminal = Terminal::new(rows, cols).map_err(|e| Error::terminal(e.to_string()))?;
        terminal.set_default_colors(Rgb { r: 0, g: 0, b: 0 }, Rgb { r: 0, g: 0, b: 0 });
        let info = terminal.scrollback_info();
        let mut pane = Self {
            title: title.into(),
            id,
            kind,
            terminal,
            backend,
            focused: false,
            color: None,
            exited: false,
            exit_code: None,
            rows,
            cols,
            recorder: None,
            force_all_dirty: true,
            render_generation: 0,
            last_total_scrollback: info.total_scrollback,
            seq_counter: 0,
            cli_type,
            configured_agent_type: None,
            last_output_at: Instant::now(),
            is_tool_executing: true,
            pending_messages: VecDeque::new(),
            notify_socket_path: None,
            agent_session_id: None,
            pending_ink_submit: Arc::new(std::sync::Mutex::new(None)),
            ink_submit_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            synthetic_prev_was_cr: false,
            supervisor_pending_structured_output: Vec::new(),
            gateway_session_id: None,
            current_generation: crate::pane::Generation(0),
            pty_spawn_config: None,
            gateway_spawn_config: None,
            gateway_terminal_id: None,
            gateway_event_bridge_started: false,
            pending_inbox_nudge: false,
            pending_inbox_nudge_since: None,
            inbox_nudge_not_before: None,
            activity_buffer: None,
            prompt_queue: crate::pane::state::PanePromptQueue::default(),
            pane_state: None,
            task_context: None,
            review_context: None,
        };
        pane.arm_claude_inbox_nudge_grace_period();
        Ok(pane)
    }

    pub(crate) fn set_agent_session_id(&mut self, session_id: Option<String>) {
        self.agent_session_id = session_id;
    }

    pub(crate) fn set_configured_agent_type(&mut self, configured_agent_type: Option<&str>) {
        self.configured_agent_type = configured_agent_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    pub(crate) fn set_pty_spawn_config(&mut self, config: PtyConfig) {
        self.pty_spawn_config = Some(config);
    }

    #[allow(clippy::too_many_arguments)]
    fn pty_pane_from_config(
        name: &str,
        kind: PaneKind,
        mut config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        configured_agent_type: Option<&str>,
        adapter_name: &str,
        materialization: AgentPaneMaterialization,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        // Open the PTY at the layout-aware size. Several TUI-style CLIs read
        // TIOCGWINSZ during startup and commit to that geometry before the
        // first resize can be delivered.
        config.rows = rows;
        config.cols = cols;
        let stored_config = config.clone();
        let mut pane = match materialization {
            AgentPaneMaterialization::Spawn => {
                let spawn_config = spawn_config_for_pty_spawn(&config);
                let pty = Pty::spawn(name, spawn_config)?;
                Self::with_pty_cli(name, kind, pty, rows, cols, cli_type)?
            }
            AgentPaneMaterialization::PlanOnly => {
                let mut pane = Self::new_with_backend_cli(
                    name,
                    name,
                    kind,
                    PaneBackend::None,
                    rows,
                    cols,
                    cli_type,
                )?;
                pane.set_tool_executing(false);
                pane
            }
        };
        pane.set_agent_session_id(session_id);
        pane.set_configured_agent_type(configured_agent_type.or(Some(adapter_name)));
        pane.set_pty_spawn_config(stored_config);
        pane.set_notify_socket_path(brehon_root, name);
        Ok(pane)
    }

    /// Create a new pane with a PTY
    pub fn with_pty(
        id: impl Into<String>,
        kind: PaneKind,
        pty: Pty,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::with_pty_cli(
            id,
            kind,
            pty,
            rows,
            cols,
            AgentAdapter::BuiltIn(SupervisorCli::Claude),
        )
    }

    /// Create a new pane with a PTY and explicit agent adapter
    pub fn with_pty_cli(
        id: impl Into<String>,
        kind: PaneKind,
        pty: Pty,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
    ) -> Result<Self> {
        let id_str: String = id.into();
        Self::new_with_backend_cli(
            id_str.clone(),
            id_str,
            kind,
            PaneBackend::Pty(pty),
            rows,
            cols,
            cli_type,
        )
    }

    /// Create a director pane (no PTY)
    pub fn director(id: impl Into<String>, rows: u16, cols: u16) -> Result<Self> {
        let id_str: String = id.into();
        Self::new_with_backend(
            id_str,
            "Director",
            PaneKind::Director,
            PaneBackend::None,
            rows,
            cols,
        )
    }

    /// Create a shell pane running the user's default shell (or a specific command).
    pub fn shell(
        name: &str,
        cwd: PathBuf,
        shell_command: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let shell = shell_command
            .map(|s| s.to_string())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string()));

        let config = PtyConfig {
            command: shell,
            args: vec![],
            cwd: Some(cwd),
            env: vec![],
            rows,
            cols,
        };
        let pty = Pty::spawn(name, config)?;
        let mut pane = Self::with_pty(name, PaneKind::Shell, pty, rows, cols)?;
        pane.set_tool_executing(false);
        Ok(pane)
    }

    /// Create a gateway-backed pane from a PtyConfig.
    ///
    /// Instead of spawning a PTY, extracts the command/args/env from the config
    /// and stores them for later gateway session spawning. The pane starts with
    /// `PaneBackend::None` and receives output via `append_output()`.
    fn gateway_pane_from_config(
        name: &str,
        kind: PaneKind,
        config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        validate_codex_gateway_bootstrap(&config, brehon_root)?;
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        let cwd = config
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let spawn_config = GatewaySpawnConfig {
            command: Some(config.command),
            args: config.args,
            env: config.env,
            cwd,
            protocol: gateway_protocol_for(&cli_type),
            tool_prefix: None,
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: None,
            sidecar_socket_path: None,
            sidecar_ready_path: None,
            sidecar_connect_timeout_ms: None,
        };
        Self::gateway_pane_from_spawn_config(
            name,
            kind,
            spawn_config,
            session_id,
            rows,
            cols,
            cli_type,
            brehon_root,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn gateway_pane_from_spawn_config(
        name: &str,
        kind: PaneKind,
        spawn_config: GatewaySpawnConfig,
        session_id: Option<String>,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let mut pane =
            Self::new_with_backend_cli(name, name, kind, PaneBackend::None, rows, cols, cli_type)?;
        // Gateway-backed panes start idle. Unlike Claude-Code PTY panes, there is no splash-screen
        // period during which we want to suppress delivery; the ACP session either is not yet
        // spawned (in which case delivery is gated by gateway-session readiness, not by this flag)
        // or is spawned and ready to accept a prompt. Leaving this true relied on every ACP agent
        // emitting OperationStarted/OperationCompleted pairs, which Kimi (and likely others) does
        // not. The flag is re-set to true by ActivityEvent handlers when tools/operations begin.
        pane.set_tool_executing(false);
        pane.set_agent_session_id(session_id);
        pane.set_configured_agent_type(
            config_env_value(&spawn_config.env, "BREHON_AGENT_TYPE").as_deref(),
        );
        pane.set_notify_socket_path(brehon_root, name);
        pane.gateway_spawn_config = Some(spawn_config);
        Ok(pane)
    }

    #[allow(clippy::too_many_arguments)]
    fn acp_sidecar_supervisor_pane_from_config(
        name: &str,
        mut config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        configured_agent_type: Option<&str>,
        adapter_name: &str,
        materialization: AgentPaneMaterialization,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let (socket_path, ready_path) = acp_sidecar_contract_paths(&config, brehon_root, name)?;
        set_config_env_value(&mut config.env, "BREHON_NATIVE_AGENT_SOCKET", &socket_path);
        set_config_env_value(
            &mut config.env,
            "BREHON_NATIVE_AGENT_READY_FILE",
            &ready_path,
        );
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        let cwd = config
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let spawn_config = GatewaySpawnConfig {
            command: None,
            args: Vec::new(),
            env: config.env.clone(),
            cwd,
            protocol: GatewayProtocol::AcpUnixSocket,
            tool_prefix: None,
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: None,
            sidecar_socket_path: Some(socket_path),
            sidecar_ready_path: Some(ready_path),
            sidecar_connect_timeout_ms: Some(ACP_SIDECAR_CONNECT_TIMEOUT_MS),
        };
        let mut pane = Self::pty_pane_from_config(
            name,
            PaneKind::Supervisor,
            config,
            rows,
            cols,
            cli_type,
            configured_agent_type,
            adapter_name,
            materialization,
            brehon_root,
        )?;
        pane.set_agent_session_id(session_id);
        pane.gateway_spawn_config = Some(spawn_config);
        Ok(pane)
    }

    /// Create a worker pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn worker(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
    ) -> Result<Self> {
        Self::worker_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            supervisor_name,
            adapter,
            model,
            server_url,
            rows,
            cols,
            teams,
            reasoning_effort,
            None,
            &[],
        )
    }

    /// Create a worker pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn worker_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
        configured_agent_type: Option<&str>,
        launcher_env: &[(String, String)],
    ) -> Result<Self> {
        Self::worker_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            supervisor_name,
            adapter,
            model,
            server_url,
            rows,
            cols,
            teams,
            reasoning_effort,
            configured_agent_type,
            launcher_env,
            AgentPaneMaterialization::Spawn,
        )
    }

    /// Create a worker pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn worker_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
        configured_agent_type: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        let worker_cli_str = Some(adapter.name());
        let adapter_owned = adapter.clone();

        if let AgentAdapter::Custom(custom) = adapter {
            match adapter.capabilities().preferred_control_plane {
                crate::harness::HarnessControlPlane::Acp => {
                    let command = custom.command.as_deref().ok_or_else(|| {
                        Error::pty(format!(
                            "Custom ACP agent '{}' is missing a launch command",
                            adapter.name()
                        ))
                    })?;
                    let mut config = if is_custom_codex_app_server(custom) {
                        PtyConfig::custom_codex_acp(
                            name,
                            "worker",
                            cwd,
                            configured_agent_type.or(Some(adapter.name())),
                            brehon_root,
                            Some(supervisor_name),
                            worker_cli_str,
                            model,
                            &custom.args,
                        )
                    } else {
                        PtyConfig::custom_acp(
                            name,
                            "worker",
                            command,
                            &custom.args,
                            configured_agent_type.or(Some(adapter.name())),
                            cwd,
                            brehon_root,
                            Some(supervisor_name),
                            worker_cli_str,
                        )
                    };
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::gateway_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::OpenAiCompatible => {
                    let mut env = build_gateway_metadata_env(
                        name,
                        "worker",
                        session_name,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd.as_path(),
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                    );
                    merge_launcher_env(&mut env, launcher_env);
                    apply_runtime_model_metadata(&mut env, model, reasoning_effort);
                    let session_id = config_env_value(&env, "BREHON_SESSION_ID");
                    let spawn_config = GatewaySpawnConfig {
                        command: None,
                        args: Vec::new(),
                        env,
                        cwd: cwd.to_string_lossy().to_string(),
                        protocol: gateway_protocol_for(adapter),
                        tool_prefix: Some(adapter.capabilities().tool_prefix.as_ref().to_string()),
                        base_url: custom.base_url.clone(),
                        api_key_env: custom.api_key_env.clone(),
                        headers: custom.headers.clone(),
                        model: model.map(str::to_string),
                        sidecar_socket_path: None,
                        sidecar_ready_path: None,
                        sidecar_connect_timeout_ms: None,
                    };
                    return Self::gateway_pane_from_spawn_config(
                        name,
                        PaneKind::Worker,
                        spawn_config,
                        session_id,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                _ => {
                    return Err(Error::pty(format!(
                        "Custom agent '{}' is not yet supported for worker spawn unless it is gateway-backed",
                        adapter.name()
                    )));
                }
            }
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    "worker",
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            // ACP-capable agents: spawned via AgentGateway with piped stdio, not PTY.
            // Build PtyConfig for command/args/env, then create a gateway pane.
            SupervisorCli::Codex => {
                let _ = server_url;
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::codex(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::codex_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::gemini(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        teams,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::gemini_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::kimi(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::kimi_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::opencode(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let server_port = allocate_loopback_port()?;
                let mut config = PtyConfig::opencode_headless_server(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    server_port,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config = PtyConfig::junie(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config = PtyConfig::agy(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::copilot(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::copilot_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
        }
    }

    /// Create a reviewer pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn reviewer(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
    ) -> Result<Self> {
        Self::reviewer_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            None,
            None,
            &[],
        )
    }

    /// Create a reviewer pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn reviewer_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
    ) -> Result<Self> {
        Self::reviewer_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
        )
    }

    /// Create an advisor pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn advisor_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
    ) -> Result<Self> {
        Self::advisor_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
        )
    }

    /// Create a reviewer pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reviewer_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        Self::role_agent_with_agent_type_materialized(
            "reviewer",
            PaneKind::Reviewer,
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            materialization,
        )
    }

    /// Create an advisor pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advisor_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        Self::role_agent_with_agent_type_materialized(
            "advisor",
            PaneKind::Advisor,
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            materialization,
        )
    }

    /// Create a non-worker role agent pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    fn role_agent_with_agent_type_materialized(
        role: &str,
        pane_kind: PaneKind,
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        let adapter_owned = adapter.clone();

        if let AgentAdapter::Custom(custom) = adapter {
            match adapter.capabilities().preferred_control_plane {
                crate::harness::HarnessControlPlane::Acp => {
                    let command = custom.command.as_deref().ok_or_else(|| {
                        Error::pty(format!(
                            "Custom ACP agent '{}' is missing a launch command",
                            adapter.name()
                        ))
                    })?;
                    let mut config = if is_custom_codex_app_server(custom) {
                        PtyConfig::custom_codex_acp(
                            name,
                            role,
                            cwd,
                            configured_agent_type.or(Some(adapter.name())),
                            brehon_root,
                            None,
                            None,
                            model,
                            &custom.args,
                        )
                    } else {
                        PtyConfig::custom_acp(
                            name,
                            role,
                            command,
                            &custom.args,
                            configured_agent_type.or(Some(adapter.name())),
                            cwd,
                            brehon_root,
                            None,
                            None,
                        )
                    };
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::gateway_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::OpenAiCompatible => {
                    let mut env = build_gateway_metadata_env(
                        name,
                        role,
                        session_name,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd.as_path(),
                        brehon_root,
                        None,
                        None,
                    );
                    merge_launcher_env(&mut env, launcher_env);
                    apply_runtime_model_metadata(&mut env, model, reasoning_effort);
                    let session_id = config_env_value(&env, "BREHON_SESSION_ID");
                    let spawn_config = GatewaySpawnConfig {
                        command: None,
                        args: Vec::new(),
                        env,
                        cwd: cwd.to_string_lossy().to_string(),
                        protocol: gateway_protocol_for(adapter),
                        tool_prefix: Some(adapter.capabilities().tool_prefix.as_ref().to_string()),
                        base_url: custom.base_url.clone(),
                        api_key_env: custom.api_key_env.clone(),
                        headers: custom.headers.clone(),
                        model: model.map(str::to_string),
                        sidecar_socket_path: None,
                        sidecar_ready_path: None,
                        sidecar_connect_timeout_ms: None,
                    };
                    return Self::gateway_pane_from_spawn_config(
                        name,
                        pane_kind.clone(),
                        spawn_config,
                        session_id,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                _ => {
                    return Err(Error::pty(format!(
                        "Custom agent '{}' is not yet supported for {role} spawn unless it is gateway-backed",
                        adapter.name()
                    )));
                }
            }
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    role,
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    teams,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            // ACP-capable role agents: gateway panes (piped stdio, no PTY)
            SupervisorCli::Codex => {
                let _ = server_url;
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::codex(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::codex_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::gemini(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        teams,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::gemini_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::kimi(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::kimi_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::opencode(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let server_port = allocate_loopback_port()?;
                let mut config = PtyConfig::opencode_headless_server(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    server_port,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config =
                    PtyConfig::junie(name, role, cwd, brehon_root, None, None, model, teams);
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config =
                    PtyConfig::agy(name, role, cwd, brehon_root, None, None, model, teams);
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                if materialization == AgentPaneMaterialization::PlanOnly {
                    let mut config = PtyConfig::copilot(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::copilot_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
        }
    }

    /// Create a supervisor pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn supervisor(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
    ) -> Result<Self> {
        Self::supervisor_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            rows,
            cols,
            adapter,
            worker_adapter,
            worker_names,
            model,
            server_url,
            teams,
            worker_cli_map,
            None,
            &HashMap::new(),
            None,
            &[],
        )
    }

    /// Create a supervisor pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn supervisor_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
        configured_agent_type: Option<&str>,
        worker_agent_type_map: &HashMap<String, String>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
    ) -> Result<Self> {
        Self::supervisor_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            worker_adapter,
            worker_names,
            model,
            server_url,
            teams,
            worker_cli_map,
            configured_agent_type,
            worker_agent_type_map,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
        )
    }

    /// Create a supervisor pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn supervisor_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
        configured_agent_type: Option<&str>,
        worker_agent_type_map: &HashMap<String, String>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        let worker_cli_str = worker_adapter.name();
        let worker_names_csv = if worker_names.is_empty() {
            None
        } else {
            Some(worker_names.join(","))
        };

        // Build BREHON_FACTORY_WORKER_POOL JSON: {"codex-1":"codex","gemini-1":"gemini"}
        let worker_pool_json = if !worker_cli_map.is_empty() {
            let map: std::collections::HashMap<&str, &str> = worker_cli_map
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str(),
                        worker_agent_type_map
                            .get(k)
                            .map(String::as_str)
                            .unwrap_or_else(|| v.name()),
                    )
                })
                .collect();
            serde_json::to_string(&map).ok()
        } else {
            None
        };

        let adapter_owned = adapter.clone();
        if let AgentAdapter::Custom(custom) = adapter {
            if let Some(reason) = custom_supervisor_requires_pty_error(adapter, custom) {
                return Err(Error::pty(reason));
            }
            let command = custom
                .command
                .as_deref()
                .expect("custom supervisor PTY contract checked command presence");
            let mut config = PtyConfig::custom_pty(
                name,
                "supervisor",
                command,
                &custom.args,
                configured_agent_type.or(Some(adapter.name())),
                cwd,
                brehon_root,
                None,
                Some(worker_cli_str),
            );
            apply_runtime_session_name(&mut config.env, session_name);
            apply_configured_agent_type(&mut config, configured_agent_type);
            merge_launcher_env(&mut config.env, launcher_env);
            apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
            Self::push_supervisor_env(
                &mut config.env,
                adapter,
                &worker_names_csv,
                &worker_pool_json,
            );
            if adapter.capabilities().preferred_control_plane
                == crate::harness::HarnessControlPlane::AcpSidecar
            {
                return Self::acp_sidecar_supervisor_pane_from_config(
                    name,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                );
            }
            return Self::pty_pane_from_config(
                name,
                PaneKind::Supervisor,
                config,
                rows,
                cols,
                adapter_owned,
                configured_agent_type,
                adapter.name(),
                materialization,
                brehon_root,
            );
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    "supervisor",
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Codex => {
                let _ = server_url;
                let mut config = PtyConfig::codex(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                let _ = server_url;
                let mut config = PtyConfig::gemini(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                let mut config = PtyConfig::kimi(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                let mut config = PtyConfig::opencode(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config = PtyConfig::junie(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config = PtyConfig::agy(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                let mut config = PtyConfig::copilot(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
        }
    }

    fn push_supervisor_env(
        env: &mut Vec<(String, String)>,
        adapter: &AgentAdapter,
        worker_names_csv: &Option<String>,
        worker_pool_json: &Option<String>,
    ) {
        env.push((
            "BREHON_FACTORY_SUPERVISOR_CLI".to_string(),
            adapter.name().to_string(),
        ));
        if let Some(csv) = worker_names_csv {
            env.push(("BREHON_FACTORY_WORKER_NAMES".to_string(), csv.clone()));
        }
        if let Some(pool_json) = worker_pool_json {
            env.push(("BREHON_FACTORY_WORKER_POOL".to_string(), pool_json.clone()));
        }
    }

    pub(crate) fn set_notify_socket_path(
        &mut self,
        brehon_root: Option<&PathBuf>,
        agent_name: &str,
    ) {
        self.notify_socket_path =
            brehon_root.map(|root| root.join(format!("notify-{agent_name}.sock")));
    }

    #[cfg(test)]
    pub(crate) fn set_notify_socket_path_for_test(&mut self, path: PathBuf) {
        self.notify_socket_path = Some(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_runtime_model_metadata, build_gateway_metadata_env, merge_launcher_env};
    use std::path::{Path, PathBuf};

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find_map(|(env_key, value)| (env_key == key).then_some(value.as_str()))
    }

    #[test]
    fn merge_launcher_env_keeps_brehon_contract_keys() {
        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), "worker-1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "true".to_string(),
            ),
        ];

        merge_launcher_env(
            &mut env,
            &[
                ("BREHON_AGENT_NAME".to_string(), "override".to_string()),
                (
                    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                    "false".to_string(),
                ),
                (
                    "BREHON_ROLE_SYSTEM_PROMPT".to_string(),
                    "Review for correctness.".to_string(),
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://localhost:11434".to_string(),
                ),
            ],
        );

        assert!(
            env.iter()
                .any(|(key, value)| key == "BREHON_AGENT_NAME" && value == "worker-1")
        );
        assert!(env.iter().any(|(key, value)| {
            key == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS" && value == "true"
        }));
        assert!(env.iter().any(|(key, value)| {
            key == "BREHON_ROLE_SYSTEM_PROMPT" && value == "Review for correctness."
        }));
        assert!(env.iter().any(|(key, value)| {
            key == "ANTHROPIC_BASE_URL" && value == "http://localhost:11434"
        }));
    }

    #[test]
    fn apply_runtime_model_metadata_sets_model_and_reasoning() {
        let mut env = Vec::new();

        apply_runtime_model_metadata(&mut env, Some("gpt-5.4"), Some("xhigh"));

        assert!(
            env.iter()
                .any(|(key, value)| key == "BREHON_AGENT_MODEL" && value == "gpt-5.4")
        );
        assert!(
            env.iter()
                .any(|(key, value)| { key == "BREHON_REASONING_EFFORT" && value == "xhigh" })
        );
    }

    #[test]
    fn gateway_metadata_env_keeps_project_and_workspace_roots_distinct() {
        let env = build_gateway_metadata_env(
            "worker-1",
            "worker",
            Some("brehon-1"),
            Some("native-agent"),
            Path::new("/repo/.brehon/worktrees/runs/brehon-1/worker-1"),
            Some(&PathBuf::from("/repo/.brehon")),
            Some("supervisor"),
            Some("native-agent"),
        );

        assert_eq!(env_value(&env, "BREHON_PROJECT_ROOT"), Some("/repo"));
        assert_eq!(
            env_value(&env, "BREHON_WORKSPACE_ROOT"),
            Some("/repo/.brehon/worktrees/runs/brehon-1/worker-1")
        );
    }
}
