//! AcpGateway implementation of AgentGateway trait.
//!
//! The main entry point for spawning and managing ACP sessions.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, RwLock};
use tracing::info;

use brehon_ports::{AgentGateway, PortError};
use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId,
};

use super::direct_tools::DirectToolBridge;
use super::lifecycle::SessionConfig;
use super::session::AcpSession;
use super::updates::SessionEvent;
use brehon_adapter_agy::{AgyAdapter, AgyConfig};
use brehon_adapter_codex::codex::CodexWsSession;
use brehon_adapter_copilot::copilot::{CopilotAdapter, CopilotConfig};
use brehon_adapter_gemini::gemini::{GeminiAdapter, GeminiConfig};
use brehon_adapter_junie::{JunieAdapter, JunieConfig};
use brehon_adapter_kimi::{KimiAdapter, KimiConfig};
use brehon_adapter_openai::{OpenAiCompatibleAdapter, OpenAiCompatibleConfig};
use brehon_adapter_opencode::OpenCodeServerSession;
use brehon_adapter_sdk::{AdapterErrorKind, AdapterEvent, AgentAdapter, SupervisorCli};

type SessionsById = Arc<RwLock<HashMap<SessionId, Arc<dyn AgentAdapter>>>>;

/// Transport protocol used to communicate with an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayProtocol {
    /// Standard ACP JSON-RPC over stdio.
    AcpStdio,
    /// ACP JSON-RPC over a Unix-domain sidecar socket owned by another process.
    AcpUnixSocket,
    /// Gemini ACP variant over stdio.
    GeminiAcpStdio,
    /// Copilot ACP variant over stdio.
    CopilotAcpStdio,
    /// Kimi ACP variant over stdio.
    KimiAcpStdio,
    /// Codex app-server communication over WebSocket.
    CodexAppServerWs,
    /// OpenCode server communication over HTTP.
    OpenCodeServer,
    /// Direct OpenAI-compatible HTTP API using chat completions.
    OpenAiCompatibleChat,
    /// Junie task-based CLI over stdio.
    JunieStdio,
    /// Agy task-based CLI over stdio.
    AgyStdio,
}

/// Map a gateway protocol to the corresponding SupervisorCli variant.
fn protocol_to_supervisor_cli(protocol: GatewayProtocol) -> Option<SupervisorCli> {
    match protocol {
        GatewayProtocol::AcpStdio => None,
        GatewayProtocol::AcpUnixSocket => None,
        GatewayProtocol::GeminiAcpStdio => Some(SupervisorCli::Gemini),
        GatewayProtocol::CopilotAcpStdio => Some(SupervisorCli::Copilot),
        GatewayProtocol::KimiAcpStdio => Some(SupervisorCli::Kimi),
        GatewayProtocol::CodexAppServerWs => Some(SupervisorCli::Codex),
        GatewayProtocol::OpenCodeServer => Some(SupervisorCli::OpenCode),
        GatewayProtocol::OpenAiCompatibleChat => None,
        GatewayProtocol::JunieStdio => Some(SupervisorCli::Junie),
        GatewayProtocol::AgyStdio => Some(SupervisorCli::Agy),
    }
}

/// Configuration for launching an agent subprocess.
#[derive(Clone)]
pub struct AgentLaunchConfig {
    /// Executable path or command name for subprocess-backed sessions.
    pub command: Option<String>,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Additional environment variables passed to the subprocess.
    pub env: Vec<(String, String)>,
    /// Transport protocol to use for this agent.
    pub protocol: GatewayProtocol,
    /// Tool-name prefix exposed for Brehon coordination tools.
    pub tool_prefix: Option<String>,
    /// Optional direct tool bridge for OpenAI-compatible sessions.
    pub tool_bridge: Option<Arc<dyn DirectToolBridge>>,
    /// Base URL for direct OpenAI-compatible sessions.
    pub base_url: Option<String>,
    /// Environment variable containing the API key for direct sessions.
    pub api_key_env: Option<String>,
    /// Extra static headers for direct sessions.
    pub headers: Vec<(String, String)>,
    /// Default model for this session.
    pub model: Option<String>,
    /// Unix-domain socket path for sidecar ACP sessions.
    pub sidecar_socket_path: Option<String>,
    /// Optional readiness file written by the sidecar owner.
    pub sidecar_ready_path: Option<String>,
    /// Maximum time to wait for the sidecar socket to accept connections.
    pub sidecar_connect_timeout_ms: Option<u64>,
}

/// Primary [`AgentGateway`] implementation that manages ACP agent sessions.
///
/// Supports multiple transport protocols and multiplexes sessions by agent ID.
#[derive(Clone)]
pub struct AcpGateway {
    sessions: SessionsById,
    default_session_config: SessionConfig,
    agent_commands: HashMap<String, AgentLaunchConfig>,
    agent_event_channels: HashMap<String, mpsc::Sender<SessionEvent>>,
    /// Per-CLI adapter registry. Maps each SupervisorCli to its most recently
    /// spawned session. If multiple sessions of the same CLI are spawned, the
    /// latest one wins. Used for future CLI-to-adapter dispatch.
    adapters: Arc<RwLock<HashMap<SupervisorCli, Arc<dyn AgentAdapter>>>>,
}

impl AcpGateway {
    /// Creates a new gateway with no registered agents.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            default_session_config: SessionConfig::default(),
            agent_commands: HashMap::new(),
            agent_event_channels: HashMap::new(),
            adapters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Sets the default session configuration applied to all newly spawned sessions.
    pub fn with_session_config(mut self, config: SessionConfig) -> Self {
        self.default_session_config = config;
        self
    }

    /// Bulk-registers agents from a map of `agent_id -> (command, args)` pairs.
    ///
    /// All entries default to the `AcpStdio` protocol with an empty environment.
    pub fn with_agent_commands(mut self, commands: HashMap<String, (String, Vec<String>)>) -> Self {
        // Convert 2-tuple to launch config with empty env using the default
        // stdio ACP adapter for backward compatibility.
        self.agent_commands = commands
            .into_iter()
            .map(|(k, (cmd, args))| {
                (
                    k,
                    AgentLaunchConfig {
                        command: Some(cmd),
                        args,
                        env: vec![],
                        protocol: GatewayProtocol::AcpStdio,
                        tool_prefix: None,
                        tool_bridge: None,
                        base_url: None,
                        api_key_env: None,
                        headers: vec![],
                        model: None,
                        sidecar_socket_path: None,
                        sidecar_ready_path: None,
                        sidecar_connect_timeout_ms: None,
                    },
                )
            })
            .collect();
        self
    }

    /// Registers an agent using the default `AcpStdio` protocol.
    pub fn register_agent(
        &mut self,
        agent_id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
    ) {
        self.register_agent_launch(
            agent_id,
            AgentLaunchConfig {
                command: Some(command.into()),
                args,
                env: vec![],
                protocol: GatewayProtocol::AcpStdio,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );
    }

    /// Registers an agent with extra environment variables using the default `AcpStdio` protocol.
    pub fn register_agent_with_env(
        &mut self,
        agent_id: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        self.register_agent_launch(
            agent_id,
            AgentLaunchConfig {
                command: Some(command.into()),
                args,
                env,
                protocol: GatewayProtocol::AcpStdio,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );
    }

    /// Registers an agent with a full [`AgentLaunchConfig`], allowing custom protocol selection.
    pub fn register_agent_launch(
        &mut self,
        agent_id: impl Into<String>,
        config: AgentLaunchConfig,
    ) {
        self.agent_commands.insert(agent_id.into(), config);
    }

    /// Registers a channel to receive [`SessionEvent`]s for the given agent.
    pub fn register_agent_event_channel(
        &mut self,
        agent_id: impl Into<String>,
        tx: mpsc::Sender<SessionEvent>,
    ) {
        self.agent_event_channels.insert(agent_id.into(), tx);
    }

    /// Blocks until a prompt response is available or the timeout expires.
    pub async fn wait_for_response(
        &self,
        session_id: &SessionId,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<super::acp_types::PromptResult, PortError> {
        let sess = self.get_session(session_id).await?;
        let result = sess
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to wait for prompt response: {}", e)))?;
        Ok(super::acp_types::PromptResult {
            response: result.response,
            tokens_used: result.tokens_used,
            stop_reason: result.stop_reason,
        })
    }

    async fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<dyn AgentAdapter>, PortError> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| PortError::Agent(format!("Session {} not found", session_id)))
    }

    /// Aggregate stability counters across all active gateway sessions.
    ///
    /// Each transport contributes the queue state it actually maintains today:
    /// ACP and Gemini report JSON-RPC pending/prompt caches, Codex reports
    /// websocket pending requests, and OpenCode/OpenAI-compatible sessions
    /// report in-flight prompt work as pending requests.
    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        let sessions: Vec<_> = {
            let sessions = self.sessions.read().await;
            sessions.values().cloned().collect()
        };
        let mut aggregate = brehon_types::StabilityCounters::default();
        for session in sessions {
            let c = session.stability_counters().await;
            aggregate.merge(&c);
        }
        aggregate
    }

    #[cfg(test)]
    pub(crate) async fn insert_acp_session_for_test(&self, session: AcpSession) -> SessionId {
        let session_id = session.session_id().clone();
        self.sessions
            .write()
            .await
            .insert(session_id.clone(), Arc::new(session));
        session_id
    }
}

async fn connect_unix_socket_with_retry(
    spec: SessionSpec,
    socket_path: &str,
    ready_path: Option<&str>,
    env: &[(String, String)],
    event_tx: Option<mpsc::Sender<SessionEvent>>,
    session_config: SessionConfig,
    timeout: Duration,
) -> Result<AcpSession, super::session::SessionError> {
    let started_at = Instant::now();
    let mut last_error = None;

    loop {
        let ready = ready_path
            .map(|path| std::path::Path::new(path).exists())
            .unwrap_or(true);

        if ready {
            match AcpSession::connect_unix_socket_with_channel(
                spec.clone(),
                socket_path,
                env,
                event_tx.clone(),
                session_config.clone(),
            )
            .await
            {
                Ok(session) => return Ok(session),
                Err(err) => last_error = Some(err),
            }
        }

        if started_at.elapsed() >= timeout {
            return Err(last_error.unwrap_or(super::session::SessionError::Timeout));
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[async_trait]
impl AgentGateway for AcpGateway {
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId, PortError> {
        let agent_id = spec.agent_id.as_str().to_string();

        let launch = self
            .agent_commands
            .get(&agent_id)
            .ok_or_else(|| PortError::Agent(format!("Unknown agent: {}", agent_id)))?;

        let session: Arc<dyn AgentAdapter> = match launch.protocol {
            GatewayProtocol::AcpStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let session = AcpSession::spawn_with_env_and_channel(
                    spec.clone(),
                    command,
                    &launch.args,
                    &launch.env,
                    self.agent_event_channels.get(&agent_id).cloned(),
                    self.default_session_config.clone(),
                )
                .await
                .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                Arc::new(session)
            }
            GatewayProtocol::AcpUnixSocket => {
                let socket_path = launch.sidecar_socket_path.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a sidecar ACP socket path",
                        agent_id
                    ))
                })?;
                let session = connect_unix_socket_with_retry(
                    spec.clone(),
                    socket_path,
                    launch.sidecar_ready_path.as_deref(),
                    &launch.env,
                    self.agent_event_channels.get(&agent_id).cloned(),
                    self.default_session_config.clone(),
                    Duration::from_millis(launch.sidecar_connect_timeout_ms.unwrap_or(5_000)),
                )
                .await
                .map_err(|e| PortError::Agent(format!("Failed to connect sidecar ACP: {}", e)))?;

                Arc::new(session)
            }
            GatewayProtocol::GeminiAcpStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let adapter = GeminiAdapter::new(GeminiConfig {
                    command: command.to_string(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = adapter.events();
                    let event_tx = event_tx.clone();
                    let session_id = adapter.session_id().await;
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(adapter)
            }
            GatewayProtocol::CopilotAcpStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let adapter = CopilotAdapter::new(CopilotConfig {
                    command: command.to_string(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = adapter.events();
                    let event_tx = event_tx.clone();
                    let session_id = adapter.session_id().await;
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(adapter)
            }
            GatewayProtocol::KimiAcpStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let adapter = KimiAdapter::new(KimiConfig {
                    command: command.to_string(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = adapter.events();
                    let event_tx = event_tx.clone();
                    let session_id = adapter.session_id().await;
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(adapter)
            }
            GatewayProtocol::CodexAppServerWs => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let session = CodexWsSession::spawn_with_env(
                    spec.clone(),
                    command,
                    &launch.args,
                    &launch.env,
                    self.agent_event_channels.get(&agent_id).cloned(),
                )
                .await
                .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                Arc::new(session)
            }
            GatewayProtocol::OpenCodeServer => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let session = OpenCodeServerSession::spawn_with_env(
                    spec.clone(),
                    command,
                    &launch.args,
                    &launch.env,
                )
                .await
                .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = session.events();
                    let event_tx = event_tx.clone();
                    let session_id = session.session_id().clone();
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(session)
            }
            GatewayProtocol::OpenAiCompatibleChat => {
                let adapter = OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
                    base_url: launch.base_url.clone(),
                    api_key_env: launch.api_key_env.clone(),
                    extra_headers: launch.headers.clone(),
                    model: launch.model.clone(),
                    tool_prefix: launch.tool_prefix.clone(),
                    tool_bridge: launch.tool_bridge.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                let session_id = adapter.session_id().await;
                if let Some(global_tx) = self.agent_event_channels.get(&agent_id).cloned() {
                    let event_rx = adapter.events();
                    tokio::spawn(forward_adapter_events(event_rx, global_tx, session_id));
                }

                Arc::new(adapter)
            }
            GatewayProtocol::JunieStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let adapter = JunieAdapter::new(JunieConfig {
                    command: command.to_string(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = adapter.events();
                    let event_tx = event_tx.clone();
                    let session_id = adapter.session_id().await;
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(adapter)
            }
            GatewayProtocol::AgyStdio => {
                let command = launch.command.as_deref().ok_or_else(|| {
                    PortError::Agent(format!(
                        "Agent {} is missing a subprocess command",
                        agent_id
                    ))
                })?;
                let adapter = AgyAdapter::new(AgyConfig {
                    command: command.to_string(),
                    args: launch.args.clone(),
                    env: launch.env.clone(),
                });
                adapter
                    .spawn(spec.clone())
                    .await
                    .map_err(|e| PortError::Agent(format!("Failed to spawn session: {}", e)))?;

                if let Some(event_tx) = self.agent_event_channels.get(&agent_id) {
                    let event_rx = adapter.events();
                    let event_tx = event_tx.clone();
                    let session_id = adapter.session_id().await;
                    tokio::spawn(forward_adapter_events(event_rx, event_tx, session_id));
                }

                Arc::new(adapter)
            }
        };

        let session_id = session.session_id().await;

        self.sessions
            .write()
            .await
            .insert(session_id.clone(), Arc::clone(&session));

        // Register adapter in the CLI registry
        let cli = protocol_to_supervisor_cli(launch.protocol);
        if let Some(cli) = cli {
            self.adapters.write().await.insert(cli, session);
        }

        info!(session_id = %session_id, agent_id = %agent_id, "Session spawned");
        Ok(session_id)
    }

    async fn set_config(
        &self,
        session: &SessionId,
        option: &str,
        value: &str,
    ) -> Result<(), PortError> {
        let sess = self.get_session(session).await?;
        sess.set_config(option, value)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to set config: {}", e)))
    }

    async fn send_prompt(
        &self,
        session: &SessionId,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, PortError> {
        let sess = self.get_session(session).await?;
        sess.send_prompt(prompt)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to send prompt: {}", e)))
    }

    async fn cancel_prompt(&self, session: &SessionId, prompt: &PromptId) -> Result<(), PortError> {
        let sess = self.get_session(session).await?;
        sess.cancel_prompt(prompt)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to cancel prompt: {}", e)))
    }

    async fn attach_terminal(&self, session: &SessionId) -> Result<Option<TerminalId>, PortError> {
        let sess = self.get_session(session).await?;
        sess.attach_terminal(80, 24)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to attach terminal: {}", e)))
    }

    async fn send_terminal_input(
        &self,
        terminal: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), PortError> {
        let sessions: Vec<_> = {
            let sessions = self.sessions.read().await;
            sessions.values().cloned().collect()
        };

        for session in sessions {
            match session.send_terminal_input(terminal, input.clone()).await {
                Ok(()) => return Ok(()),
                Err(ref e) if e.kind == AdapterErrorKind::UnsupportedOperation => continue,
                Err(e) => {
                    return Err(PortError::Agent(format!(
                        "Failed to send terminal input: {}",
                        e
                    )))
                }
            }
        }

        Err(PortError::Agent(format!("Terminal {} not found", terminal)))
    }

    async fn resolve_permission(
        &self,
        session: &SessionId,
        permission_id: &str,
        approved: bool,
    ) -> Result<(), PortError> {
        let sess = self.get_session(session).await?;
        sess.resolve_permission(permission_id, approved)
            .await
            .map_err(|e| PortError::Agent(format!("Failed to resolve permission: {}", e)))
    }

    async fn kill_session(&self, session: &SessionId) -> Result<(), PortError> {
        let sess = self.get_session(session).await?;
        sess.terminate()
            .await
            .map_err(|e| PortError::Agent(format!("Failed to kill session: {}", e)))?;

        self.sessions.write().await.remove(session);

        info!(session_id = %session, "Session killed");
        Ok(())
    }

    async fn health_check(&self, session: &SessionId) -> Result<HealthStatus, PortError> {
        let sess = self.get_session(session).await?;
        sess.health_check()
            .await
            .map_err(|e| PortError::Agent(format!("Health check failed: {}", e)))
    }

    async fn capabilities(&self, session: &SessionId) -> Result<AgentCapabilities, PortError> {
        let sess = self.get_session(session).await?;
        sess.capabilities()
            .await
            .map_err(|e| PortError::Agent(format!("Failed to get capabilities: {}", e)))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, PortError> {
        let sessions: Vec<_> = {
            let sessions = self.sessions.read().await;
            sessions.values().cloned().collect()
        };
        let mut result = Vec::with_capacity(sessions.len());

        for session in sessions {
            result.push(session.session_info().await);
        }

        Ok(result)
    }
}

/// Forward [`AdapterEvent`]s from an adapter into the gateway's [`SessionEvent`] channel.
async fn forward_adapter_events(
    mut event_rx: mpsc::Receiver<AdapterEvent>,
    event_tx: mpsc::Sender<SessionEvent>,
    session_id: SessionId,
) {
    while let Some(event) = event_rx.recv().await {
        let se = match event {
            AdapterEvent::Output { text } => SessionEvent::Output {
                session_id: session_id.clone(),
                text,
            },
            AdapterEvent::OperationStarted { operation } => SessionEvent::OperationStarted {
                session_id: session_id.clone(),
                operation,
            },
            AdapterEvent::OperationCompleted { operation, success } => {
                SessionEvent::OperationCompleted {
                    session_id: session_id.clone(),
                    operation,
                    success,
                }
            }
            AdapterEvent::PermissionRequest {
                permission_id,
                action,
                details,
            } => SessionEvent::PermissionRequest {
                session_id: session_id.clone(),
                permission_id,
                action,
                details,
            },
            AdapterEvent::Progress { message, percent } => SessionEvent::Progress {
                session_id: session_id.clone(),
                message,
                percent,
            },
            AdapterEvent::ToolCallStarted {
                tool_id,
                tool_name,
                details,
            } => SessionEvent::ToolCallStarted {
                session_id: session_id.clone(),
                tool_id,
                tool_name,
                details,
            },
            AdapterEvent::ToolCallCompleted {
                tool_id,
                tool_name,
                status,
                details,
            } => SessionEvent::ToolCallCompleted {
                session_id: session_id.clone(),
                tool_id,
                tool_name,
                status,
                details,
            },
            _ => continue,
        };
        if event_tx.send(se).await.is_err() {
            break;
        }
    }
}

impl Default for AcpGateway {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a new [`AcpGateway`] with default settings.
pub fn create_gateway() -> AcpGateway {
    AcpGateway::new()
}

/// Creates a new [`AcpGateway`] with the given session configuration.
pub fn create_gateway_with_config(config: SessionConfig) -> AcpGateway {
    AcpGateway::new().with_session_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use brehon_adapter_sdk::{AdapterError, AdapterKind, AdapterResult, PromptResult};
    use brehon_ports::AgentGateway;
    use chrono::Utc;
    use std::any::Any;
    use std::sync::Mutex;
    use std::time::Duration;

    struct WaitResponseMockAdapter {
        session_id: SessionId,
        wait_calls: Mutex<Vec<(PromptId, u64)>>,
    }

    impl WaitResponseMockAdapter {
        fn new(session_id: SessionId) -> Self {
            Self {
                session_id,
                wait_calls: Mutex::new(Vec::new()),
            }
        }

        fn wait_calls(&self) -> Vec<(PromptId, u64)> {
            self.wait_calls
                .lock()
                .expect("wait_calls mutex poisoned")
                .clone()
        }
    }

    fn mock_capabilities() -> AgentCapabilities {
        AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: brehon_types::ToolCallStreaming::None,
        }
    }

    #[async_trait]
    impl AgentAdapter for WaitResponseMockAdapter {
        async fn spawn(&self, _spec: SessionSpec) -> AdapterResult<SessionId> {
            Ok(self.session_id.clone())
        }

        async fn send_prompt(&self, _prompt: PromptTurn) -> AdapterResult<PromptHandle> {
            Err(AdapterError::unsupported_operation(
                "send_prompt is not used in this test adapter",
            ))
        }

        async fn wait_for_response(
            &self,
            prompt_id: &PromptId,
            timeout_ms: u64,
        ) -> AdapterResult<PromptResult> {
            self.wait_calls
                .lock()
                .expect("wait_calls mutex poisoned")
                .push((prompt_id.clone(), timeout_ms));
            let mut result = PromptResult::default();
            result.response = Some(format!("mock-response-{}", prompt_id.as_str()));
            result.tokens_used = Some(7);
            result.stop_reason = Some("stop".to_string());
            Ok(result)
        }

        fn events(&self) -> mpsc::Receiver<AdapterEvent> {
            let (_tx, rx) = mpsc::channel(1);
            rx
        }

        async fn terminate(&self) -> AdapterResult<()> {
            Ok(())
        }

        fn kind(&self) -> AdapterKind {
            AdapterKind::Mock
        }

        async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
            Ok(mock_capabilities())
        }

        async fn session_id(&self) -> SessionId {
            self.session_id.clone()
        }

        async fn session_info(&self) -> SessionInfo {
            SessionInfo {
                session_id: self.session_id.clone(),
                agent_id: brehon_types::AgentId::new("mock-agent"),
                role: "worker".to_string(),
                health: HealthStatus::Healthy,
                created_at: Utc::now(),
                capabilities: mock_capabilities(),
            }
        }

        async fn stability_counters(&self) -> brehon_types::StabilityCounters {
            brehon_types::StabilityCounters::default()
        }

        async fn set_config(&self, _option: &str, _value: &str) -> AdapterResult<()> {
            Ok(())
        }

        async fn cancel_prompt(&self, _prompt: &PromptId) -> AdapterResult<()> {
            Ok(())
        }

        async fn health_check(&self) -> AdapterResult<HealthStatus> {
            Ok(HealthStatus::Healthy)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn test_gateway_creation() {
        let gateway = AcpGateway::new();
        assert!(gateway.agent_commands.is_empty());
    }

    #[test]
    fn test_register_agent() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent("test-agent", "echo", vec!["hello".to_string()]);

        assert!(gateway.agent_commands.contains_key("test-agent"));
    }

    #[test]
    fn test_with_agent_commands_defaults_to_stdio_acp() {
        let gateway = AcpGateway::new().with_agent_commands(HashMap::from([(
            "test-agent".to_string(),
            ("echo".to_string(), vec!["hello".to_string()]),
        )]));

        let launch = gateway
            .agent_commands
            .get("test-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::AcpStdio);
        assert!(launch.env.is_empty());
    }

    #[test]
    fn test_register_agent_launch_preserves_protocol() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "codex-agent",
            AgentLaunchConfig {
                command: Some("codex".to_string()),
                args: vec!["app-server".to_string()],
                env: vec![("CODEX_HOME".to_string(), "/tmp/codex-home".to_string())],
                protocol: GatewayProtocol::CodexAppServerWs,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );

        let launch = gateway
            .agent_commands
            .get("codex-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::CodexAppServerWs);
        assert_eq!(launch.command.as_deref(), Some("codex"));
    }

    #[test]
    fn test_register_agent_launch_preserves_gemini_protocol() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "gemini-agent",
            AgentLaunchConfig {
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                env: vec![],
                protocol: GatewayProtocol::GeminiAcpStdio,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );

        let launch = gateway
            .agent_commands
            .get("gemini-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::GeminiAcpStdio);
    }

    #[test]
    fn test_register_agent_launch_preserves_copilot_protocol() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "copilot-agent",
            AgentLaunchConfig {
                command: Some("copilot".to_string()),
                args: vec!["--acp".to_string(), "--stdio".to_string()],
                env: vec![],
                protocol: GatewayProtocol::CopilotAcpStdio,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );

        let launch = gateway
            .agent_commands
            .get("copilot-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::CopilotAcpStdio);
    }

    #[test]
    fn test_register_agent_launch_preserves_opencode_server_protocol() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "opencode-agent",
            AgentLaunchConfig {
                command: Some("opencode".to_string()),
                args: vec![
                    "serve".to_string(),
                    "--port".to_string(),
                    "43123".to_string(),
                ],
                env: vec![(
                    "BREHON_OPENCODE_SERVER_URL".to_string(),
                    "http://127.0.0.1:43123".to_string(),
                )],
                protocol: GatewayProtocol::OpenCodeServer,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );

        let launch = gateway
            .agent_commands
            .get("opencode-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::OpenCodeServer);
    }

    #[test]
    fn test_register_agent_launch_preserves_openai_protocol() {
        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "direct-agent",
            AgentLaunchConfig {
                command: None,
                args: vec![],
                env: vec![],
                protocol: GatewayProtocol::OpenAiCompatibleChat,
                tool_prefix: Some("mcp_brehon_".to_string()),
                tool_bridge: None,
                base_url: Some("https://example.invalid/v1".to_string()),
                api_key_env: Some("TEST_API_KEY".to_string()),
                headers: vec![("x-test".to_string(), "1".to_string())],
                model: Some("gpt-test".to_string()),
                sidecar_socket_path: None,
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: None,
            },
        );

        let launch = gateway
            .agent_commands
            .get("direct-agent")
            .expect("registered launch config");
        assert_eq!(launch.protocol, GatewayProtocol::OpenAiCompatibleChat);
        assert_eq!(
            launch.base_url.as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(launch.model.as_deref(), Some("gpt-test"));
    }

    #[tokio::test]
    async fn test_gateway_spawns_acp_unix_socket_session() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir_in("/tmp").expect("tempdir should be created");
        let socket_path = tmp.path().join("gateway-acp.sock");
        let listener = UnixListener::bind(&socket_path).expect("socket listener should bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("client should connect");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);

            for expected_method in ["initialize", "session/new"] {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("request line should read");
                let request = match crate::protocol::parse_message(line.trim_end())
                    .expect("request should parse")
                {
                    crate::protocol::JsonRpcMessage::Request(request) => request,
                    other => panic!("unexpected message: {other:?}"),
                };
                assert_eq!(request.method, expected_method);

                let result = match expected_method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "content_block_types": ["text"],
                            "session_config_options": ["mode", "model"],
                            "permission_support": true,
                            "terminal_support": false,
                            "tool_call_streaming": "full"
                        }
                    }),
                    "session/new" => serde_json::json!({
                        "sessionId": "gateway-sidecar-session"
                    }),
                    _ => unreachable!(),
                };
                let response = crate::protocol::JsonRpcResponse::success(request.id, result);
                let mut encoded =
                    serde_json::to_string(&response).expect("response should serialize");
                encoded.push('\n');
                write_half
                    .write_all(encoded.as_bytes())
                    .await
                    .expect("response should write");
            }

            let mut eof = String::new();
            let read = reader
                .read_line(&mut eof)
                .await
                .expect("client shutdown should be observable");
            assert_eq!(read, 0);
        });

        let mut gateway = AcpGateway::new();
        gateway.register_agent_launch(
            "sidecar-agent",
            AgentLaunchConfig {
                command: None,
                args: vec![],
                env: vec![],
                protocol: GatewayProtocol::AcpUnixSocket,
                tool_prefix: None,
                tool_bridge: None,
                base_url: None,
                api_key_env: None,
                headers: vec![],
                model: None,
                sidecar_socket_path: Some(socket_path.to_string_lossy().to_string()),
                sidecar_ready_path: None,
                sidecar_connect_timeout_ms: Some(1_000),
            },
        );

        let spec = SessionSpec::new(
            brehon_types::AgentId::new("sidecar-agent"),
            "worker".to_string(),
            std::env::temp_dir().to_string_lossy().to_string(),
        );
        let session_id = gateway
            .spawn(spec)
            .await
            .expect("gateway should spawn sidecar ACP session");

        let capabilities = gateway
            .capabilities(&session_id)
            .await
            .expect("capabilities should be available");
        assert!(capabilities.permission_support);
        assert_eq!(
            capabilities.tool_call_streaming,
            brehon_types::ToolCallStreaming::Full
        );

        gateway
            .kill_session(&session_id)
            .await
            .expect("sidecar session should shut down");
        server.await.expect("server task should finish");
    }

    #[tokio::test]
    async fn test_gateway_wait_for_response_dispatches_through_adapter_trait() {
        let gateway = AcpGateway::new();
        let session_id = SessionId::new("mock-session");
        let adapter = Arc::new(WaitResponseMockAdapter::new(session_id.clone()));

        gateway
            .sessions
            .write()
            .await
            .insert(session_id.clone(), adapter.clone());

        let prompt_id = PromptId::new("prompt-123");
        let result = gateway
            .wait_for_response(&session_id, &prompt_id, 4321)
            .await
            .expect("wait_for_response should dispatch to adapter trait");

        assert_eq!(result.response.as_deref(), Some("mock-response-prompt-123"));
        assert_eq!(result.tokens_used, Some(7));
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
        assert_eq!(adapter.wait_calls(), vec![(prompt_id.clone(), 4321)]);
    }

    #[tokio::test]
    async fn test_gateway_resolve_permission_allows_before_timeout() {
        let gateway = AcpGateway::new();
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let session = super::super::session::AcpSession::test_session_with_channel(
            Some(event_tx),
            Duration::from_secs(1),
            super::super::permissions::PermissionPolicy::default(),
        );
        let session_id = gateway.insert_acp_session_for_test(session).await;
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let permission_id = request.id.clone();

        let response_task = tokio::spawn({
            let gateway = gateway.clone();
            let session_id = session_id.clone();
            async move {
                let session = gateway
                    .get_session(&session_id)
                    .await
                    .expect("session exists");
                let acp_session = session
                    .as_any()
                    .downcast_ref::<AcpSession>()
                    .expect("expected ACP session");
                acp_session.test_response_for_request(request).await
            }
        });

        let event = event_rx
            .recv()
            .await
            .expect("permission event should be emitted");
        match event {
            SessionEvent::PermissionRequest {
                session_id: event_session_id,
                permission_id: event_permission_id,
                ..
            } => {
                assert_eq!(event_session_id, session_id);
                assert_eq!(event_permission_id, permission_id);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        AgentGateway::resolve_permission(&gateway, &session_id, &permission_id, true)
            .await
            .expect("permission resolution should succeed");

        let response = response_task.await.expect("task should complete");
        assert_eq!(
            response.result.expect("response should contain outcome"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once",
                }
            })
        );
    }

    #[tokio::test]
    async fn test_gateway_resolve_permission_denies_before_timeout() {
        let gateway = AcpGateway::new();
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let session = super::super::session::AcpSession::test_session_with_channel(
            Some(event_tx),
            Duration::from_secs(1),
            super::super::permissions::PermissionPolicy::default(),
        );
        let session_id = gateway.insert_acp_session_for_test(session).await;
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let permission_id = request.id.clone();

        let response_task = tokio::spawn({
            let gateway = gateway.clone();
            let session_id = session_id.clone();
            async move {
                let session = gateway
                    .get_session(&session_id)
                    .await
                    .expect("session exists");
                let acp_session = session
                    .as_any()
                    .downcast_ref::<AcpSession>()
                    .expect("expected ACP session");
                acp_session.test_response_for_request(request).await
            }
        });

        let event = event_rx
            .recv()
            .await
            .expect("permission event should be emitted");
        match event {
            SessionEvent::PermissionRequest {
                permission_id: event_permission_id,
                ..
            } => {
                assert_eq!(event_permission_id, permission_id);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        AgentGateway::resolve_permission(&gateway, &session_id, &permission_id, false)
            .await
            .expect("permission resolution should succeed");

        let response = response_task.await.expect("task should complete");
        assert_eq!(
            response.result.expect("response should contain outcome"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );
    }

    #[tokio::test]
    async fn test_gateway_resolve_permission_reports_expired_request() {
        let gateway = AcpGateway::new();
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let session = super::super::session::AcpSession::test_session_with_channel(
            Some(event_tx),
            Duration::from_millis(20),
            super::super::permissions::PermissionPolicy::default(),
        );
        let session_id = gateway.insert_acp_session_for_test(session).await;
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let permission_id = request.id.clone();

        let response_task = tokio::spawn({
            let gateway = gateway.clone();
            let session_id = session_id.clone();
            async move {
                let session = gateway
                    .get_session(&session_id)
                    .await
                    .expect("session exists");
                let acp_session = session
                    .as_any()
                    .downcast_ref::<AcpSession>()
                    .expect("expected ACP session");
                acp_session.test_response_for_request(request).await
            }
        });

        let event = event_rx
            .recv()
            .await
            .expect("permission event should be emitted");
        match event {
            SessionEvent::PermissionRequest {
                permission_id: event_permission_id,
                ..
            } => {
                assert_eq!(event_permission_id, permission_id);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let response = response_task.await.expect("task should complete");
        assert_eq!(
            response.result.expect("response should contain outcome"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );

        let err = AgentGateway::resolve_permission(&gateway, &session_id, &permission_id, true)
            .await
            .expect_err("timed-out permission should not resolve");
        match err {
            PortError::Agent(message) => {
                assert!(message.contains("expired or timed out"));
                assert!(!message.contains("not found"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
