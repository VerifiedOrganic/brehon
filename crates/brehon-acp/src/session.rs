//! ACP Session management.
//!
//! Manages a single ACP connection including transport setup,
//! lifecycle, and EOF detection.

use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use brehon_adapter_sdk::{
    AdapterError, AdapterErrorKind, AdapterEvent, AdapterResult, AgentAdapter, PromptResult,
};
use brehon_ports::PortError;
use brehon_types::{
    AdapterKind, AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId,
    SessionInfo, SessionSpec, TerminalId,
};

use super::acp_types::SessionMetadata;
use super::config::ConfigOption;
use super::lifecycle::SessionConfig;
use super::peer::{AcpPeer, SubprocessAcpPeer, UnixSocketAcpPeer};
use super::permissions::{
    PermissionDecision, PermissionPolicy, PermissionRequest, PermissionResolution,
};
use super::process::AgentProcess;
use super::protocol::JsonRpcMessage;
use super::stability_runtime::{
    brehon_root_from_env, clear_session_snapshot, persist_session_snapshot,
    schedule_clear_session_snapshot, schedule_persist_session_snapshot,
};
use super::terminals::TranscriptBuffer;
use super::updates::SessionEvent;

pub(crate) struct AcpSessionInner {
    pub(crate) session_id: SessionId,
    pub(crate) remote_session_id: String,
    pub(crate) spec: SessionSpec,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) peer: Mutex<Option<Box<dyn AcpPeer>>>,
    pub(crate) pending_requests:
        Mutex<HashMap<String, oneshot::Sender<super::protocol::JsonRpcResponse>>>,
    /// Pending prompt-response oneshot channels, keyed by prompt ID.
    ///
    /// `send_prompt` registers a sender before writing to the agent so the
    /// reader can complete the prompt without polling. Entries are removed on
    /// completion, timeout, send failure, and session shutdown.
    pub(crate) pending_prompt_responses:
        Mutex<HashMap<String, oneshot::Sender<Result<super::acp_types::PromptResult, String>>>>,
    /// Receivers paired with `pending_prompt_responses`, retrieved by
    /// `wait_for_response` using the prompt ID from `send_prompt`.
    pub(crate) prompt_response_receivers:
        Mutex<HashMap<String, oneshot::Receiver<Result<super::acp_types::PromptResult, String>>>>,
    pub(crate) blocked_sends: AtomicUsize,
    pub(crate) prompt_result_tokens_used: AtomicU64,
    pub(crate) usage_update_tokens_used: AtomicU64,
    pub(crate) active_prompt_id: Mutex<Option<String>>,
    active_prompt_token_attribution: Mutex<Option<ActivePromptTokenAttribution>>,
    pub(crate) event_tx: StdMutex<Option<mpsc::Sender<SessionEvent>>>,
    pub(crate) alive: AtomicBool,
    /// When set to `true`, the session reader loop should exit promptly
    /// rather than waiting for the next `recv_line` timeout. Set during
    /// `kill()` so the reader task can be awaited deterministically.
    pub(crate) shutdown: AtomicBool,
    /// Tracked `JoinHandle` for the session reader task spawned in
    /// `spawn_with_env_and_channel`. Ownership enables deterministic
    /// cancellation and await during session shutdown.
    pub(crate) reader_handle: Mutex<Option<JoinHandle<()>>>,
    pub(crate) terminal_manager: super::terminals::TerminalManager,
    pub(crate) transcript_buffer: TranscriptBuffer,
    pub(crate) permission_manager: super::permissions::PermissionManager,
    pub(crate) permission_timeout: Duration,
    pub(crate) config_manager: super::config::ConfigManager,
}

#[derive(Debug, Clone)]
struct ActivePromptTokenAttribution {
    prompt_id: String,
    task_id: String,
    usage_tokens_attributed: u64,
}

/// A live ACP agent session, wrapping an ACP transport peer.
///
/// Provides methods for sending prompts, managing terminals, setting config,
/// and monitoring session health.
pub struct AcpSession {
    inner: Arc<AcpSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if self.inner.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }

        self.inner.alive.store(false, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(err) = shutdown_session_inner(&inner, Duration::from_secs(2)).await {
                        warn!(
                            session_id = %inner.session_id,
                            error = %err,
                            "Failed to clean up dropped ACP session"
                        );
                    }
                });
            }
            Err(err) => {
                warn!(
                    session_id = %self.inner.session_id,
                    error = %err,
                    "Dropped ACP session outside a Tokio runtime; async transport cleanup could not be scheduled"
                );
            }
        }
    }
}

fn parse_new_session_mcp_servers(env: &[(String, String)]) -> Vec<serde_json::Value> {
    env.iter()
        .find_map(|(key, value)| (key == "BREHON_ACP_MCP_SERVERS_JSON").then_some(value))
        .and_then(|value| serde_json::from_str::<Vec<serde_json::Value>>(value).ok())
        .unwrap_or_default()
}

impl AcpSession {
    /// Spawns an agent subprocess and completes the ACP handshake.
    pub async fn spawn(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        session_config: SessionConfig,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_env_and_channel(spec, command, args, &[], None, session_config).await
    }

    /// Spawns an agent subprocess with additional environment variables and completes the handshake.
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        session_config: SessionConfig,
    ) -> Result<Self, SessionError> {
        Self::spawn_with_env_and_channel(spec, command, args, env, None, session_config).await
    }

    /// Spawns an agent subprocess with optional event channel attachment before the reader starts.
    pub async fn spawn_with_env_and_channel(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
        session_config: SessionConfig,
    ) -> Result<Self, SessionError> {
        let process = AgentProcess::spawn_with_env(command, args, &spec.worktree_path, env)
            .await
            .map_err(|e| SessionError::InitFailed(e.to_string()))?;

        Self::from_peer(
            spec,
            Box::new(SubprocessAcpPeer::new(process)),
            env,
            event_tx,
            session_config,
        )
        .await
    }

    /// Connects to an already-running ACP peer over a Unix-domain socket.
    pub async fn connect_unix_socket_with_channel(
        spec: SessionSpec,
        socket_path: impl AsRef<std::path::Path>,
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
        session_config: SessionConfig,
    ) -> Result<Self, SessionError> {
        let peer = UnixSocketAcpPeer::connect(socket_path)
            .await
            .map_err(|e| SessionError::Io(e.to_string()))?;

        Self::from_peer(spec, Box::new(peer), env, event_tx, session_config).await
    }

    async fn from_peer(
        spec: SessionSpec,
        mut peer: Box<dyn AcpPeer>,
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
        session_config: SessionConfig,
    ) -> Result<Self, SessionError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let permission_policy = PermissionPolicy::default();

        let metadata = SessionMetadata {
            role: Some(spec.role.clone()),
            task_id: None,
            agent_id: Some(spec.agent_id.as_str().to_string()),
        };

        // Perform handshake
        let request = super::acp_types::create_initialize_request("", Some(metadata.clone()));

        let line = super::protocol::serialize_request(&request)
            .map_err(|e| SessionError::Protocol(e.message))?;

        peer.send_line(&line)
            .await
            .map_err(|e| SessionError::Io(e.to_string()))?;

        // Wait for response
        let response = wait_for_response(
            peer.as_mut(),
            &request.id,
            session_config.init_timeout,
            permission_policy.timeout_decision,
        )
        .await
        .map_err(SessionError::Protocol)?;

        let capabilities = match response.error {
            Some(err) => return Err(SessionError::InitFailed(err.message)),
            None => {
                let init_result: super::acp_types::InitializeResult = serde_json::from_value(
                    response.result.clone().unwrap_or(serde_json::Value::Null),
                )
                .map_err(|e| SessionError::Protocol(format!("Failed to parse result: {}", e)))?;
                init_result.capabilities.into()
            }
        };

        let new_session_request = super::acp_types::create_new_session_request(
            &spec.worktree_path,
            parse_new_session_mcp_servers(env),
        );
        let line = super::protocol::serialize_request(&new_session_request)
            .map_err(|e| SessionError::Protocol(e.message))?;

        peer.send_line(&line)
            .await
            .map_err(|e| SessionError::Io(e.to_string()))?;

        let new_session_response = wait_for_response(
            peer.as_mut(),
            &new_session_request.id,
            session_config.init_timeout,
            permission_policy.timeout_decision,
        )
        .await
        .map_err(SessionError::Protocol)?;

        let remote_session_id = match new_session_response.error {
            Some(err) => return Err(SessionError::InitFailed(err.message)),
            None => {
                super::acp_types::parse_new_session_result(&new_session_response)
                    .map_err(SessionError::Protocol)?
                    .session_id
            }
        };

        let config_manager = super::config::ConfigManager::new(&capabilities);
        let permission_manager = super::permissions::PermissionManager::new(permission_policy);

        let inner = Arc::new(AcpSessionInner {
            session_id: session_id.clone(),
            remote_session_id,
            spec: spec.clone(),
            capabilities: capabilities.clone(),
            peer: Mutex::new(Some(peer)),
            pending_requests: Mutex::new(HashMap::new()),
            pending_prompt_responses: Mutex::new(HashMap::new()),
            prompt_response_receivers: Mutex::new(HashMap::new()),
            blocked_sends: AtomicUsize::new(0),
            prompt_result_tokens_used: AtomicU64::new(0),
            usage_update_tokens_used: AtomicU64::new(0),
            active_prompt_id: Mutex::new(None),
            active_prompt_token_attribution: Mutex::new(None),
            event_tx: StdMutex::new(event_tx),
            alive: AtomicBool::new(true),
            terminal_manager: super::terminals::TerminalManager::new(),
            transcript_buffer: TranscriptBuffer::new(10000),
            permission_manager,
            permission_timeout: session_config.permission_timeout,
            config_manager,
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner));
        *inner.reader_handle.lock().await = Some(reader_handle);
        persist_session_snapshot(
            session_id.as_str(),
            brehon_types::StabilityCounters::default(),
        );

        debug!(session_id = %session_id, capabilities = ?capabilities, "Session initialized");

        Ok(Self { inner, created_at })
    }

    /// Attaches a channel for receiving [`SessionEvent`]s from this session.
    pub fn with_event_channel(self, tx: mpsc::Sender<SessionEvent>) -> Self {
        *self
            .inner
            .event_tx
            .lock()
            .expect("event channel mutex poisoned") = Some(tx);
        self
    }

    /// Sends a prompt to the agent and returns a handle for tracking the response.
    ///
    /// Registers a oneshot sender/receiver pair before sending so that the
    /// reader loop can complete prompt waits without polling.
    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, SessionError> {
        let prompt_id = prompt.prompt_id.as_str().to_string();
        {
            let mut active_prompt = self.inner.active_prompt_id.lock().await;
            claim_active_prompt_slot(&mut active_prompt, &prompt_id)?;
        }
        self.set_prompt_token_attribution(&prompt_id, &prompt.content)
            .await;

        let request = super::acp_types::create_prompt_request(
            &prompt_id,
            &self.inner.remote_session_id,
            &prompt.content,
        );

        let (tx, rx) = oneshot::channel();
        // Ordering invariant: sender and receiver are registered before the
        // process receives the prompt. The reader loop cannot observe a
        // response for this prompt until send_line completes below, so there
        // is no race between registration and completion even though these
        // are two separate lock acquisitions.
        self.inner
            .pending_prompt_responses
            .lock()
            .await
            .insert(prompt_id.clone(), tx);
        self.inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(prompt_id, rx);
        self.persist_runtime_stability();

        let mut peer_guard = self.inner.peer.lock().await;
        let peer = match peer_guard.as_mut() {
            Some(peer) => peer,
            None => {
                drop(peer_guard);
                self.cleanup_pending_prompt(prompt.prompt_id.as_str()).await;
                let mut active_prompt = self.inner.active_prompt_id.lock().await;
                clear_active_prompt_slot(&mut active_prompt, prompt.prompt_id.as_str());
                self.inner.blocked_sends.fetch_add(1, Ordering::Relaxed);
                self.persist_runtime_stability();
                return Err(SessionError::NotRunning);
            }
        };

        let line = match super::protocol::serialize_request(&request) {
            Ok(line) => line,
            Err(e) => {
                drop(peer_guard);
                self.cleanup_pending_prompt(prompt.prompt_id.as_str()).await;
                let mut active_prompt = self.inner.active_prompt_id.lock().await;
                clear_active_prompt_slot(&mut active_prompt, prompt.prompt_id.as_str());
                self.persist_runtime_stability();
                return Err(SessionError::Protocol(e.message));
            }
        };

        if let Err(e) = peer.send_line(&line).await {
            drop(peer_guard);
            self.cleanup_pending_prompt(prompt.prompt_id.as_str()).await;
            let mut active_prompt = self.inner.active_prompt_id.lock().await;
            clear_active_prompt_slot(&mut active_prompt, prompt.prompt_id.as_str());
            self.inner.blocked_sends.fetch_add(1, Ordering::Relaxed);
            self.persist_runtime_stability();
            return Err(SessionError::Io(e.to_string()));
        }
        drop(peer_guard);

        let handle = PromptHandle {
            prompt_id: prompt.prompt_id.clone(),
            session_id: self.inner.session_id.as_str().to_string(),
            created_at: prompt.sent_at,
        };

        debug!(session_id = %self.inner.session_id, prompt_id = %prompt.prompt_id, "Sent prompt");
        Ok(handle)
    }

    /// Removes a prompt ID from both pending prompt-response maps.
    async fn cleanup_pending_prompt(&self, prompt_id: &str) {
        self.inner
            .pending_prompt_responses
            .lock()
            .await
            .remove(prompt_id);
        self.inner
            .prompt_response_receivers
            .lock()
            .await
            .remove(prompt_id);
        clear_prompt_token_attribution(&self.inner, prompt_id).await;
    }

    async fn set_prompt_token_attribution(&self, prompt_id: &str, prompt_content: &str) {
        let task_id = brehon_root_from_env().and_then(|root| {
            brehon_types::infer_task_token_target(
                &root,
                self.inner.spec.agent_id.as_str(),
                &self.inner.spec.role,
                prompt_content,
            )
        });
        let mut attribution = self.inner.active_prompt_token_attribution.lock().await;
        *attribution = task_id.map(|task_id| ActivePromptTokenAttribution {
            prompt_id: prompt_id.to_string(),
            task_id,
            usage_tokens_attributed: 0,
        });
    }

    /// Waits for a prompt response using event-driven completion with timeout.
    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<super::acp_types::PromptResult, SessionError> {
        self.wait_for_response_internal(prompt_id, timeout_ms, None)
            .await
    }

    async fn wait_for_response_internal(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
        poll_notify: Option<Arc<Notify>>,
    ) -> Result<super::acp_types::PromptResult, SessionError> {
        let receiver = self
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .remove(prompt_id.as_str())
            .ok_or_else(|| {
                SessionError::Protocol(format!(
                    "No pending prompt response for {}",
                    prompt_id.as_str()
                ))
            })?;

        match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            await_oneshot_receiver(receiver, poll_notify),
        )
        .await
        {
            Ok(Ok(result)) => {
                self.persist_runtime_stability();
                result.map_err(SessionError::Protocol)
            }
            Ok(Err(_)) => {
                self.persist_runtime_stability();
                Err(SessionError::ProcessDied)
            }
            Err(_) => {
                self.inner
                    .pending_prompt_responses
                    .lock()
                    .await
                    .remove(prompt_id.as_str());
                clear_prompt_token_attribution(&self.inner, prompt_id.as_str()).await;
                self.persist_runtime_stability();
                Err(SessionError::Timeout)
            }
        }
    }

    async fn handle_notification(
        inner: &Arc<AcpSessionInner>,
        notification: super::protocol::JsonRpcNotification,
    ) -> Result<(), SessionError> {
        if notification.method != "session/update" {
            return Ok(());
        }

        let Some(params) = notification.params else {
            return Ok(());
        };

        let update = params.get("update").unwrap_or(&params);
        if record_usage_update_tokens(inner, update).await {
            let pending = inner.pending_requests.lock().await.len();
            let results = inner.pending_prompt_responses.lock().await.len();
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                session_stability_counters(inner, pending, results),
            );
        }

        let event = super::updates::normalize_session_update_value(&inner.session_id, update)
            .map_err(|e| SessionError::UpdateError(e.to_string()))?;

        let Some(event) = event else {
            return Ok(());
        };

        let tx = {
            inner
                .event_tx
                .lock()
                .expect("event channel mutex poisoned")
                .clone()
        };

        if let Some(tx) = tx {
            let _ = tx.send(event).await;
        }

        Ok(())
    }

    /// Cancels an in-flight prompt by sending a cancellation notification to the agent.
    pub async fn cancel_prompt(
        &self,
        prompt_id: &PromptId,
        _reason: Option<&str>,
    ) -> Result<(), SessionError> {
        let notification =
            super::acp_types::create_cancel_notification(&self.inner.remote_session_id);

        let line = super::protocol::serialize_notification(&notification)
            .map_err(|e| SessionError::Protocol(e.message))?;

        let send_result = {
            let mut peer = self.inner.peer.lock().await;
            match peer.as_mut() {
                Some(peer) => peer
                    .send_line(&line)
                    .await
                    .map_err(|e| SessionError::Io(e.to_string())),
                None => Err(SessionError::NotRunning),
            }
        };

        self.cleanup_pending_prompt(prompt_id.as_str()).await;
        let mut active_prompt = self.inner.active_prompt_id.lock().await;
        clear_active_prompt_slot(&mut active_prompt, prompt_id.as_str());
        drop(active_prompt);
        self.persist_runtime_stability();

        send_result?;

        debug!(session_id = %self.inner.session_id, prompt_id = %prompt_id, "Cancelled prompt");
        Ok(())
    }

    /// Requests a terminal attachment from the agent, returning its ID on success.
    ///
    /// Returns `Ok(None)` if the agent does not advertise terminal support.
    pub async fn attach_terminal(
        &self,
        cols: u16,
        rows: u16,
    ) -> Result<Option<TerminalId>, SessionError> {
        if !self.inner.capabilities.terminal_support {
            debug!("Terminal support not advertised, returning None");
            return Ok(None);
        }

        let request = super::acp_types::create_terminal_attach_request(
            &self.inner.remote_session_id,
            cols,
            rows,
        );
        let response = self.send_request(request).await?;

        match response.error {
            Some(err) => {
                if err.code == -32601 || err.message.contains("not supported") {
                    return Ok(None);
                }
                Err(SessionError::TerminalError(err.message))
            }
            None => {
                let result: super::acp_types::TerminalAttachResult =
                    serde_json::from_value(response.result.unwrap_or(serde_json::Value::Null))
                        .map_err(|e| {
                            SessionError::TerminalError(format!(
                                "Failed to parse terminal attach result: {e}"
                            ))
                        })?;
                self.inner
                    .terminal_manager
                    .register_attached_terminal(
                        &self.inner.session_id,
                        &result.terminal_id,
                        cols,
                        rows,
                    )
                    .await;
                Ok(Some(TerminalId::new(&result.terminal_id)))
            }
        }
    }

    /// Writes raw bytes to an attached terminal.
    pub async fn send_terminal_input(
        &self,
        terminal_id: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), SessionError> {
        if !self.inner.capabilities.terminal_support {
            return Err(SessionError::TerminalError("Terminal not supported".into()));
        }

        let mut peer = self.inner.peer.lock().await;
        let peer = peer.as_mut().ok_or(SessionError::NotRunning)?;

        self.inner
            .terminal_manager
            .send_input(peer.as_mut(), terminal_id, input)
            .await
            .map_err(|e| SessionError::TerminalError(e.to_string()))
    }

    /// Sets a configuration option on the remote agent session.
    pub async fn set_config(
        &self,
        option: &str,
        value: &str,
        required: bool,
    ) -> Result<(), SessionError> {
        let options = vec![ConfigOption {
            name: option.to_string(),
            value: value.to_string(),
            required,
        }];
        self.inner
            .config_manager
            .check_support(&options)
            .map_err(|e| SessionError::ConfigError(e.to_string()))?;

        let request = super::acp_types::create_set_config_request(
            &self.inner.remote_session_id,
            option,
            value,
        );
        let response = self.send_request(request).await?;

        if let Some(err) = response.error {
            return Err(SessionError::ConfigError(err.message));
        }

        debug!(session_id = %self.inner.session_id, option, value, "Set config option");
        Ok(())
    }

    /// Returns the current health status of the session.
    pub async fn health_check(&self) -> Result<HealthStatus, SessionError> {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return Ok(HealthStatus::Unhealthy);
        }
        let peer = self.inner.peer.lock().await;
        match peer.as_ref() {
            Some(proc) if proc.is_alive() => Ok(HealthStatus::Healthy),
            Some(_) => Ok(HealthStatus::Unhealthy),
            None => Ok(HealthStatus::Unknown),
        }
    }

    /// Immediately terminates the agent transport.
    /// Terminates the agent transport and awaits all spawned work for
    /// deterministic shutdown.
    ///
    /// Sets the `shutdown` flag so the reader loop exits promptly, kills the
    /// underlying peer (which also awaits its own reader tasks), then takes
    /// and joins the session reader `JoinHandle` with a bounded timeout.
    /// Finally clears all pending prompt/request entries.
    pub async fn kill(&self) -> Result<(), SessionError> {
        shutdown_session_inner(&self.inner, Duration::from_secs(2)).await
    }

    /// Returns `true` if the agent transport is still running.
    pub async fn is_alive(&self) -> bool {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return false;
        }
        let peer = self.inner.peer.lock().await;
        match peer.as_ref() {
            Some(proc) => proc.is_alive(),
            None => false,
        }
    }

    /// Returns the capabilities negotiated during the ACP handshake.
    pub fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities.clone()
    }

    /// Returns metadata about this session (ID, agent, role, capabilities).
    pub fn session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.inner.session_id.clone(),
            agent_id: self.inner.spec.agent_id.clone(),
            role: self.inner.spec.role.clone(),
            health: HealthStatus::Unknown,
            created_at: self.created_at,
            capabilities: self.inner.capabilities.clone(),
        }
    }

    pub async fn resolve_permission(
        &self,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> PermissionResolution {
        self.inner
            .permission_manager
            .resolve(permission_id, decision)
            .await
    }

    #[cfg(test)]
    pub(crate) fn test_session_with_channel(
        event_tx: Option<mpsc::Sender<SessionEvent>>,
        permission_timeout: Duration,
        policy: PermissionPolicy,
    ) -> Self {
        let capabilities = AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: true,
            terminal_support: false,
            tool_call_streaming: brehon_types::ToolCallStreaming::None,
        };
        Self {
            inner: Arc::new(AcpSessionInner {
                session_id: SessionId::new("session-1"),
                remote_session_id: "remote-1".to_string(),
                spec: SessionSpec::new(
                    brehon_types::AgentId::new("agent-1"),
                    "worker".to_string(),
                    ".".to_string(),
                ),
                capabilities: capabilities.clone(),
                peer: Mutex::new(None),
                pending_requests: Mutex::new(HashMap::new()),
                pending_prompt_responses: Mutex::new(HashMap::new()),
                prompt_response_receivers: Mutex::new(HashMap::new()),
                blocked_sends: AtomicUsize::new(0),
                prompt_result_tokens_used: AtomicU64::new(0),
                usage_update_tokens_used: AtomicU64::new(0),
                active_prompt_token_attribution: Mutex::new(None),
                event_tx: StdMutex::new(event_tx),
                alive: AtomicBool::new(true),
                terminal_manager: super::terminals::TerminalManager::new(),
                transcript_buffer: TranscriptBuffer::new(10),
                permission_manager: super::permissions::PermissionManager::new(policy),
                permission_timeout,
                config_manager: super::config::ConfigManager::new(&capabilities),
                shutdown: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
                active_prompt_id: Mutex::new(None),
            }),
            created_at: chrono::Utc::now(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn test_response_for_request(
        &self,
        request: super::protocol::JsonRpcRequest,
    ) -> super::protocol::JsonRpcResponse {
        response_for_agent_request(&self.inner, &request).await
    }

    /// Returns the most recent `count` lines from the session transcript buffer.
    pub async fn get_transcript(&self, count: usize) -> Vec<super::terminals::TranscriptLine> {
        self.inner.transcript_buffer.get_recent(count).await
    }

    /// Appends a line to the session transcript buffer.
    pub async fn append_transcript(&self, content: String, is_stdout: bool) {
        self.inner
            .transcript_buffer
            .append(content, is_stdout)
            .await;
    }

    /// Returns a reference to this session's unique identifier.
    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }

    /// Returns a reference to the terminal manager for this session.
    pub fn terminal_manager(&self) -> &super::terminals::TerminalManager {
        &self.inner.terminal_manager
    }

    /// Returns a reference to the mutex-guarded ACP peer handle.
    pub(crate) fn peer(&self) -> &Mutex<Option<Box<dyn AcpPeer>>> {
        &self.inner.peer
    }

    /// Derive a stability counter snapshot for this session.
    ///
    /// `pending_requests` and pending prompt waiters require async locks;
    /// `blocked_sends` is read atomically without locking.
    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        let pending = self.inner.pending_requests.lock().await.len();
        let results = self.inner.pending_prompt_responses.lock().await.len();
        session_stability_counters(&self.inner, pending, results)
    }

    fn persist_runtime_stability(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let pending = inner.pending_requests.lock().await.len();
            let results = inner.pending_prompt_responses.lock().await.len();
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                session_stability_counters(&inner, pending, results),
            );
        });
    }

    async fn send_request(
        &self,
        request: super::protocol::JsonRpcRequest,
    ) -> Result<super::protocol::JsonRpcResponse, SessionError> {
        self.send_request_internal(request, None).await
    }

    async fn send_request_internal(
        &self,
        request: super::protocol::JsonRpcRequest,
        poll_notify: Option<Arc<Notify>>,
    ) -> Result<super::protocol::JsonRpcResponse, SessionError> {
        let (tx, rx) = oneshot::channel();
        let request_id = request.id.clone();

        self.inner
            .pending_requests
            .lock()
            .await
            .insert(request_id.clone(), tx);
        self.persist_runtime_stability();

        let line = match super::protocol::serialize_request(&request) {
            Ok(line) => line,
            Err(e) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                return Err(SessionError::Protocol(e.message));
            }
        };

        enum SendFailure {
            NotRunning,
            Io(String),
        }

        let send_result = {
            let mut peer = self.inner.peer.lock().await;
            match peer.as_mut() {
                Some(proc) => proc
                    .send_line(&line)
                    .await
                    .map_err(|err| SendFailure::Io(err.to_string())),
                None => Err(SendFailure::NotRunning),
            }
        };

        if let Err(err) = send_result {
            self.inner.pending_requests.lock().await.remove(&request_id);
            self.inner.blocked_sends.fetch_add(1, Ordering::Relaxed);
            self.persist_runtime_stability();
            return Err(match err {
                SendFailure::NotRunning => SessionError::NotRunning,
                SendFailure::Io(err) => SessionError::Io(err),
            });
        }

        let response = await_oneshot_receiver(rx, poll_notify)
            .await
            .map_err(|_| SessionError::ProcessDied);
        self.persist_runtime_stability();
        response
    }
}

async fn await_oneshot_receiver<T>(
    receiver: oneshot::Receiver<T>,
    poll_notify: Option<Arc<Notify>>,
) -> Result<T, oneshot::error::RecvError> {
    let mut receiver = Box::pin(receiver);
    let mut notified = false;

    poll_fn(|cx| {
        if !notified {
            notified = true;
            if let Some(notify) = poll_notify.as_ref() {
                notify.notify_one();
            }
        }
        receiver.as_mut().poll(cx)
    })
    .await
}

async fn shutdown_session_inner(
    inner: &Arc<AcpSessionInner>,
    reader_join_timeout: Duration,
) -> Result<(), SessionError> {
    inner.alive.store(false, Ordering::SeqCst);
    inner.shutdown.store(true, Ordering::SeqCst);

    let peer = {
        let mut peer = inner.peer.lock().await;
        peer.take()
    };

    let peer_shutdown_result = if let Some(mut proc) = peer {
        proc.shutdown()
            .await
            .map_err(|e| SessionError::KillFailed(e.to_string()))
    } else {
        Ok(())
    };

    join_reader_task(inner, reader_join_timeout).await;
    clear_pending_session_state(inner).await;

    debug!(session_id = %inner.session_id, "Session killed");
    clear_session_snapshot(inner.session_id.as_str());
    peer_shutdown_result
}

async fn join_reader_task(inner: &Arc<AcpSessionInner>, timeout: Duration) {
    let handle = inner.reader_handle.lock().await.take();
    let Some(mut handle) = handle else {
        return;
    };

    tokio::select! {
        result = &mut handle => {
            if let Err(err) = result {
                warn!(
                    session_id = %inner.session_id,
                    error = %err,
                    "ACP session reader task exited with an error during shutdown"
                );
            }
        }
        _ = tokio::time::sleep(timeout) => {
            warn!(
                session_id = %inner.session_id,
                "ACP session reader task did not exit before shutdown timeout; aborting"
            );
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn clear_pending_session_state(inner: &Arc<AcpSessionInner>) {
    inner.pending_prompt_responses.lock().await.clear();
    inner.prompt_response_receivers.lock().await.clear();
    inner.pending_requests.lock().await.clear();
    inner.active_prompt_token_attribution.lock().await.take();
    inner.active_prompt_id.lock().await.take();
}

fn session_tokens_used(inner: &AcpSessionInner) -> u64 {
    inner
        .prompt_result_tokens_used
        .load(Ordering::Relaxed)
        .max(inner.usage_update_tokens_used.load(Ordering::Relaxed))
}

fn session_stability_counters(
    inner: &AcpSessionInner,
    pending_requests: usize,
    pending_prompt_waiters: usize,
) -> brehon_types::StabilityCounters {
    brehon_types::StabilityCounters {
        pending_requests,
        pending_prompt_waiters,
        blocked_sends: inner.blocked_sends.load(Ordering::Relaxed),
        tokens_used: session_tokens_used(inner),
        ..Default::default()
    }
}

async fn record_prompt_result_tokens(
    inner: &Arc<AcpSessionInner>,
    prompt_id: &str,
    tokens_used: Option<u64>,
) {
    if let Some(tokens) = tokens_used.filter(|tokens| *tokens > 0) {
        inner
            .prompt_result_tokens_used
            .fetch_add(tokens, Ordering::Relaxed);
    }
    finish_prompt_token_attribution(inner, prompt_id, tokens_used).await;
}

async fn record_usage_update_tokens(
    inner: &Arc<AcpSessionInner>,
    update: &serde_json::Value,
) -> bool {
    let Some(tokens) = usage_tokens_from_update(update) else {
        return false;
    };
    let previous = inner
        .usage_update_tokens_used
        .fetch_max(tokens, Ordering::Relaxed);
    if tokens > previous {
        attribute_usage_delta_to_active_task(inner, tokens - previous).await;
    }
    true
}

async fn attribute_usage_delta_to_active_task(inner: &Arc<AcpSessionInner>, tokens_delta: u64) {
    if tokens_delta == 0 {
        return;
    }
    let task_id = {
        let mut attribution = inner.active_prompt_token_attribution.lock().await;
        let Some(attribution) = attribution.as_mut() else {
            return;
        };
        attribution.usage_tokens_attributed = attribution
            .usage_tokens_attributed
            .saturating_add(tokens_delta);
        attribution.task_id.clone()
    };
    persist_task_token_delta(inner, &task_id, tokens_delta);
}

async fn finish_prompt_token_attribution(
    inner: &Arc<AcpSessionInner>,
    prompt_id: &str,
    prompt_result_tokens: Option<u64>,
) {
    let task_delta = {
        let mut attribution = inner.active_prompt_token_attribution.lock().await;
        let Some(active) = attribution.take() else {
            return;
        };
        if active.prompt_id != prompt_id {
            *attribution = Some(active);
            return;
        }
        Some((
            active.task_id,
            prompt_result_tokens
                .unwrap_or(0)
                .saturating_sub(active.usage_tokens_attributed),
        ))
    };

    if let Some((task_id, delta)) = task_delta.filter(|(_, delta)| *delta > 0) {
        persist_task_token_delta(inner, &task_id, delta);
    }
}

async fn clear_prompt_token_attribution(inner: &Arc<AcpSessionInner>, prompt_id: &str) {
    let mut attribution = inner.active_prompt_token_attribution.lock().await;
    if attribution
        .as_ref()
        .is_some_and(|active| active.prompt_id == prompt_id)
    {
        attribution.take();
    }
}

fn persist_task_token_delta(inner: &AcpSessionInner, task_id: &str, tokens_delta: u64) {
    if tokens_delta == 0 {
        return;
    }
    let Some(root) = brehon_root_from_env() else {
        return;
    };
    if let Err(err) = brehon_types::record_task_token_usage(&root, task_id, tokens_delta) {
        debug!(
            session_id = %inner.session_id,
            task_id,
            tokens_delta,
            error = %err,
            "Failed to persist task token usage"
        );
    }
}

fn usage_tokens_from_update(update: &serde_json::Value) -> Option<u64> {
    if update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(serde_json::Value::as_str)
        != Some("usage_update")
    {
        return None;
    }

    let usage = update
        .get("usage")
        .or_else(|| update.get("usageUpdate"))
        .unwrap_or(update);

    token_field(
        usage,
        &[
            "tokensUsed",
            "tokens_used",
            "totalTokens",
            "total_tokens",
            "totalTokenCount",
            "total_token_count",
        ],
    )
    .or_else(|| {
        sum_token_fields(
            usage,
            &[
                "inputTokens",
                "input_tokens",
                "promptTokens",
                "prompt_tokens",
                "outputTokens",
                "output_tokens",
                "completionTokens",
                "completion_tokens",
                "cacheCreationInputTokens",
                "cache_creation_input_tokens",
                "cacheReadInputTokens",
                "cache_read_input_tokens",
                "cachedTokens",
                "cached_tokens",
                "reasoningTokens",
                "reasoning_tokens",
            ],
        )
    })
}

fn token_field(value: &serde_json::Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(token_value))
}

fn sum_token_fields(value: &serde_json::Value, fields: &[&str]) -> Option<u64> {
    let mut total = 0u64;
    let mut found = false;
    for field in fields {
        if let Some(tokens) = value.get(*field).and_then(token_value) {
            total = total.saturating_add(tokens);
            found = true;
        }
    }
    found.then_some(total)
}

fn token_value(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
        .or_else(|| value.as_str().and_then(|v| v.trim().parse().ok()))
}

fn spawn_reader(inner: Arc<AcpSessionInner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Check the shutdown flag first so the reader exits promptly
            // when kill() is called, rather than waiting for the next
            // recv_line timeout.
            if inner.shutdown.load(Ordering::SeqCst) {
                debug!(session_id = %inner.session_id, "Reader exiting due to shutdown signal");
                break;
            }

            let line = {
                let mut peer = inner.peer.lock().await;
                let Some(proc) = peer.as_mut() else {
                    break;
                };
                match proc.recv_line(100).await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(_) => continue,
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            match super::protocol::parse_message(&line) {
                Ok(JsonRpcMessage::Response(response)) => {
                    let (waiter, pending_request_count) =
                        remove_pending_request(&inner.pending_requests, &response.id).await;
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(response);
                        let pending_prompt_waiters =
                            inner.pending_prompt_responses.lock().await.len();
                        schedule_persist_session_snapshot(
                            inner.session_id.as_str().to_string(),
                            session_stability_counters(
                                &inner,
                                pending_request_count,
                                pending_prompt_waiters,
                            ),
                        );
                        continue;
                    }

                    let mut active_prompt = inner.active_prompt_id.lock().await;
                    clear_active_prompt_slot(&mut active_prompt, &response.id);

                    let prompt_result = super::acp_types::parse_prompt_result(&response)
                        .map_err(|err| format!("Failed to parse prompt result: {err}"));
                    if let Ok(result) = &prompt_result {
                        record_prompt_result_tokens(&inner, &response.id, result.tokens_used).await;
                    } else {
                        clear_prompt_token_attribution(&inner, &response.id).await;
                    }
                    if let Some(sender) = inner
                        .pending_prompt_responses
                        .lock()
                        .await
                        .remove(&response.id)
                    {
                        let _ = sender.send(prompt_result);
                    }
                    schedule_persist_session_snapshot(
                        inner.session_id.as_str().to_string(),
                        session_stability_counters(
                            &inner,
                            inner.pending_requests.lock().await.len(),
                            inner.pending_prompt_responses.lock().await.len(),
                        ),
                    );
                }
                Ok(JsonRpcMessage::Notification(notification)) => {
                    if let Err(err) = AcpSession::handle_notification(&inner, notification).await {
                        debug!(session_id = %inner.session_id, error = %err, "Failed to handle ACP notification");
                    }
                }
                Ok(JsonRpcMessage::Request(request)) => {
                    if let Err(err) = handle_agent_request(Arc::clone(&inner), request).await {
                        debug!(
                            session_id = %inner.session_id,
                            error = %err,
                            "Failed to handle ACP server request"
                        );
                    }
                }
                Err(err) => {
                    debug!(
                        session_id = %inner.session_id,
                        raw = %line,
                        error = %err.message,
                        "Ignoring non-JSON ACP output"
                    );
                }
            }
        }

        inner.alive.store(false, Ordering::SeqCst);
        inner.pending_prompt_responses.lock().await.clear();
        inner.prompt_response_receivers.lock().await.clear();
        inner.pending_requests.lock().await.clear();
        inner.active_prompt_token_attribution.lock().await.take();
        schedule_clear_session_snapshot(inner.session_id.as_str().to_string());
    })
}

async fn handle_agent_request(
    inner: Arc<AcpSessionInner>,
    request: super::protocol::JsonRpcRequest,
) -> Result<(), String> {
    let response = response_for_agent_request(&inner, &request).await;

    let line = serde_json::to_string(&response).map_err(|err| err.to_string())?;
    let mut peer = inner.peer.lock().await;
    let proc = peer
        .as_mut()
        .ok_or_else(|| "ACP peer is no longer running".to_string())?;
    proc.send_line(&line).await.map_err(|err| err.to_string())
}

fn is_permission_request(request: &super::protocol::JsonRpcRequest) -> bool {
    matches!(
        request.method.as_str(),
        "requestPermission" | "session/request_permission"
    )
}

async fn response_for_agent_request(
    inner: &Arc<AcpSessionInner>,
    request: &super::protocol::JsonRpcRequest,
) -> super::protocol::JsonRpcResponse {
    if is_permission_request(request) {
        return mediate_permission_request(inner, request).await;
    }

    super::protocol::JsonRpcResponse::error(
        request.id.clone(),
        super::protocol::JsonRpcError::method_not_found(),
    )
}

async fn mediate_permission_request(
    inner: &Arc<AcpSessionInner>,
    request: &super::protocol::JsonRpcRequest,
) -> super::protocol::JsonRpcResponse {
    let permission_request = PermissionRequest {
        session_id: inner.session_id.clone(),
        permission_id: request.id.clone(),
        action: permission_action(request),
        details: request.params.clone(),
        timestamp: chrono::Utc::now(),
    };

    let decision = match inner
        .permission_manager
        .decision_for_request(&permission_request)
    {
        PermissionDecision::Approved => PermissionDecision::Approved,
        PermissionDecision::Denied => PermissionDecision::Denied,
        PermissionDecision::Ask => {
            let mediation_started_at = Instant::now();
            let action = permission_request.action.clone();
            let event_tx = inner
                .event_tx
                .lock()
                .expect("event channel mutex poisoned")
                .clone();

            let Some(tx) = event_tx else {
                return super::protocol::JsonRpcResponse::success(
                    request.id.clone(),
                    permission_response_for_decision(
                        request.params.as_ref(),
                        inner.permission_manager.timeout_decision(),
                    ),
                );
            };

            let response_rx = inner
                .permission_manager
                .register_request(permission_request)
                .await;

            let event = SessionEvent::PermissionRequest {
                session_id: inner.session_id.clone(),
                permission_id: request.id.clone(),
                action,
                details: request.params.clone(),
            };
            let send_timeout = inner
                .permission_timeout
                .saturating_sub(mediation_started_at.elapsed());

            let handoff_delivered = send_permission_event(&tx, event, send_timeout).await;

            if !handoff_delivered {
                inner.permission_manager.expire(request.id.as_str()).await;
                inner.permission_manager.timeout_decision()
            } else {
                let wait_timeout = inner
                    .permission_timeout
                    .saturating_sub(mediation_started_at.elapsed());

                match await_permission_decision(response_rx, wait_timeout).await {
                    Some(PermissionDecision::Approved) => PermissionDecision::Approved,
                    Some(PermissionDecision::Denied) => PermissionDecision::Denied,
                    Some(PermissionDecision::Ask) | None => {
                        // Safe to expire here: if resolve() already removed the entry,
                        // expire() becomes a no-op and preserves the winning decision.
                        inner.permission_manager.expire(request.id.as_str()).await;
                        inner.permission_manager.timeout_decision()
                    }
                }
            }
        }
    };

    emit_permission_resolution_event(inner, request.id.as_str(), decision).await;

    super::protocol::JsonRpcResponse::success(
        request.id.clone(),
        permission_response_for_decision(request.params.as_ref(), decision),
    )
}

/// Sends the event within the remaining timeout budget.
///
/// `Duration::ZERO` still permits one immediate poll, so a send succeeds when
/// the channel can accept the event without waiting.
async fn send_permission_event(
    tx: &mpsc::Sender<SessionEvent>,
    event: SessionEvent,
    timeout: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(timeout, tx.send(event)).await,
        Ok(Ok(()))
    )
}

async fn await_permission_decision(
    response_rx: oneshot::Receiver<PermissionDecision>,
    timeout: Duration,
) -> Option<PermissionDecision> {
    match tokio::time::timeout(timeout, response_rx).await {
        Ok(Ok(decision)) => Some(decision),
        Ok(Err(_)) | Err(_) => None,
    }
}

async fn emit_permission_resolution_event(
    inner: &Arc<AcpSessionInner>,
    permission_id: &str,
    decision: PermissionDecision,
) {
    let event_tx = inner
        .event_tx
        .lock()
        .expect("event channel mutex poisoned")
        .clone();
    let Some(tx) = event_tx else {
        return;
    };
    let approved = matches!(decision, PermissionDecision::Approved);
    let event = SessionEvent::PermissionResolved {
        session_id: inner.session_id.clone(),
        permission_id: permission_id.to_string(),
        approved,
    };
    match tx.try_send(event) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            warn!(
                session_id = %inner.session_id,
                permission_id,
                "Permission resolution event channel is full; dropping resolution event"
            );
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            debug!(
                session_id = %inner.session_id,
                permission_id,
                "Permission resolution event channel is closed"
            );
        }
    }
}

fn permission_action(request: &super::protocol::JsonRpcRequest) -> String {
    request
        .params
        .as_ref()
        .and_then(|params| params.get("action"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            request
                .params
                .as_ref()
                .and_then(|params| params.get("kind"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or(request.method.as_str())
        .to_string()
}

fn permission_response_for_decision(
    params: Option<&serde_json::Value>,
    decision: PermissionDecision,
) -> serde_json::Value {
    let selected = params
        .and_then(|params| params.get("options"))
        .and_then(serde_json::Value::as_array)
        .and_then(|options| {
            options.iter().find_map(|option| {
                let kind = option.get("kind").and_then(serde_json::Value::as_str)?;
                let option_id = option
                    .get("optionId")
                    .or_else(|| option.get("id"))
                    .and_then(serde_json::Value::as_str)?;
                match decision {
                    PermissionDecision::Approved
                        if kind.starts_with("allow_") || kind == "allow" || kind == "approve" =>
                    {
                        Some(option_id.to_string())
                    }
                    PermissionDecision::Denied
                        if kind.starts_with("deny_")
                            || kind == "deny"
                            || kind == "cancel"
                            || kind == "reject" =>
                    {
                        Some(option_id.to_string())
                    }
                    _ => None,
                }
            })
        });

    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
            }
        }),
        None => serde_json::json!({
            "outcome": {
                "outcome": "cancelled",
            }
        }),
    }
}

async fn wait_for_response(
    peer: &mut dyn AcpPeer,
    expected_id: &str,
    timeout: std::time::Duration,
    handshake_permission_decision: PermissionDecision,
) -> Result<super::protocol::JsonRpcResponse, String> {
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err("Timeout waiting for response".to_string());
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        match peer.recv_line(remaining.as_millis() as u64).await {
            Ok(Some(line)) => {
                if line.is_empty() {
                    continue;
                }

                match super::protocol::parse_message(&line) {
                    Ok(JsonRpcMessage::Response(response)) => {
                        if response.id == expected_id {
                            return Ok(response);
                        }
                    }
                    Ok(JsonRpcMessage::Notification(_)) => {
                        continue;
                    }
                    Ok(JsonRpcMessage::Request(request)) => {
                        let response = if is_permission_request(&request) {
                            super::protocol::JsonRpcResponse::success(
                                request.id.clone(),
                                permission_response_for_decision(
                                    request.params.as_ref(),
                                    handshake_permission_decision,
                                ),
                            )
                        } else {
                            super::protocol::JsonRpcResponse::error(
                                request.id.clone(),
                                super::protocol::JsonRpcError::method_not_found(),
                            )
                        };
                        let line =
                            serde_json::to_string(&response).map_err(|err| err.to_string())?;
                        peer.send_line(&line).await.map_err(|err| err.to_string())?;
                        continue;
                    }
                    Err(e) => {
                        debug!(expected_id, raw = %line, error = %e.message, "Ignoring non-JSON ACP output during handshake");
                        continue;
                    }
                }
            }
            Ok(None) => {
                return Err("Process died".to_string());
            }
            Err(_) => {
                continue;
            }
        }
    }
}

fn claim_active_prompt_slot(
    active_prompt: &mut Option<String>,
    prompt_id: &str,
) -> Result<(), SessionError> {
    if let Some(existing) = active_prompt.as_ref() {
        return Err(SessionError::PromptInProgress(existing.clone()));
    }
    *active_prompt = Some(prompt_id.to_string());
    Ok(())
}

fn clear_active_prompt_slot(active_prompt: &mut Option<String>, prompt_id: &str) {
    if active_prompt.as_deref() == Some(prompt_id) {
        *active_prompt = None;
    }
}

async fn remove_pending_request(
    pending_requests: &Mutex<HashMap<String, oneshot::Sender<super::protocol::JsonRpcResponse>>>,
    request_id: &str,
) -> (
    Option<oneshot::Sender<super::protocol::JsonRpcResponse>>,
    usize,
) {
    let mut pending_requests = pending_requests.lock().await;
    let waiter = pending_requests.remove(request_id);
    let remaining = pending_requests.len();
    (waiter, remaining)
}

/// Errors that can occur during ACP session operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session initialization failed: {0}")]
    InitFailed(String),
    #[error("Session not running")]
    NotRunning,
    #[error("Process died")]
    ProcessDied,
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Timeout")]
    Timeout,
    #[error("Malformed JSON")]
    MalformedJson,
    #[error("Prompt already in progress: {0}")]
    PromptInProgress(String),
    #[error("Config error: {0}")]
    ConfigError(String),
    #[error("Terminal error: {0}")]
    TerminalError(String),
    #[error("Shutdown failed: {0}")]
    ShutdownFailed(String),
    #[error("Kill failed: {0}")]
    KillFailed(String),
    #[error("Update error: {0}")]
    UpdateError(String),
}

impl From<SessionError> for PortError {
    fn from(err: SessionError) -> Self {
        PortError::Agent(err.to_string())
    }
}

#[async_trait::async_trait]
impl AgentAdapter for AcpSession {
    async fn spawn(&self, _spec: SessionSpec) -> AdapterResult<SessionId> {
        Err(AdapterError::unsupported_operation(
            "AcpSession must be constructed via spawn_with_env; AgentAdapter::spawn is not supported",
        ))
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        AcpSession::send_prompt(self, prompt)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let result = AcpSession::wait_for_response(self, prompt_id, timeout_ms)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::TimedOut, e.to_string()))?;
        let mut pr = PromptResult::default();
        pr.response = result.response;
        pr.tokens_used = result.tokens_used;
        pr.stop_reason = result.stop_reason;
        Ok(pr)
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (_tx, rx) = mpsc::channel(1);
        // AcpSession delivers events through the SessionEvent channel passed
        // during spawn (agent_event_channels), not through this trait method.
        // The gateway directly subscribes to that channel for ACP-stdio
        // sessions, so this method returns an empty receiver.
        rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        AcpSession::kill(self)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::TransportClosed, e.to_string()))
    }

    fn kind(&self) -> AdapterKind {
        AdapterKind::Acp
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        Ok(AcpSession::capabilities(self))
    }

    async fn session_id(&self) -> SessionId {
        AcpSession::session_id(self).clone()
    }

    async fn session_info(&self) -> SessionInfo {
        AcpSession::session_info(self)
    }

    async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        AcpSession::stability_counters(self).await
    }

    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()> {
        let required = option.ends_with('!');
        let option_name = option.trim_end_matches('!');
        AcpSession::set_config(self, option_name, value, required)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        AcpSession::cancel_prompt(self, prompt, Some("User cancelled"))
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        AcpSession::health_check(self)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn attach_terminal(&self, cols: u16, rows: u16) -> AdapterResult<Option<TerminalId>> {
        AcpSession::attach_terminal(self, cols, rows)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn send_terminal_input(
        &self,
        terminal: &TerminalId,
        input: Vec<u8>,
    ) -> AdapterResult<()> {
        if AcpSession::terminal_manager(self)
            .get_terminal(terminal)
            .await
            .is_none()
        {
            return Err(AdapterError::unsupported_operation(format!(
                "Terminal {} not found in this session",
                terminal
            )));
        }
        let mut peer_guard = AcpSession::peer(self).lock().await;
        let peer = peer_guard.as_mut().ok_or_else(|| {
            AdapterError::new(AdapterErrorKind::TransportClosed, "ACP peer not running")
        })?;
        AcpSession::terminal_manager(self)
            .send_input(peer.as_mut(), terminal, input)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn resolve_permission(&self, permission_id: &str, approved: bool) -> AdapterResult<()> {
        let decision = if approved {
            PermissionDecision::Approved
        } else {
            PermissionDecision::Denied
        };
        match AcpSession::resolve_permission(self, permission_id, decision).await {
            PermissionResolution::Resolved => Ok(()),
            PermissionResolution::Expired => Err(AdapterError::new(
                AdapterErrorKind::TimedOut,
                format!(
                    "Permission request {} already expired or timed out before it could be resolved",
                    permission_id
                ),
            )),
            PermissionResolution::NotFound => Err(AdapterError::new(
                AdapterErrorKind::SendFailed,
                format!("Permission request {} not found", permission_id),
            )),
            PermissionResolution::InvalidDecision => Err(AdapterError::new(
                AdapterErrorKind::UnsupportedOperation,
                format!(
                    "Permission request {} cannot be resolved with a non-terminal decision",
                    permission_id
                ),
            )),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigManager, permissions::PermissionManager, protocol::JsonRpcRequest,
        terminals::TerminalManager,
    };
    use brehon_types::{AgentId, MessageKind, ToolCallStreaming};

    fn test_capabilities() -> AgentCapabilities {
        AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: true,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::None,
        }
    }

    fn test_inner(
        event_tx: Option<mpsc::Sender<SessionEvent>>,
        permission_timeout: Duration,
        policy: PermissionPolicy,
    ) -> Arc<AcpSessionInner> {
        let capabilities = test_capabilities();
        Arc::new(AcpSessionInner {
            session_id: SessionId::new("session-1"),
            remote_session_id: "remote-1".to_string(),
            spec: SessionSpec::new(
                AgentId::new("agent-1"),
                "worker".to_string(),
                ".".to_string(),
            ),
            capabilities: capabilities.clone(),
            peer: Mutex::new(None),
            pending_requests: Mutex::new(HashMap::new()),
            pending_prompt_responses: Mutex::new(HashMap::new()),
            prompt_response_receivers: Mutex::new(HashMap::new()),
            blocked_sends: AtomicUsize::new(0),
            prompt_result_tokens_used: AtomicU64::new(0),
            usage_update_tokens_used: AtomicU64::new(0),
            active_prompt_token_attribution: Mutex::new(None),
            event_tx: StdMutex::new(event_tx),
            alive: AtomicBool::new(true),
            terminal_manager: super::super::terminals::TerminalManager::new(),
            transcript_buffer: TranscriptBuffer::new(10),
            permission_manager: super::super::permissions::PermissionManager::new(policy),
            permission_timeout,
            config_manager: super::super::config::ConfigManager::new(&capabilities),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
            active_prompt_id: Mutex::new(None),
        })
    }

    const LONG_LIVED_TEST_HELPER_ENV: &str = "BREHON_ACP_LONG_LIVED_TEST_HELPER";

    #[test]
    #[ignore = "Spawned by test_session_with_long_lived_process as a helper child"]
    fn long_lived_process_helper() {
        if std::env::var_os(LONG_LIVED_TEST_HELPER_ENV).is_none() {
            return;
        }

        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }

    async fn test_session_with_long_lived_process() -> Arc<AcpSession> {
        let current_exe = std::env::current_exe().expect("test binary path should resolve");
        let process = AgentProcess::spawn_with_env(
            current_exe
                .to_str()
                .expect("test binary path should be valid UTF-8"),
            &[
                "long_lived_process_helper".to_string(),
                "--ignored".to_string(),
                "--nocapture".to_string(),
            ],
            std::env::temp_dir()
                .to_str()
                .expect("temp dir should be valid UTF-8"),
            &[(LONG_LIVED_TEST_HELPER_ENV.to_string(), "1".to_string())],
        )
        .await
        .expect("long-lived helper process should spawn");

        let capabilities = AgentCapabilities {
            content_block_types: vec!["text".into()],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::None,
        };

        let spec = SessionSpec::new(
            AgentId::new("test-agent"),
            "worker".to_string(),
            std::env::temp_dir()
                .to_str()
                .expect("temp dir should be valid UTF-8")
                .to_string(),
        );

        let session = Arc::new(AcpSession {
            inner: Arc::new(AcpSessionInner {
                session_id: SessionId::new("session-under-test"),
                remote_session_id: "remote-session".to_string(),
                spec,
                capabilities: capabilities.clone(),
                peer: Mutex::new(Some(Box::new(SubprocessAcpPeer::new(process)))),
                pending_requests: Mutex::new(HashMap::new()),
                pending_prompt_responses: Mutex::new(HashMap::new()),
                prompt_response_receivers: Mutex::new(HashMap::new()),
                blocked_sends: AtomicUsize::new(0),
                prompt_result_tokens_used: AtomicU64::new(0),
                usage_update_tokens_used: AtomicU64::new(0),
                active_prompt_token_attribution: Mutex::new(None),
                event_tx: StdMutex::new(None),
                alive: AtomicBool::new(true),
                terminal_manager: TerminalManager::new(),
                transcript_buffer: TranscriptBuffer::new(32),
                permission_manager: PermissionManager::new(PermissionPolicy::default()),
                permission_timeout: Duration::from_secs(1),
                config_manager: ConfigManager::new(&capabilities),
                shutdown: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
                active_prompt_id: Mutex::new(None),
            }),
            created_at: chrono::Utc::now(),
        });

        // Spawn the session reader and track its handle, mirroring the
        // production path in `spawn_with_env_and_channel`.
        let reader_handle = spawn_reader(Arc::clone(&session.inner));
        *session.inner.reader_handle.lock().await = Some(reader_handle);

        session
    }

    #[tokio::test]
    async fn test_connect_unix_socket_completes_initialize_and_session_new() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir_in("/tmp").expect("tempdir should be created");
        let socket_path = tmp.path().join("acp.sock");
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
                let request = match super::super::protocol::parse_message(line.trim_end())
                    .expect("request should parse")
                {
                    JsonRpcMessage::Request(request) => request,
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
                        "sessionId": "sidecar-session"
                    }),
                    _ => unreachable!(),
                };
                let response = super::super::protocol::JsonRpcResponse::success(request.id, result);
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

        let spec = SessionSpec::new(
            AgentId::new("sidecar-agent"),
            "worker".to_string(),
            std::env::temp_dir()
                .to_str()
                .expect("temp dir should be valid UTF-8")
                .to_string(),
        );
        let session = AcpSession::connect_unix_socket_with_channel(
            spec,
            &socket_path,
            &[],
            None,
            SessionConfig {
                init_timeout: Duration::from_secs(1),
                ..SessionConfig::default()
            },
        )
        .await
        .expect("unix socket session should initialize");

        assert_eq!(session.inner.remote_session_id, "sidecar-session");
        assert!(session.capabilities().permission_support);
        assert_eq!(
            session.capabilities().tool_call_streaming,
            ToolCallStreaming::Full
        );

        session.kill().await.expect("session should shut down");
        server.await.expect("server task should finish");
    }

    #[test]
    fn test_permission_response_for_approved_decision_prefers_allow_option() {
        let response = permission_response_for_decision(
            Some(&serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
            PermissionDecision::Approved,
        );

        assert_eq!(
            response,
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once",
                }
            })
        );
    }

    #[test]
    fn test_permission_response_for_denied_decision_prefers_deny_option() {
        let response = permission_response_for_decision(
            Some(&serde_json::json!({
                "options": [
                    { "optionId": "allow_once", "kind": "allow_once" },
                    { "optionId": "cancel", "kind": "deny" }
                ]
            })),
            PermissionDecision::Denied,
        );

        assert_eq!(
            response,
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );
    }

    #[tokio::test]
    async fn test_permission_request_waits_for_explicit_decision() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let inner = test_inner(
            Some(event_tx),
            Duration::from_secs(1),
            PermissionPolicy::default(),
        );
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let request_id = request.id.clone();

        let mut response_task = tokio::spawn({
            let inner = Arc::clone(&inner);
            async move { response_for_agent_request(&inner, &request).await }
        });

        let event = event_rx
            .recv()
            .await
            .expect("permission event should be emitted");
        match event {
            SessionEvent::PermissionRequest { permission_id, .. } => {
                assert_eq!(permission_id, request_id);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut response_task)
                .await
                .is_err()
        );

        assert_eq!(
            inner
                .permission_manager
                .resolve(request_id.as_str(), PermissionDecision::Approved)
                .await,
            PermissionResolution::Resolved
        );

        let response = response_task.await.expect("task should complete");
        assert_eq!(
            response.result.expect("permission response should succeed"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once",
                }
            })
        );
        assert!(matches!(
            event_rx.recv().await,
            Some(SessionEvent::PermissionResolved {
                permission_id,
                approved: true,
                ..
            }) if permission_id == request_id
        ));
    }

    #[tokio::test]
    async fn test_permission_request_uses_timeout_policy_when_unresolved() {
        let (event_tx, _event_rx) = mpsc::channel(1);
        let inner = test_inner(
            Some(event_tx),
            Duration::from_millis(10),
            PermissionPolicy::default(),
        );
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let response = response_for_agent_request(&inner, &request).await;

        assert_eq!(
            response.result.expect("permission response should succeed"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );
    }

    #[tokio::test]
    async fn test_permission_request_uses_timeout_policy_when_event_handoff_stalls() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(SessionEvent::Progress {
                session_id: SessionId::new("session-1"),
                message: "fill the channel".to_string(),
                percent: None,
            })
            .await
            .expect("channel should accept initial event");
        let inner = test_inner(
            Some(event_tx),
            Duration::from_millis(10),
            PermissionPolicy::default(),
        );
        let request = super::super::protocol::JsonRpcRequest::new(
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );
        let request_id = request.id.clone();

        let response = response_for_agent_request(&inner, &request).await;

        assert_eq!(
            response.result.expect("permission response should succeed"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );
        assert_eq!(
            inner
                .permission_manager
                .resolve(request_id.as_str(), PermissionDecision::Approved)
                .await,
            PermissionResolution::Expired
        );

        let buffered = event_rx.recv().await.expect("buffered event should remain");
        assert!(matches!(buffered, SessionEvent::Progress { .. }));
        assert!(
            event_rx.try_recv().is_err(),
            "permission event should not be enqueued after handoff timeout"
        );
    }

    #[tokio::test]
    async fn test_send_permission_event_allows_immediate_handoff_with_zero_timeout() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let event = SessionEvent::PermissionRequest {
            session_id: SessionId::new("session-1"),
            permission_id: "perm-1".to_string(),
            action: "bash".to_string(),
            details: None,
        };

        assert!(send_permission_event(&event_tx, event, Duration::ZERO).await);
        assert!(matches!(
            event_rx.recv().await,
            Some(SessionEvent::PermissionRequest { permission_id, .. }) if permission_id == "perm-1"
        ));
    }

    #[tokio::test]
    async fn test_permission_request_zero_timeout_still_emits_event_before_timing_out() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let inner = test_inner(Some(event_tx), Duration::ZERO, PermissionPolicy::default());
        let request = super::super::protocol::JsonRpcRequest::new_with_id(
            "perm-zero-timeout",
            "requestPermission",
            Some(serde_json::json!({
                "options": [
                    { "optionId": "cancel", "kind": "deny" },
                    { "optionId": "allow_once", "kind": "allow_once" }
                ]
            })),
        );

        let response = mediate_permission_request(&inner, &request).await;

        // Zero timeout expires immediately, so the default deny timeout_decision selects "cancel".
        assert_eq!(
            response.result.expect("permission response should succeed"),
            serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "cancel",
                }
            })
        );
        assert!(matches!(
            event_rx.recv().await,
            Some(SessionEvent::PermissionRequest { permission_id, .. })
                if permission_id == "perm-zero-timeout"
        ));
        assert_eq!(
            inner
                .permission_manager
                .resolve("perm-zero-timeout", PermissionDecision::Approved)
                .await,
            PermissionResolution::Expired
        );
    }

    #[tokio::test]
    async fn test_await_permission_decision_allows_ready_value_with_zero_timeout() {
        let (response_tx, response_rx) = oneshot::channel();
        response_tx
            .send(PermissionDecision::Approved)
            .expect("receiver should still be alive");

        assert_eq!(
            await_permission_decision(response_rx, Duration::ZERO).await,
            Some(PermissionDecision::Approved)
        );
    }

    #[tokio::test]
    async fn test_response_for_unknown_agent_request_is_method_not_found() {
        let inner = test_inner(None, Duration::from_millis(10), PermissionPolicy::default());
        let response = response_for_agent_request(
            &inner,
            &super::super::protocol::JsonRpcRequest::new("unknownMethod", None),
        )
        .await;

        assert_eq!(response.error.unwrap().code, -32601);
    }

    fn test_session() -> AcpSession {
        AcpSession {
            inner: Arc::new(AcpSessionInner {
                session_id: SessionId::new("sess-test"),
                remote_session_id: "remote".to_string(),
                spec: SessionSpec::new(
                    brehon_types::AgentId::new("agent"),
                    "worker".into(),
                    ".".into(),
                ),
                capabilities: AgentCapabilities {
                    content_block_types: vec![],
                    session_config_options: vec![],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: brehon_types::ToolCallStreaming::None,
                },
                peer: Mutex::new(None),
                pending_requests: Mutex::new(HashMap::new()),
                pending_prompt_responses: Mutex::new(HashMap::new()),
                prompt_response_receivers: Mutex::new(HashMap::new()),
                blocked_sends: AtomicUsize::new(0),
                prompt_result_tokens_used: AtomicU64::new(0),
                usage_update_tokens_used: AtomicU64::new(0),
                active_prompt_id: Mutex::new(None),
                active_prompt_token_attribution: Mutex::new(None),
                event_tx: StdMutex::new(None),
                alive: AtomicBool::new(true),
                terminal_manager: super::super::terminals::TerminalManager::new(),
                transcript_buffer: TranscriptBuffer::new(128),
                permission_manager: super::super::permissions::PermissionManager::new(
                    PermissionPolicy::default(),
                ),
                permission_timeout: Duration::from_secs(1),
                config_manager: super::super::config::ConfigManager::new(&AgentCapabilities {
                    content_block_types: vec![],
                    session_config_options: vec![],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: brehon_types::ToolCallStreaming::None,
                }),
                shutdown: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
            }),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_usage_tokens_from_update_accepts_total_usage() {
        let update = serde_json::json!({
            "sessionUpdate": "usage_update",
            "usage": {
                "totalTokens": 1234
            }
        });

        assert_eq!(usage_tokens_from_update(&update), Some(1234));
    }

    #[test]
    fn test_usage_tokens_from_update_sums_known_parts() {
        let update = serde_json::json!({
            "sessionUpdate": "usage_update",
            "usage": {
                "inputTokens": 100,
                "outputTokens": 50,
                "cacheReadInputTokens": 25,
                "reasoningTokens": "5"
            }
        });

        assert_eq!(usage_tokens_from_update(&update), Some(180));
    }

    #[test]
    fn test_claim_active_prompt_slot_rejects_overlap() {
        let mut active = None;
        claim_active_prompt_slot(&mut active, "prompt-1").expect("first prompt should claim slot");
        let err = claim_active_prompt_slot(&mut active, "prompt-2")
            .expect_err("second prompt should be rejected");
        assert!(matches!(err, SessionError::PromptInProgress(existing) if existing == "prompt-1"));
    }

    #[test]
    fn test_clear_active_prompt_slot_ignores_nonmatching_prompt() {
        let mut active = Some("prompt-1".to_string());
        clear_active_prompt_slot(&mut active, "prompt-2");
        assert_eq!(active.as_deref(), Some("prompt-1"));
        clear_active_prompt_slot(&mut active, "prompt-1");
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn test_remove_pending_request_returns_count_without_deadlocking() {
        let (tx, _rx) = oneshot::channel();
        let pending_requests = Mutex::new(HashMap::from([("request-1".to_string(), tx)]));

        let (waiter, remaining) = tokio::time::timeout(
            Duration::from_millis(100),
            remove_pending_request(&pending_requests, "request-1"),
        )
        .await
        .expect("remove_pending_request should not deadlock");

        assert!(waiter.is_some());
        assert_eq!(remaining, 0);
    }

    // --- Event-driven prompt wait tests ---

    #[tokio::test]
    async fn test_oneshot_prompt_response_completes() {
        let (tx, rx) = oneshot::channel();

        let prompt_result: Result<crate::acp_types::PromptResult, String> =
            Ok(crate::acp_types::PromptResult {
                response: Some("hello".to_string()),
                tokens_used: Some(42),
                stop_reason: Some("end_turn".to_string()),
            });

        tx.send(prompt_result).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx).await;

        match result {
            Ok(Ok(inner)) => {
                let pr = inner.unwrap();
                assert_eq!(pr.response.as_deref(), Some("hello"));
                assert_eq!(pr.tokens_used, Some(42));
            }
            _ => panic!("Expected successful oneshot completion"),
        }
    }

    #[tokio::test]
    async fn test_oneshot_prompt_response_timeout() {
        let (_tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx).await;

        assert!(result.is_err(), "Should time out when no sender completes");
    }

    #[tokio::test]
    async fn test_oneshot_prompt_response_sender_dropped() {
        let (tx, rx) = oneshot::channel::<Result<crate::acp_types::PromptResult, String>>();
        drop(tx);

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx).await;

        match result {
            Ok(Err(_)) => {}
            _ => panic!("Expected RecvError when sender is dropped"),
        }
    }

    #[tokio::test]
    async fn test_oneshot_cleanup_removes_pending_entry() {
        let mut senders: HashMap<
            String,
            oneshot::Sender<Result<crate::acp_types::PromptResult, String>>,
        > = HashMap::new();
        let mut receivers: HashMap<
            String,
            oneshot::Receiver<Result<crate::acp_types::PromptResult, String>>,
        > = HashMap::new();

        for id in &["p-1", "p-2"] {
            let (tx, rx) = oneshot::channel();
            senders.insert(id.to_string(), tx);
            receivers.insert(id.to_string(), rx);
        }

        senders
            .remove("p-1")
            .unwrap()
            .send(Ok(crate::acp_types::PromptResult::default()))
            .unwrap();
        senders.clear();

        let rx1 = receivers.remove("p-1").unwrap();
        let r1 = tokio::time::timeout(std::time::Duration::from_millis(50), rx1).await;
        assert!(r1.is_ok() && r1.unwrap().is_ok());

        let rx2 = receivers.remove("p-2").unwrap();
        let r2 = tokio::time::timeout(std::time::Duration::from_millis(50), rx2).await;
        assert!(r2.is_ok() && r2.unwrap().is_err());
    }

    mod runtime_safety_tests {
        use super::*;
        include!("session_runtime_safety_tests.rs");
    }
}
