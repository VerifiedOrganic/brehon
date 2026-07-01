use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, warn};

use brehon_adapter_sdk::process::AgentProcess;
use brehon_adapter_sdk::protocol::{JsonRpcError, JsonRpcNotification, JsonRpcRequest};
use brehon_adapter_sdk::session_event::{session_event_to_adapter_event, SessionEvent};
use brehon_adapter_sdk::stability_runtime::{
    brehon_root_from_env, clear_session_snapshot, persist_session_snapshot,
    schedule_clear_session_snapshot, schedule_persist_session_snapshot,
};
use brehon_adapter_sdk::{
    AdapterError, AdapterErrorKind, AdapterEvent, AdapterResult, AgentAdapter, PromptResult,
};
use brehon_types::{
    AdapterKind, AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId,
    SessionInfo, SessionSpec, StabilityCounters, TerminalId, ToolCallStreaming,
};

type CodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type CodexSink = SplitSink<CodexSocket, Message>;
type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<CodexResponse>>>>;

const CODEX_PROTOCOL_VERSION: &str = "2025-03-26";
const CODEX_PERMISSION_PROFILE_ENV: &str = "CODEX_PERMISSION_PROFILE";
const CONNECT_TIMEOUT_MS: u64 = 8_000;
const CONNECT_ATTEMPT_TIMEOUT_MS: u64 = 1_000;
const REQUEST_TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CodexError {
    #[error("failed to spawn codex app-server: {0}")]
    Spawn(String),
    #[error("websocket error: {0}")]
    WebSocket(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("session not running")]
    #[allow(dead_code)]
    NotRunning,
    #[error("request timeout")]
    Timeout,
}

pub struct CodexWsSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    launch_config: CodexLaunchConfig,
    capabilities: AgentCapabilities,
    process: Mutex<Option<AgentProcess>>,
    writer: Mutex<CodexSink>,
    pending_requests: PendingRequests,
    thread_id: Mutex<Option<String>>,
    streamed_agent_messages: Mutex<HashSet<String>>,
    mcp_server_statuses: Mutex<HashMap<String, CodexMcpServerStatus>>,
    event_tx: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    alive: AtomicBool,
    /// Shutdown flag: when set, the reader loop exits after completing the
    /// current websocket message rather than waiting indefinitely. The
    /// websocket writer is closed during `kill()` to unblock a stalled read.
    shutdown: AtomicBool,
    /// Tracked JoinHandle for the websocket reader task, enabling deterministic
    /// cancellation and await during session shutdown.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    /// Broadcast channel for adapter events, bridged from the inner SessionEvent
    /// stream so that `AgentAdapter::events()` can subscribe to live events.
    adapter_event_broadcast: std::sync::Mutex<Option<broadcast::Sender<AdapterEvent>>>,
    /// Active prompt ID for correlating prompt lifecycle state.
    active_prompt_id: Mutex<Option<String>>,
    /// Active turn ID associated with the active prompt.
    active_prompt_turn_id: Mutex<Option<String>>,
    /// Conservative task attribution for token persistence.
    active_prompt_token_attribution: Mutex<Option<ActivePromptTokenAttribution>>,
    /// Cumulative provider-reported token usage for this session.
    tokens_used: AtomicU64,
    /// Pending prompt-response oneshot senders, keyed by prompt ID.
    ///
    /// `send_prompt` registers a sender before writing to the agent so the
    /// reader can complete the prompt without polling. Entries are removed on
    /// completion, timeout, send failure, and session shutdown.
    pending_prompt_responses: Mutex<HashMap<String, oneshot::Sender<PromptResult>>>,
    /// Receivers paired with `pending_prompt_responses`, retrieved by
    /// `wait_for_response` using the prompt ID from `send_prompt`.
    prompt_response_receivers: Mutex<HashMap<String, oneshot::Receiver<PromptResult>>>,
}

pub struct CodexWsSession {
    inner: Arc<CodexWsSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
struct ActivePromptTokenAttribution {
    prompt_id: String,
    task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CodexInboundMessage {
    Request(CodexRequest),
    Response(CodexResponse),
    Notification(CodexNotification),
}

#[derive(Debug, Clone, Deserialize)]
struct CodexResponse {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: Option<String>,
    id: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexNotification {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: Option<String>,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexRequest {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: Option<String>,
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct CodexLaunchConfig {
    model: Option<String>,
    approval_policy: String,
    thread_sandbox: String,
    turn_sandbox_policy: serde_json::Value,
    extra_writable_roots: Vec<String>,
    config: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexMcpServerStatus {
    status: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexPermissionProfile {
    Observe,
    Dependency,
    Workspace,
    Reviewer,
    Operator,
    Unsafe,
}

impl CodexPermissionProfile {
    fn from_env_or_role(env: &[(String, String)]) -> Self {
        env.iter()
            .rev()
            .find_map(|(key, value)| {
                (key == CODEX_PERMISSION_PROFILE_ENV)
                    .then(|| Self::from_env_value(value))
                    .flatten()
            })
            .or_else(|| {
                env.iter().rev().find_map(|(key, value)| {
                    (key == "BREHON_AGENT_ROLE").then(|| Self::from_role(value))
                })
            })
            .unwrap_or(Self::Workspace)
    }

    fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "observe" => Some(Self::Observe),
            "dependency" => Some(Self::Dependency),
            "workspace" => Some(Self::Workspace),
            "reviewer" => Some(Self::Reviewer),
            "operator" => Some(Self::Operator),
            "unsafe" => Some(Self::Unsafe),
            _ => None,
        }
    }

    fn from_role(role: &str) -> Self {
        match role.trim() {
            "supervisor" => Self::Operator,
            "reviewer" => Self::Reviewer,
            "advisor" => Self::Observe,
            "research" => Self::Observe,
            _ => Self::Workspace,
        }
    }

    fn default_sandbox(self) -> &'static str {
        match self {
            Self::Observe | Self::Dependency => "read-only",
            Self::Workspace | Self::Reviewer | Self::Operator => "workspace-write",
            Self::Unsafe => "danger-full-access",
        }
    }
}

fn sandbox_policy_for_mode(mode: &str) -> serde_json::Value {
    match mode {
        "danger-full-access" => serde_json::json!({ "type": "dangerFullAccess" }),
        "workspace-write" => serde_json::json!({ "type": "workspaceWrite" }),
        "read-only" => serde_json::json!({ "type": "readOnly" }),
        other => serde_json::json!({ "type": other }),
    }
}

impl CodexLaunchConfig {
    fn from_profile(profile: CodexPermissionProfile) -> Self {
        let thread_sandbox = profile.default_sandbox().to_string();
        Self {
            model: None,
            approval_policy: "never".to_string(),
            turn_sandbox_policy: sandbox_policy_for_mode(&thread_sandbox),
            thread_sandbox,
            extra_writable_roots: Vec::new(),
            config: serde_json::Map::new(),
        }
    }
}

impl Default for CodexLaunchConfig {
    fn default() -> Self {
        Self::from_profile(CodexPermissionProfile::Workspace)
    }
}

impl CodexWsSession {
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
    ) -> Result<Self, CodexError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let ws_url = allocate_ws_url()?;
        let launch_config = extract_codex_launch_config(args, env);

        let mut spawn_args = args.to_vec();
        spawn_args.push("--listen".to_string());
        spawn_args.push(ws_url.clone());

        let process = AgentProcess::spawn_with_env(command, &spawn_args, &spec.worktree_path, env)
            .await
            .map_err(|e| CodexError::Spawn(e.to_string()))?;

        let socket = connect_with_retry(&ws_url).await?;
        let (writer, reader) = socket.split();
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));

        let (adapter_event_tx, _) = broadcast::channel(64);
        let adapter_event_broadcast = std::sync::Mutex::new(Some(adapter_event_tx));

        // Bridge SessionEvent stream to AdapterEvent broadcast so that
        // AgentAdapter::events() can receive live events.
        let (internal_event_tx, mut internal_event_rx) = mpsc::channel::<SessionEvent>(256);
        let bridge_broadcast_tx = adapter_event_broadcast
            .lock()
            .expect("adapter_event_broadcast should not be poisoned")
            .clone()
            .expect("adapter_event_broadcast just initialized");
        let bridge_external_tx = event_tx;
        tokio::spawn(async move {
            while let Some(session_event) = internal_event_rx.recv().await {
                if let Some(ref tx) = bridge_external_tx {
                    let _ = tx.send(session_event.clone()).await;
                }
                if let Some(adapter_event) = session_event_to_adapter_event(session_event) {
                    let _ = bridge_broadcast_tx.send(adapter_event);
                }
            }
        });

        let inner = Arc::new(CodexWsSessionInner {
            session_id: session_id.clone(),
            spec: spec.clone(),
            launch_config,
            capabilities: AgentCapabilities {
                content_block_types: vec!["text".to_string(), "image".to_string()],
                session_config_options: vec![],
                permission_support: false,
                terminal_support: false,
                tool_call_streaming: ToolCallStreaming::Basic,
            },
            process: Mutex::new(Some(process)),
            writer: Mutex::new(writer),
            pending_requests: Arc::clone(&pending_requests),
            thread_id: Mutex::new(None),
            streamed_agent_messages: Mutex::new(HashSet::new()),
            mcp_server_statuses: Mutex::new(HashMap::new()),
            event_tx: Mutex::new(Some(internal_event_tx)),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
            adapter_event_broadcast,
            active_prompt_id: Mutex::new(None),
            active_prompt_turn_id: Mutex::new(None),
            active_prompt_token_attribution: Mutex::new(None),
            tokens_used: AtomicU64::new(0),
            pending_prompt_responses: Mutex::new(HashMap::new()),
            prompt_response_receivers: Mutex::new(HashMap::new()),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner), pending_requests, reader);
        *inner.reader_handle.lock().await = Some(reader_handle);
        persist_session_snapshot(session_id.as_str(), StabilityCounters::default());

        let session = Self { inner, created_at };
        session.initialize().await?;
        session.start_thread().await?;
        Ok(session)
    }

    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities.clone()
    }

    pub async fn stability_counters(&self) -> StabilityCounters {
        StabilityCounters {
            pending_requests: self.inner.pending_requests.lock().await.len(),
            pending_prompt_waiters: self.inner.pending_prompt_responses.lock().await.len(),
            tokens_used: self.inner.tokens_used.load(Ordering::Relaxed),
            ..Default::default()
        }
    }

    fn persist_runtime_stability(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                StabilityCounters {
                    pending_requests: inner.pending_requests.lock().await.len(),
                    pending_prompt_waiters: inner.pending_prompt_responses.lock().await.len(),
                    tokens_used: inner.tokens_used.load(Ordering::Relaxed),
                    ..Default::default()
                },
            );
        });
    }

    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, CodexError> {
        let thread_id = self
            .inner
            .thread_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| CodexError::Protocol("Codex thread not initialized".to_string()))?;

        let prompt_id = prompt.prompt_id.as_str().to_string();
        {
            let mut active_prompt = self.inner.active_prompt_id.lock().await;
            claim_active_prompt_slot(&mut active_prompt, &prompt_id)?;
        }
        set_prompt_token_attribution(&self.inner, &prompt_id, &prompt.content).await;

        let (tx, rx) = oneshot::channel();
        // Ordering invariant: sender and receiver are registered before the
        // websocket receives turn/start. The reader loop cannot observe a
        // turn/completed for this prompt until the request is sent below, so
        // there is no race between registration and completion.
        self.inner
            .pending_prompt_responses
            .lock()
            .await
            .insert(prompt_id.clone(), tx);
        self.inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(prompt_id.clone(), rx);
        self.persist_runtime_stability();

        let params = build_turn_start_params(
            &thread_id,
            &self.inner.spec.worktree_path,
            &prompt.content,
            &self.inner.launch_config,
        );

        let response = self.send_request("turn/start", Some(params)).await;
        if let Err(err) = &response {
            self.cleanup_prompt_registration(&prompt_id).await;
            return Err(err.clone());
        }
        let response = response.unwrap();
        if let Some(err) = response.error {
            self.cleanup_prompt_registration(&prompt_id).await;
            return Err(CodexError::Protocol(err.message));
        }
        let turn_id = match turn_id_from_turn_start_response(&response) {
            Some(turn_id) => turn_id,
            None => {
                self.cleanup_prompt_registration(&prompt_id).await;
                return Err(CodexError::Protocol(
                    "Codex turn/start missing turn.id".to_string(),
                ));
            }
        };
        {
            let mut active_prompt_turn_id = self.inner.active_prompt_turn_id.lock().await;
            active_prompt_turn_id.replace(turn_id);
        }

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at: prompt.sent_at,
        })
    }

    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<brehon_adapter_sdk::PromptResult, CodexError> {
        let receiver = self
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .remove(prompt_id.as_str())
            .ok_or_else(|| {
                CodexError::Protocol(format!(
                    "No pending prompt response for {}",
                    prompt_id.as_str()
                ))
            })?;

        match timeout(Duration::from_millis(timeout_ms), receiver).await {
            Ok(Ok(result)) => {
                self.persist_runtime_stability();
                Ok(result)
            }
            Ok(Err(_)) => {
                let mut active_prompt = self.inner.active_prompt_id.lock().await;
                let mut active_prompt_turn_id = self.inner.active_prompt_turn_id.lock().await;
                if clear_active_prompt_state(&mut active_prompt, prompt_id.as_str()) {
                    active_prompt_turn_id.take();
                }
                self.inner
                    .pending_prompt_responses
                    .lock()
                    .await
                    .remove(prompt_id.as_str());
                self.persist_runtime_stability();
                Err(CodexError::Protocol(format!(
                    "Prompt response channel closed for {}",
                    prompt_id.as_str()
                )))
            }
            Err(_) => {
                let mut active_prompt = self.inner.active_prompt_id.lock().await;
                let mut active_prompt_turn_id = self.inner.active_prompt_turn_id.lock().await;
                if clear_active_prompt_state(&mut active_prompt, prompt_id.as_str()) {
                    active_prompt_turn_id.take();
                }
                self.inner
                    .pending_prompt_responses
                    .lock()
                    .await
                    .remove(prompt_id.as_str());
                self.persist_runtime_stability();
                Err(CodexError::Timeout)
            }
        }
    }

    async fn cleanup_prompt_registration(&self, prompt_id: &str) {
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
        let mut active_prompt = self.inner.active_prompt_id.lock().await;
        let mut active_prompt_turn_id = self.inner.active_prompt_turn_id.lock().await;
        if clear_active_prompt_state(&mut active_prompt, prompt_id) {
            active_prompt_turn_id.take();
        }
        clear_prompt_token_attribution(&self.inner, prompt_id).await;
        self.persist_runtime_stability();
    }

    pub async fn cancel_prompt(&self, _prompt_id: &PromptId) -> Result<(), CodexError> {
        Err(CodexError::Protocol(
            "Codex websocket prompt cancellation is not implemented".to_string(),
        ))
    }

    pub async fn set_config(&self, _option: &str, _value: &str) -> Result<(), CodexError> {
        Ok(())
    }

    /// Terminates the Codex session and awaits all spawned work for
    /// deterministic shutdown.
    pub async fn kill(&self) -> Result<(), CodexError> {
        self.inner.alive.store(false, Ordering::SeqCst);
        self.inner.shutdown.store(true, Ordering::SeqCst);

        // Close the websocket writer to unblock the reader promptly.
        {
            let mut writer = self.inner.writer.lock().await;
            let _ = writer.close().await;
        }

        let mut process = self.inner.process.lock().await;
        let result = if let Some(proc) = process.take() {
            proc.kill()
                .await
                .map_err(|e| CodexError::Spawn(e.to_string()))
        } else {
            Ok(())
        };
        drop(process);

        // Await the reader task with a bounded timeout for deterministic shutdown.
        let reader_handle = self.inner.reader_handle.lock().await.take();
        if let Some(handle) = reader_handle {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        self.inner.pending_prompt_responses.lock().await.clear();
        self.inner.prompt_response_receivers.lock().await.clear();
        self.inner.active_prompt_id.lock().await.take();
        self.inner.active_prompt_turn_id.lock().await.take();
        self.inner
            .active_prompt_token_attribution
            .lock()
            .await
            .take();

        clear_session_snapshot(self.inner.session_id.as_str());
        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, CodexError> {
        let process = self.inner.process.lock().await;
        let process_alive = process
            .as_ref()
            .map(|proc| proc.is_alive())
            .unwrap_or(false)
            && self.inner.alive.load(Ordering::SeqCst);
        drop(process);

        if !process_alive {
            return Ok(HealthStatus::Unhealthy);
        }

        let mcp_statuses = self.inner.mcp_server_statuses.lock().await;
        Ok(if codex_brehon_mcp_is_unhealthy(&mcp_statuses) {
            HealthStatus::Unhealthy
        } else {
            HealthStatus::Healthy
        })
    }

    pub fn session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.inner.session_id.clone(),
            agent_id: self.inner.spec.agent_id.clone(),
            role: self.inner.spec.role.clone(),
            health: if self.inner.alive.load(Ordering::SeqCst) {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            created_at: self.created_at,
            capabilities: self.inner.capabilities.clone(),
        }
    }

    async fn initialize(&self) -> Result<(), CodexError> {
        let params = serde_json::json!({
            "clientInfo": {
                "name": "brehon",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "cwd": self.inner.spec.worktree_path,
            "protocolVersion": CODEX_PROTOCOL_VERSION,
        });
        let response = self.send_request("initialize", Some(params)).await?;
        if let Some(err) = response.error {
            return Err(CodexError::Protocol(err.message));
        }

        self.send_notification("initialized", None).await?;
        Ok(())
    }

    async fn start_thread(&self) -> Result<(), CodexError> {
        let params =
            build_thread_start_params(&self.inner.spec.worktree_path, &self.inner.launch_config);
        let response = self.send_request("thread/start", Some(params)).await?;
        if let Some(err) = response.error {
            return Err(CodexError::Protocol(err.message));
        }

        let thread_id = response
            .result
            .as_ref()
            .and_then(|result| result.get("thread"))
            .and_then(|thread| thread.get("id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                CodexError::Protocol("Codex thread/start missing thread.id".to_string())
            })?
            .to_string();

        *self.inner.thread_id.lock().await = Some(thread_id);
        Ok(())
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<CodexResponse, CodexError> {
        let request = JsonRpcRequest::new(method, params);
        let id = request.id.clone();
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending_requests
            .lock()
            .await
            .insert(id.clone(), tx);
        self.persist_runtime_stability();

        let payload = brehon_adapter_sdk::protocol::serialize_request(&request)
            .map_err(|e| CodexError::Protocol(e.message))?;

        {
            let mut writer = self.inner.writer.lock().await;
            if let Err(err) = writer.send(Message::Text(payload.into())).await {
                self.inner.pending_requests.lock().await.remove(&id);
                self.persist_runtime_stability();
                return Err(CodexError::WebSocket(err.to_string()));
            }
        }

        match timeout(Duration::from_millis(REQUEST_TIMEOUT_MS), rx).await {
            Ok(Ok(response)) => {
                self.persist_runtime_stability();
                Ok(response)
            }
            Ok(Err(_)) => {
                self.persist_runtime_stability();
                Err(CodexError::Protocol(format!(
                    "Codex request channel closed for {id}"
                )))
            }
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&id);
                self.persist_runtime_stability();
                Err(CodexError::Timeout)
            }
        }
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), CodexError> {
        let notification = JsonRpcNotification::new(method, params);
        let payload = brehon_adapter_sdk::protocol::serialize_notification(&notification)
            .map_err(|e| CodexError::Protocol(e.message))?;

        let mut writer = self.inner.writer.lock().await;
        writer
            .send(Message::Text(payload.into()))
            .await
            .map_err(|e| CodexError::WebSocket(e.to_string()))?;
        Ok(())
    }
}

fn allocate_ws_url() -> Result<String, CodexError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| CodexError::Spawn(format!("Failed to allocate websocket port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CodexError::Spawn(format!("Failed to read websocket port: {e}")))?
        .port();
    drop(listener);
    Ok(format!("ws://127.0.0.1:{port}"))
}

async fn connect_with_retry(ws_url: &str) -> Result<CodexSocket, CodexError> {
    connect_with_retry_with_timeouts(
        ws_url,
        Duration::from_millis(CONNECT_TIMEOUT_MS),
        Duration::from_millis(CONNECT_ATTEMPT_TIMEOUT_MS),
    )
    .await
}

async fn connect_with_retry_with_timeouts(
    ws_url: &str,
    total_timeout: Duration,
    attempt_timeout: Duration,
) -> Result<CodexSocket, CodexError> {
    let deadline = tokio::time::Instant::now() + total_timeout;
    let mut last_error = None;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let reason = last_error
                .take()
                .unwrap_or_else(|| "timed out waiting for websocket handshake".to_string());
            return Err(CodexError::WebSocket(format!(
                "failed to connect to {ws_url}: {reason}"
            )));
        }

        let remaining = deadline.saturating_duration_since(now);
        let this_attempt_timeout = remaining.min(attempt_timeout);

        match timeout(this_attempt_timeout, connect_async(ws_url)).await {
            Ok(Ok((socket, _))) => return Ok(socket),
            Ok(Err(err)) => {
                last_error = Some(err.to_string());
            }
            Err(_) => {
                last_error = Some(format!(
                    "websocket handshake timed out after {} ms",
                    this_attempt_timeout.as_millis()
                ));
            }
        }

        let sleep_for = Duration::from_millis(100)
            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if !sleep_for.is_zero() {
            sleep(sleep_for).await;
        }
    }
}

fn spawn_reader(
    inner: Arc<CodexWsSessionInner>,
    pending_requests: PendingRequests,
    mut reader: SplitStream<CodexSocket>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if inner.shutdown.load(Ordering::SeqCst) {
                debug!(session_id = %inner.session_id, "Codex reader exiting due to shutdown signal");
                break;
            }
            let message = match reader.next().await {
                Some(msg) => msg,
                None => break,
            };
            let raw = match message {
                Ok(Message::Text(text)) => text.to_string(),
                Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(err) => {
                        warn!(error = %err, "Ignoring non-UTF8 Codex websocket frame");
                        continue;
                    }
                },
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => break,
                Ok(other) => {
                    debug!(message = ?other, "Ignoring Codex websocket message");
                    continue;
                }
                Err(err) => {
                    warn!(error = %err, "Codex websocket reader failed");
                    break;
                }
            };

            match parse_codex_message(&raw) {
                Ok(CodexInboundMessage::Response(response)) => {
                    let (tx, pending_count) =
                        remove_pending_request(&pending_requests, &response.id).await;
                    if let Some(tx) = tx {
                        let _ = tx.send(response);
                        schedule_persist_session_snapshot(
                            inner.session_id.as_str().to_string(),
                            StabilityCounters {
                                pending_requests: pending_count,
                                tokens_used: inner.tokens_used.load(Ordering::Relaxed),
                                ..Default::default()
                            },
                        );
                    }
                }
                Ok(CodexInboundMessage::Notification(notification)) => {
                    forward_codex_notification(&inner, notification).await;
                }
                Ok(CodexInboundMessage::Request(request)) => {
                    handle_codex_request(&inner, request).await;
                }
                Err(err) => {
                    warn!(error = ?err, raw = %raw, "Failed to parse Codex websocket message");
                }
            }
        }

        inner.alive.store(false, Ordering::SeqCst);
        inner.pending_prompt_responses.lock().await.clear();
        inner.prompt_response_receivers.lock().await.clear();
        inner.active_prompt_id.lock().await.take();
        inner.active_prompt_turn_id.lock().await.take();
        inner.active_prompt_token_attribution.lock().await.take();
        schedule_clear_session_snapshot(inner.session_id.as_str().to_string());
    })
}

async fn remove_pending_request(
    pending_requests: &PendingRequests,
    request_id: &str,
) -> (Option<oneshot::Sender<CodexResponse>>, usize) {
    let mut pending = pending_requests.lock().await;
    let tx = pending.remove(request_id);
    let remaining = pending.len();
    (tx, remaining)
}

fn claim_active_prompt_slot(
    active_prompt: &mut Option<String>,
    prompt_id: &str,
) -> Result<(), CodexError> {
    if let Some(existing) = active_prompt.as_deref() {
        return Err(CodexError::Protocol(format!(
            "Concurrent prompts are not supported by Codex websocket adapter (active prompt: {existing})"
        )));
    }
    *active_prompt = Some(prompt_id.to_string());
    Ok(())
}

fn clear_active_prompt_state(active_prompt: &mut Option<String>, prompt_id: &str) -> bool {
    if active_prompt.as_deref() == Some(prompt_id) {
        active_prompt.take();
        true
    } else {
        false
    }
}

async fn set_prompt_token_attribution(
    inner: &Arc<CodexWsSessionInner>,
    prompt_id: &str,
    prompt_content: &str,
) {
    let task_id = brehon_root_from_env().and_then(|root| {
        brehon_types::infer_task_token_target(
            &root,
            inner.spec.agent_id.as_str(),
            &inner.spec.role,
            prompt_content,
        )
    });
    let mut attribution = inner.active_prompt_token_attribution.lock().await;
    *attribution = task_id.map(|task_id| ActivePromptTokenAttribution {
        prompt_id: prompt_id.to_string(),
        task_id,
    });
}

async fn clear_prompt_token_attribution(inner: &Arc<CodexWsSessionInner>, prompt_id: &str) {
    let mut attribution = inner.active_prompt_token_attribution.lock().await;
    if attribution
        .as_ref()
        .is_some_and(|active| active.prompt_id == prompt_id)
    {
        attribution.take();
    }
}

async fn record_prompt_result_tokens(
    inner: &Arc<CodexWsSessionInner>,
    prompt_id: &str,
    tokens_used: Option<u64>,
) {
    if let Some(tokens) = tokens_used.filter(|tokens| *tokens > 0) {
        inner.tokens_used.fetch_add(tokens, Ordering::Relaxed);
        let task_id = {
            let mut attribution = inner.active_prompt_token_attribution.lock().await;
            let Some(active) = attribution.take() else {
                return;
            };
            if active.prompt_id != prompt_id {
                *attribution = Some(active);
                return;
            }
            active.task_id
        };
        persist_task_token_delta(inner, &task_id, tokens);
    } else {
        clear_prompt_token_attribution(inner, prompt_id).await;
    }
}

fn persist_task_token_delta(inner: &CodexWsSessionInner, task_id: &str, tokens_delta: u64) {
    let Some(root) = brehon_root_from_env() else {
        return;
    };
    if let Err(err) = brehon_types::record_task_token_usage(&root, task_id, tokens_delta) {
        warn!(
            session_id = %inner.session_id,
            task_id,
            tokens_delta,
            error = %err,
            "Failed to persist Codex task token usage"
        );
    }
}

fn turn_id_matches(active_turn_id: Option<&str>, completed_turn_id: Option<&str>) -> bool {
    match (active_turn_id, completed_turn_id) {
        (None, None) => true,
        (Some(active_turn_id), Some(completed_turn_id)) => active_turn_id == completed_turn_id,
        _ => false,
    }
}

async fn forward_codex_notification(
    inner: &Arc<CodexWsSessionInner>,
    notification: CodexNotification,
) {
    let mut events = Vec::new();

    match notification.method.as_str() {
        "thread/started" => events.push(SessionEvent::Progress {
            session_id: inner.session_id.clone(),
            message: "Codex thread started".to_string(),
            percent: None,
        }),
        "thread/status/changed" => {
            if let Some(params) = notification.params.as_ref() {
                let status = params
                    .get("status")
                    .and_then(|status| status.get("type"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                if status != "systemError" {
                    events.push(SessionEvent::Progress {
                        session_id: inner.session_id.clone(),
                        message: format!("Codex thread status: {status}"),
                        percent: None,
                    });
                }
            }
        }
        "error" => {
            if let Some(message) = codex_error_message(notification.params.as_ref()) {
                events.push(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message: format!("Codex error: {message}"),
                    percent: None,
                });
            }
        }
        "turn/started" => events.push(SessionEvent::OperationStarted {
            session_id: inner.session_id.clone(),
            operation: "turn".to_string(),
        }),
        "turn/completed" => {
            let success = turn_completed_success(notification.params.as_ref());
            events.push(SessionEvent::OperationCompleted {
                session_id: inner.session_id.clone(),
                operation: "turn".to_string(),
                success,
            });
            let completed_turn_id = turn_completed_turn_id(notification.params.as_ref());
            let mut handled_turn = false;
            let mut pending_prompt_waiters = 0usize;

            {
                let mut active_prompt_id = inner.active_prompt_id.lock().await;
                let mut active_prompt_turn_id = inner.active_prompt_turn_id.lock().await;
                if let Some(prompt_id) = active_prompt_id.clone() {
                    if turn_id_matches(
                        active_prompt_turn_id.as_deref(),
                        completed_turn_id.as_deref(),
                    ) {
                        let mut pending_prompt_responses =
                            inner.pending_prompt_responses.lock().await;
                        let sender = pending_prompt_responses.remove(&prompt_id);
                        pending_prompt_waiters = pending_prompt_responses.len();
                        let tokens_used = turn_completed_tokens_used(notification.params.as_ref());

                        if let Some(sender) = sender {
                            let mut result = brehon_adapter_sdk::PromptResult::default();
                            result.stop_reason = if success {
                                Some("stop".to_string())
                            } else {
                                Some("error".to_string())
                            };
                            result.response = turn_response_text(notification.params.as_ref());
                            result.tokens_used = tokens_used;
                            let _ = sender.send(result);
                        }

                        record_prompt_result_tokens(inner, &prompt_id, tokens_used).await;
                        clear_active_prompt_state(&mut active_prompt_id, &prompt_id);
                        active_prompt_turn_id.take();
                        handled_turn = true;
                    }
                }
            }

            if !handled_turn {
                return;
            }

            let pending_requests = inner.pending_requests.lock().await.len();
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                StabilityCounters {
                    pending_requests,
                    pending_prompt_waiters,
                    tokens_used: inner.tokens_used.load(Ordering::Relaxed),
                    ..Default::default()
                },
            );
        }
        "mcpServer/startupStatus/updated" => {
            if let Some(params) = notification.params.as_ref() {
                update_mcp_server_status(inner, params).await;
                let name = params
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let status = params
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let error = params
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .filter(|error| !error.is_empty());
                let message = match error {
                    Some(error) => format!("MCP server {name}: {status} ({error})"),
                    None => format!("MCP server {name}: {status}"),
                };
                events.push(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message,
                    percent: None,
                });
            }
        }
        "item/agentMessage/delta" => {
            if let Some(item_id) = item_id(notification.params.as_ref()) {
                inner.streamed_agent_messages.lock().await.insert(item_id);
            }
            if let Some(text) = agent_message_delta_text(notification.params.as_ref()) {
                events.push(SessionEvent::Output {
                    session_id: inner.session_id.clone(),
                    text,
                });
            }
        }
        "item/mcpToolCall/progress" => {
            if let Some(message) = describe_mcp_tool_progress(notification.params.as_ref()) {
                events.push(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message,
                    percent: None,
                });
            }
        }
        "item/started" => {
            if let Some(event) =
                codex_item_event(&inner.session_id, notification.params.as_ref(), "started")
            {
                events.push(event);
            } else if let Some(message) =
                describe_item_progress(notification.params.as_ref(), "started")
            {
                events.push(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message,
                    percent: None,
                });
            }
        }
        "item/completed" => {
            if let Some(event) =
                codex_item_event(&inner.session_id, notification.params.as_ref(), "completed")
            {
                events.push(event);
            } else if let Some(message) =
                describe_item_progress(notification.params.as_ref(), "completed")
            {
                events.push(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message,
                    percent: None,
                });
            }
            if let Some(text) =
                completed_agent_message_text(inner, notification.params.as_ref()).await
            {
                events.push(SessionEvent::Output {
                    session_id: inner.session_id.clone(),
                    text,
                });
            }
        }
        method if method.starts_with("item/") => {}
        _ => {}
    }

    if events.is_empty() {
        return;
    }

    let tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = tx {
        for event in events {
            let _ = tx.send(event).await;
        }
    }
}

async fn update_mcp_server_status(inner: &Arc<CodexWsSessionInner>, params: &serde_json::Value) {
    let Some(name) = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return;
    };

    let status = params
        .get("status")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let error = params
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(str::to_string);

    inner
        .mcp_server_statuses
        .lock()
        .await
        .insert(name.to_string(), CodexMcpServerStatus { status, error });
}

fn codex_brehon_mcp_is_unhealthy(statuses: &HashMap<String, CodexMcpServerStatus>) -> bool {
    statuses.get("brehon").is_some_and(|status| {
        codex_mcp_status_is_unhealthy(&status.status, status.error.as_deref())
    })
}

fn codex_mcp_status_is_unhealthy(status: &str, error: Option<&str>) -> bool {
    let normalized = status.trim().to_ascii_lowercase();
    let has_error = error.is_some_and(|value| !value.trim().is_empty());

    match normalized.as_str() {
        "starting" => has_error,
        "ready" | "started" | "connected" | "ok" => false,
        "failed" | "error" | "timed_out" | "timeout" | "unavailable" | "stopped"
        | "disconnected" | "closed" => true,
        _ => {
            normalized.contains("fail")
                || normalized.contains("error")
                || normalized.contains("timeout")
                || normalized.contains("unavailable")
                || normalized.contains("disconnect")
                || normalized.contains("closed")
                || has_error
        }
    }
}

fn build_thread_start_params(
    worktree_path: &str,
    launch_config: &CodexLaunchConfig,
) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    params.insert(
        "cwd".to_string(),
        serde_json::Value::String(worktree_path.to_string()),
    );
    params.insert(
        "approvalPolicy".to_string(),
        serde_json::Value::String(launch_config.approval_policy.clone()),
    );
    params.insert(
        "personality".to_string(),
        serde_json::Value::String("none".to_string()),
    );
    params.insert(
        "sandbox".to_string(),
        serde_json::Value::String(launch_config.thread_sandbox.clone()),
    );
    if let Some(model) = launch_config.model.as_ref() {
        params.insert(
            "model".to_string(),
            serde_json::Value::String(model.clone()),
        );
    }
    if !launch_config.config.is_empty() {
        params.insert(
            "config".to_string(),
            serde_json::Value::Object(launch_config.config.clone()),
        );
    }
    serde_json::Value::Object(params)
}

fn build_turn_start_params(
    thread_id: &str,
    worktree_path: &str,
    content: &str,
    launch_config: &CodexLaunchConfig,
) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    params.insert(
        "threadId".to_string(),
        serde_json::Value::String(thread_id.to_string()),
    );
    params.insert(
        "cwd".to_string(),
        serde_json::Value::String(worktree_path.to_string()),
    );
    params.insert(
        "approvalPolicy".to_string(),
        serde_json::Value::String(launch_config.approval_policy.clone()),
    );
    params.insert(
        "sandboxPolicy".to_string(),
        sandbox_policy_for_worktree(
            &launch_config.turn_sandbox_policy,
            worktree_path,
            &launch_config.extra_writable_roots,
        ),
    );
    if let Some(model) = launch_config.model.as_ref() {
        params.insert(
            "model".to_string(),
            serde_json::Value::String(model.clone()),
        );
    }
    params.insert(
        "input".to_string(),
        serde_json::json!([
            {
                "type": "text",
                "text": content,
            }
        ]),
    );
    serde_json::Value::Object(params)
}

fn sandbox_policy_for_worktree(
    policy: &serde_json::Value,
    worktree_path: &str,
    extra_writable_roots: &[String],
) -> serde_json::Value {
    let mut policy = policy.clone();
    if policy.get("type").and_then(serde_json::Value::as_str) == Some("workspaceWrite") {
        if let serde_json::Value::Object(map) = &mut policy {
            let mut writable_roots = Vec::new();
            push_unique_writable_root(&mut writable_roots, worktree_path);
            for root in extra_writable_roots {
                push_unique_writable_root(&mut writable_roots, root);
            }
            for root in git_writable_roots_for_worktree(worktree_path) {
                push_unique_writable_root(&mut writable_roots, &root);
            }
            map.insert(
                "writableRoots".to_string(),
                serde_json::Value::Array(
                    writable_roots
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            map.entry("networkAccess".to_string())
                .or_insert(serde_json::Value::Bool(false));
        }
    }
    policy
}

fn extract_codex_launch_config(args: &[String], env: &[(String, String)]) -> CodexLaunchConfig {
    let mut config = CodexLaunchConfig::from_profile(CodexPermissionProfile::from_env_or_role(env));
    config.extra_writable_roots = extra_writable_roots_from_env(env);
    let mut idx = 0;

    while idx < args.len() {
        match args[idx].as_str() {
            "--dangerously-bypass-approvals-and-sandbox" => {
                config.approval_policy = "never".to_string();
                config.thread_sandbox = "danger-full-access".to_string();
                config.turn_sandbox_policy = serde_json::json!({ "type": "dangerFullAccess" });
                idx += 1;
            }
            "--ask-for-approval" | "-a" => {
                if let Some(value) = args.get(idx + 1) {
                    config.approval_policy = value.clone();
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--sandbox" | "-s" => {
                if let Some(value) = args.get(idx + 1) {
                    apply_sandbox_mode(&mut config, value);
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "--model" | "-m" => {
                if let Some(value) = args.get(idx + 1) {
                    config.model = Some(value.clone());
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            "-c" | "--config" => {
                if let Some(value) = args.get(idx + 1) {
                    apply_config_override(&mut config, value);
                    idx += 2;
                } else {
                    idx += 1;
                }
            }
            _ => idx += 1,
        }
    }

    config
}

fn extra_writable_roots_from_env(env: &[(String, String)]) -> Vec<String> {
    let mut roots = Vec::new();
    for key in ["BREHON_ROOT"] {
        if let Some((_, value)) = env.iter().rev().find(|(env_key, _)| env_key == key) {
            push_unique_writable_root(&mut roots, value);
        }
    }
    roots
}

fn git_writable_roots_for_worktree(worktree_path: &str) -> Vec<String> {
    let worktree = Path::new(worktree_path);
    let git_file = worktree.join(".git");
    let Ok(git_contents) = std::fs::read_to_string(&git_file) else {
        return Vec::new();
    };

    let Some(raw_gitdir) = git_contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };

    let gitdir = resolve_path(worktree, raw_gitdir);
    let mut roots = Vec::new();
    push_unique_writable_root(&mut roots, &path_to_string(&gitdir));

    let commondir_file = gitdir.join("commondir");
    if let Ok(commondir_contents) = std::fs::read_to_string(commondir_file) {
        let raw_commondir = commondir_contents.trim();
        if !raw_commondir.is_empty() {
            let common_gitdir = resolve_path(&gitdir, raw_commondir);
            push_unique_writable_root(&mut roots, &path_to_string(&common_gitdir));
        }
    }

    roots
}

fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let raw_path = Path::new(raw);
    let path = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        base.join(raw_path)
    };
    path.canonicalize().unwrap_or(path)
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn push_unique_writable_root(roots: &mut Vec<String>, raw_root: &str) {
    let root = raw_root.trim();
    if root.is_empty() || roots.iter().any(|existing| existing == root) {
        return;
    }
    roots.push(root.to_string());
}

fn apply_sandbox_mode(config: &mut CodexLaunchConfig, mode: &str) {
    config.thread_sandbox = mode.to_string();
    config.turn_sandbox_policy = sandbox_policy_for_mode(mode);
}

fn apply_config_override(config: &mut CodexLaunchConfig, override_arg: &str) {
    let Some((key, raw_value)) = override_arg.split_once('=') else {
        return;
    };

    if key == "model" {
        config.model = Some(
            parse_config_value(raw_value)
                .as_str()
                .unwrap_or(raw_value)
                .to_string(),
        );
        return;
    }

    if key.contains('"') || key.starts_with("projects.") {
        return;
    }

    if !matches!(
        key,
        "sandbox_permissions"
            | "model_instructions_file"
            | "service_tier"
            | "reasoning_effort"
            | "model_reasoning_effort"
    ) && !key.starts_with("shell_environment_policy.")
    {
        return;
    }

    set_nested_json_value(&mut config.config, key, parse_config_value(raw_value));
}

fn parse_config_value(raw: &str) -> serde_json::Value {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) {
        return json;
    }

    match raw {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => serde_json::Value::String(trim_config_quotes(raw).to_string()),
    }
}

fn trim_config_quotes(raw: &str) -> &str {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

fn set_nested_json_value(
    root: &mut serde_json::Map<String, serde_json::Value>,
    dotted_key: &str,
    value: serde_json::Value,
) {
    let mut parts = dotted_key.split('.').peekable();
    let mut current = root;

    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            current.insert(part.to_string(), value);
            return;
        }

        current = current
            .entry(part.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .expect("nested config value should be an object");
    }
}

fn parse_codex_message(raw: &str) -> Result<CodexInboundMessage, serde_json::Error> {
    serde_json::from_str(raw)
}

async fn handle_codex_request(inner: &Arc<CodexWsSessionInner>, request: CodexRequest) {
    match request.method.as_str() {
        "mcpServer/elicitation/request" => {
            if should_auto_accept_mcp_tool_call(request.params.as_ref()) {
                if let Err(err) = send_codex_response(
                    inner,
                    request.id.clone(),
                    serde_json::json!({ "action": "accept" }),
                )
                .await
                {
                    warn!(error = %err, "Failed to auto-accept Codex MCP tool approval");
                    return;
                }

                let tx = inner.event_tx.lock().await.clone();
                if let Some(tx) = tx {
                    let _ = tx
                        .send(SessionEvent::Progress {
                            session_id: inner.session_id.clone(),
                            message: describe_approval_message(request.params.as_ref())
                                .unwrap_or_else(|| "Approved Codex MCP tool call".to_string()),
                            percent: None,
                        })
                        .await;
                }
            } else {
                let tx = inner.event_tx.lock().await.clone();
                if let Some(tx) = tx {
                    let _ = tx
                        .send(SessionEvent::Progress {
                            session_id: inner.session_id.clone(),
                            message: "Codex requested unsupported MCP user input".to_string(),
                            percent: None,
                        })
                        .await;
                }
            }
        }
        other => {
            let tx = inner.event_tx.lock().await.clone();
            if let Some(tx) = tx {
                let _ = tx
                    .send(SessionEvent::Progress {
                        session_id: inner.session_id.clone(),
                        message: format!("Codex request requires handling: {other}"),
                        percent: None,
                    })
                    .await;
            }
        }
    }
}

async fn send_codex_response(
    inner: &Arc<CodexWsSessionInner>,
    id: serde_json::Value,
    result: serde_json::Value,
) -> Result<(), CodexError> {
    let payload = serde_json::to_string(&serde_json::json!({
        "id": id,
        "result": result,
    }))
    .map_err(|err| CodexError::Protocol(err.to_string()))?;

    let mut writer = inner.writer.lock().await;
    writer
        .send(Message::Text(payload.into()))
        .await
        .map_err(|err| CodexError::WebSocket(err.to_string()))
}

fn describe_item_progress(params: Option<&serde_json::Value>, phase: &str) -> Option<String> {
    let item_type = params
        .and_then(|params| params.get("item"))
        .and_then(|item| item.get("type"))
        .and_then(serde_json::Value::as_str)?;

    match item_type {
        "reasoning" => Some(format!("Codex reasoning {phase}")),
        "agentMessage" => Some(format!("Codex response {phase}")),
        "mcpToolCall" => describe_mcp_tool_item(params, phase),
        "userMessage" => None,
        other => Some(format!("Codex item {other} {phase}")),
    }
}

fn codex_item_event(
    session_id: &SessionId,
    params: Option<&serde_json::Value>,
    phase: &str,
) -> Option<SessionEvent> {
    let item = params.and_then(|params| params.get("item"))?;
    let item_type = item.get("type").and_then(serde_json::Value::as_str)?;
    if matches!(item_type, "reasoning" | "agentMessage" | "userMessage") {
        return None;
    }

    let tool_id =
        item_id(params).unwrap_or_else(|| format!("codex-{item_type}-{}", uuid::Uuid::new_v4()));
    let tool_name = codex_item_tool_name(item_type, item)?;

    if phase == "started" {
        Some(SessionEvent::ToolCallStarted {
            session_id: session_id.clone(),
            tool_id,
            tool_name,
            details: codex_item_tool_details(item, params, phase),
        })
    } else {
        Some(SessionEvent::ToolCallCompleted {
            session_id: session_id.clone(),
            tool_id,
            tool_name,
            status: codex_item_completion_status(item, params),
            details: codex_item_tool_details(item, params, phase),
        })
    }
}

fn codex_item_tool_details(
    item: &serde_json::Value,
    params: Option<&serde_json::Value>,
    phase: &str,
) -> Option<serde_json::Value> {
    let mut object = serde_json::Map::new();
    if phase == "started" {
        if let Some(input) = item
            .get("input")
            .or_else(|| item.get("arguments"))
            .or_else(|| params.and_then(|params| params.get("input")))
        {
            object.insert("input".to_string(), input.clone());
        }
    } else if let Some(output) = item
        .get("output")
        .or_else(|| item.get("result"))
        .or_else(|| params.and_then(|params| params.get("output")))
    {
        object.insert("output".to_string(), output.clone());
    }
    (!object.is_empty()).then_some(serde_json::Value::Object(object))
}

fn codex_item_tool_name(item_type: &str, item: &serde_json::Value) -> Option<String> {
    match item_type {
        "commandExecution" => Some(command_execution_tool_name(item)),
        "mcpToolCall" => Some(mcp_tool_display_name(item)),
        other => Some(codex_item_type_name(other)),
    }
}

fn command_execution_tool_name(item: &serde_json::Value) -> String {
    string_or_first_array_element(
        item.get("command")
            .or_else(|| item.get("cmd"))
            .or_else(|| item.get("argv")),
    )
    .map(|command| primary_command_name(&command))
    .or_else(|| {
        item.get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToString::to_string)
    })
    .unwrap_or_else(|| "shell".to_string())
}

fn mcp_tool_display_name(item: &serde_json::Value) -> String {
    let server = item
        .get("server")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("mcp");
    let tool = item
        .get("tool")
        .or_else(|| item.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool");

    if server == "brehon" {
        if let Some(encoded) = encode_brehon_tool_name(item) {
            return encoded;
        }
        return format!("brehon_{tool}");
    }

    format!("{server}_{tool}")
}

fn encode_brehon_tool_name(item: &serde_json::Value) -> Option<String> {
    let input = item
        .get("input")
        .or_else(|| item.get("arguments"))
        .or_else(|| item.get("params"))?;

    match input {
        serde_json::Value::Object(map) => {
            if let Some(action) = map.get("action").and_then(serde_json::Value::as_str) {
                return Some(format!(r#"{{"action":"{action}"}}"#));
            }
            if let Some(status) = map.get("status").and_then(serde_json::Value::as_str) {
                return Some(format!(r#"{{"status":"{status}"}}"#));
            }
        }
        serde_json::Value::String(raw) => {
            let parsed = serde_json::from_str::<serde_json::Value>(raw).ok()?;
            return encode_brehon_tool_name(&serde_json::json!({ "input": parsed }));
        }
        _ => {}
    }

    None
}

fn codex_item_completion_status(
    item: &serde_json::Value,
    params: Option<&serde_json::Value>,
) -> String {
    item.get("status")
        .or_else(|| params.and_then(|params| params.get("status")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "completed".to_string())
}

fn string_or_first_array_element(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Array(items) => items
            .iter()
            .find_map(|item| item.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(ToString::to_string),
        _ => None,
    }
}

fn primary_command_name(command: &str) -> String {
    let token = command
        .split_whitespace()
        .next()
        .unwrap_or("shell")
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'));
    let basename = token.rsplit('/').next().unwrap_or(token);
    match basename {
        "bash" | "sh" | "zsh" | "fish" => "shell".to_string(),
        "" => "shell".to_string(),
        other => other.to_string(),
    }
}

fn codex_item_type_name(item_type: &str) -> String {
    let mut out = String::with_capacity(item_type.len());
    for (idx, ch) in item_type.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn agent_message_delta_text(params: Option<&serde_json::Value>) -> Option<String> {
    let params = params?;
    if let Some(item_type) = params
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(serde_json::Value::as_str)
    {
        if item_type != "agentMessage" {
            return None;
        }
    }

    text_value(
        params
            .get("delta")
            .or_else(|| params.get("content"))
            .or_else(|| params.get("text"))
            .or_else(|| params.get("item").and_then(|item| item.get("delta")))
            .or_else(|| params.get("item").and_then(|item| item.get("content")))
            .or_else(|| params.get("item").and_then(|item| item.get("text"))),
    )
}

fn describe_mcp_tool_item(params: Option<&serde_json::Value>, phase: &str) -> Option<String> {
    let item = params.and_then(|params| params.get("item"))?;
    let server = item
        .get("server")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let tool = item
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    Some(format!("Codex MCP tool {server}/{tool} {phase}"))
}

fn describe_mcp_tool_progress(params: Option<&serde_json::Value>) -> Option<String> {
    let item = params.and_then(|params| params.get("item"))?;
    let server = item
        .get("server")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let tool = item
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let status = item
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("inProgress");
    if matches!(
        status,
        "queued" | "inProgress" | "running" | "started" | "completed" | "success" | "ok"
    ) {
        return None;
    }
    Some(format!("Codex MCP tool {server}/{tool}: {status}"))
}

fn should_auto_accept_mcp_tool_call(params: Option<&serde_json::Value>) -> bool {
    params
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("codex_approval_kind"))
        .and_then(serde_json::Value::as_str)
        == Some("mcp_tool_call")
}

fn describe_approval_message(params: Option<&serde_json::Value>) -> Option<String> {
    let server = params
        .and_then(|params| params.get("serverName"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let tool = params
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("tool_params_display"))
        .and_then(serde_json::Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                let name = item.get("name").and_then(serde_json::Value::as_str)?;
                let value = item.get("value").and_then(serde_json::Value::as_str)?;
                (name == "name" || name == "action").then_some(value)
            })
        });

    match tool {
        Some(tool) => Some(format!("Approved Codex MCP tool call on {server}: {tool}")),
        None => Some(format!("Approved Codex MCP tool call on {server}")),
    }
}

async fn completed_agent_message_text(
    inner: &Arc<CodexWsSessionInner>,
    params: Option<&serde_json::Value>,
) -> Option<String> {
    if let Some(item_id) = item_id(params) {
        let saw_delta = inner.streamed_agent_messages.lock().await.remove(&item_id);
        if saw_delta {
            return None;
        }
    }

    let text = params
        .and_then(|params| params.get("item"))
        .and_then(|item| item.get("type").zip(item.get("text")))
        .and_then(|(kind, text)| (kind.as_str() == Some("agentMessage")).then_some(text))
        .and_then(|text| text_value(Some(text)))?;

    Some(text)
}

fn item_id(params: Option<&serde_json::Value>) -> Option<String> {
    params
        .and_then(|params| params.get("itemId"))
        .or_else(|| params.and_then(|params| params.get("item").and_then(|item| item.get("id"))))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn text_value(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => (!text.is_empty()).then(|| text.clone()),
        serde_json::Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .or_else(|| map.get("delta"))
            .and_then(|nested| text_value(Some(nested))),
        serde_json::Value::Array(items) => {
            let text: String = items
                .iter()
                .filter_map(|item| text_value(Some(item)))
                .collect();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn codex_error_message(params: Option<&serde_json::Value>) -> Option<String> {
    let raw = params
        .and_then(|params| params.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)?
        .trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(message) = json.get("error").and_then(serde_json::Value::as_str) {
            let message = message.trim();
            if !message.is_empty() {
                return Some(message.to_string());
            }
        }
    }

    Some(raw.to_string())
}

fn turn_id_from_turn_start_response(response: &CodexResponse) -> Option<String> {
    response
        .result
        .as_ref()?
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn turn_completed_turn_id(params: Option<&serde_json::Value>) -> Option<String> {
    params
        .and_then(|params| params.get("turn"))
        .and_then(|turn| turn.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

fn turn_completed_success(params: Option<&serde_json::Value>) -> bool {
    let Some(turn) = params.and_then(|params| params.get("turn")) else {
        return true;
    };

    match turn.get("status").and_then(serde_json::Value::as_str) {
        Some("failed") => false,
        Some("completed") => true,
        Some("interrupted") => false,
        Some("inProgress") => false,
        Some(_) | None => turn.get("error").is_none_or(serde_json::Value::is_null),
    }
}

fn turn_completed_tokens_used(params: Option<&serde_json::Value>) -> Option<u64> {
    let params = params?;
    let usage = params
        .get("usage")
        .or_else(|| params.get("tokenUsage"))
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("usage").or_else(|| turn.get("tokenUsage")))
        })?;

    token_field(
        usage,
        &[
            "tokensUsed",
            "tokens_used",
            "total",
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
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

/// Extracts the agent message text from a `turn/completed` notification's
/// turn items, if any.
fn turn_response_text(params: Option<&serde_json::Value>) -> Option<String> {
    let turn = params?.get("turn")?;
    let items = turn.get("items")?.as_array()?;

    let texts: Vec<String> = items
        .iter()
        .filter_map(|item| {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("agentMessage") {
                text_value(item.get("text"))
            } else {
                None
            }
        })
        .collect();

    let combined = texts.join("");
    (!combined.is_empty()).then_some(combined)
}

#[async_trait::async_trait]
impl AgentAdapter for CodexWsSession {
    async fn spawn(&self, _spec: SessionSpec) -> AdapterResult<SessionId> {
        Err(AdapterError::unsupported_operation(
            "CodexWsSession must be constructed via spawn_with_env; AgentAdapter::spawn is not supported",
        ))
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        self.send_prompt(prompt)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        self.wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(|e| {
                let kind = match e {
                    CodexError::Timeout => AdapterErrorKind::TimedOut,
                    _ => AdapterErrorKind::SendFailed,
                };
                AdapterError::new(kind, e.to_string())
            })
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let broadcast = self.inner.adapter_event_broadcast.lock().unwrap();
        let (mpsc_tx, mpsc_rx) = mpsc::channel(64);
        if let Some(broadcast) = broadcast.as_ref() {
            let mut broadcast_rx = broadcast.subscribe();
            tokio::spawn(async move {
                loop {
                    match broadcast_rx.recv().await {
                        Ok(event) => {
                            if mpsc_tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("Broadcast receiver lagged by {} messages", skipped);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        mpsc_rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        self.kill()
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::TransportClosed, e.to_string()))
    }

    fn kind(&self) -> AdapterKind {
        AdapterKind::Codex
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        Ok(self.capabilities())
    }

    async fn session_id(&self) -> SessionId {
        self.session_id().clone()
    }

    async fn session_info(&self) -> SessionInfo {
        self.session_info()
    }

    async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        self.stability_counters().await
    }

    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()> {
        self.set_config(option, value)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        self.cancel_prompt(prompt)
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        self.health_check()
            .await
            .map_err(|e| AdapterError::new(AdapterErrorKind::SendFailed, e.to_string()))
    }

    async fn attach_terminal(&self, _cols: u16, _rows: u16) -> AdapterResult<Option<TerminalId>> {
        Ok(None)
    }

    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        _input: Vec<u8>,
    ) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Terminal input is not supported for Codex websocket sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for Codex websocket sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::{AgentId, MessageKind};
    use tokio::net::TcpListener;

    #[test]
    fn test_parse_codex_response_without_jsonrpc_field() {
        let message = parse_codex_message(r#"{"id":"1","result":{"ok":true}}"#)
            .expect("parse codex response");

        match message {
            CodexInboundMessage::Response(response) => {
                assert_eq!(response.id, "1");
                assert!(response.result.is_some());
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn test_parse_codex_notification_without_jsonrpc_field() {
        let message =
            parse_codex_message(r#"{"method":"thread/started","params":{"thread":{"id":"t-1"}}}"#)
                .expect("parse codex notification");

        match message {
            CodexInboundMessage::Notification(notification) => {
                assert_eq!(notification.method, "thread/started");
                assert!(notification.params.is_some());
            }
            _ => panic!("expected notification"),
        }
    }

    #[test]
    fn test_build_thread_start_params_defaults_to_workspace_write() {
        let launch = CodexLaunchConfig::default();
        let params = build_thread_start_params("/tmp/worktree", &launch);
        assert_eq!(params["cwd"], "/tmp/worktree");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["personality"], "none");
        assert_eq!(params["sandbox"], "workspace-write");
    }

    #[test]
    fn test_build_turn_start_params_defaults_to_workspace_write() {
        let launch = CodexLaunchConfig::default();
        let params = build_turn_start_params("thread-1", "/tmp/worktree", "hello", &launch);
        assert_eq!(params["threadId"], "thread-1");
        assert_eq!(params["cwd"], "/tmp/worktree");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(params["sandboxPolicy"]["writableRoots"][0], "/tmp/worktree");
        assert_eq!(params["sandboxPolicy"]["networkAccess"], false);
        assert_eq!(params["input"][0]["type"], "text");
        assert_eq!(params["input"][0]["text"], "hello");
    }

    #[test]
    fn test_build_turn_start_params_adds_brehon_root_to_workspace_write() {
        let env = vec![(
            "BREHON_ROOT".to_string(),
            "/tmp/brehon-control-plane".to_string(),
        )];
        let launch = extract_codex_launch_config(&[], &env);

        let params = build_turn_start_params("thread-1", "/tmp/worktree", "hello", &launch);
        let roots = params["sandboxPolicy"]["writableRoots"]
            .as_array()
            .expect("writable roots");

        assert!(roots.iter().any(|root| root == "/tmp/worktree"));
        assert!(roots.iter().any(|root| root == "/tmp/brehon-control-plane"));
    }

    #[test]
    fn test_build_turn_start_params_adds_linked_worktree_git_dirs() {
        let test_root =
            std::env::temp_dir().join(format!("brehon-codex-gitdirs-{}", uuid::Uuid::new_v4()));
        let worktree = test_root.join("worktree");
        let common_gitdir = test_root.join("repo").join(".git");
        let worktree_gitdir = common_gitdir.join("worktrees").join("worker");

        std::fs::create_dir_all(&worktree).expect("create worktree");
        std::fs::create_dir_all(&worktree_gitdir).expect("create linked gitdir");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_gitdir.display()),
        )
        .expect("write gitdir file");
        std::fs::write(worktree_gitdir.join("commondir"), "../..\n").expect("write commondir");

        let launch = CodexLaunchConfig::default();
        let worktree_arg = worktree.to_string_lossy().to_string();
        let params = build_turn_start_params("thread-1", &worktree_arg, "hello", &launch);
        let roots = params["sandboxPolicy"]["writableRoots"]
            .as_array()
            .expect("writable roots");

        assert!(roots.iter().any(|root| root == &worktree_arg));
        assert!(roots.iter().any(|root| {
            root == &path_to_string(&worktree_gitdir.canonicalize().expect("canonical gitdir"))
        }));
        assert!(roots.iter().any(|root| {
            root == &path_to_string(&common_gitdir.canonicalize().expect("canonical common dir"))
        }));

        let _ = std::fs::remove_dir_all(test_root);
    }

    #[test]
    fn test_build_turn_start_params_does_not_add_writable_roots_to_read_only() {
        let launch = CodexLaunchConfig::from_profile(CodexPermissionProfile::Observe);
        let params = build_turn_start_params("thread-1", "/tmp/worktree", "hello", &launch);

        assert_eq!(params["sandboxPolicy"]["type"], "readOnly");
        assert!(params["sandboxPolicy"].get("writableRoots").is_none());
    }

    #[test]
    fn test_extract_codex_launch_config_reads_model_and_config_overrides() {
        let args = vec![
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "-c".to_string(),
            "model=\"gpt-5.4\"".to_string(),
            "-c".to_string(),
            "model_reasoning_effort=\"xhigh\"".to_string(),
            "-c".to_string(),
            "sandbox_permissions=[\"disk-full-read-access\"]".to_string(),
            "-c".to_string(),
            "shell_environment_policy.inherit=all".to_string(),
        ];

        let config = extract_codex_launch_config(&args, &[]);

        assert_eq!(config.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(config.approval_policy, "never");
        assert_eq!(config.thread_sandbox, "danger-full-access");
        assert_eq!(config.turn_sandbox_policy["type"], "dangerFullAccess");
        assert_eq!(
            config.config["sandbox_permissions"][0],
            serde_json::Value::String("disk-full-read-access".to_string())
        );
        assert_eq!(config.config["model_reasoning_effort"], "xhigh");
        assert_eq!(
            config.config["shell_environment_policy"]["inherit"],
            serde_json::Value::String("all".to_string())
        );
    }

    #[test]
    fn test_build_thread_start_params_carries_model_and_config() {
        let launch = extract_codex_launch_config(
            &[
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "-c".to_string(),
                "model=\"gpt-5.4\"".to_string(),
                "-c".to_string(),
                "shell_environment_policy.inherit=all".to_string(),
            ],
            &[],
        );

        let params = build_thread_start_params("/tmp/worktree", &launch);

        assert_eq!(params["model"], "gpt-5.4");
        assert_eq!(
            params["config"]["shell_environment_policy"]["inherit"],
            "all"
        );
    }

    #[test]
    fn test_extract_codex_launch_config_uses_observe_profile_defaults() {
        let env = vec![
            (
                CODEX_PERMISSION_PROFILE_ENV.to_string(),
                "observe".to_string(),
            ),
            ("BREHON_AGENT_ROLE".to_string(), "research".to_string()),
        ];

        let config = extract_codex_launch_config(&[], &env);

        assert_eq!(config.approval_policy, "never");
        assert_eq!(config.thread_sandbox, "read-only");
        assert_eq!(config.turn_sandbox_policy["type"], "readOnly");
    }

    #[test]
    fn test_extract_codex_launch_config_uses_unsafe_profile_defaults() {
        let env = vec![(
            CODEX_PERMISSION_PROFILE_ENV.to_string(),
            "unsafe".to_string(),
        )];

        let config = extract_codex_launch_config(&[], &env);

        assert_eq!(config.approval_policy, "never");
        assert_eq!(config.thread_sandbox, "danger-full-access");
        assert_eq!(config.turn_sandbox_policy["type"], "dangerFullAccess");
    }

    #[test]
    fn test_extract_codex_launch_config_falls_back_to_role_defaults() {
        let env = vec![("BREHON_AGENT_ROLE".to_string(), "advisor".to_string())];

        let config = extract_codex_launch_config(&[], &env);

        assert_eq!(config.approval_policy, "never");
        assert_eq!(config.thread_sandbox, "read-only");
        assert_eq!(config.turn_sandbox_policy["type"], "readOnly");
    }

    #[test]
    fn test_should_auto_accept_mcp_tool_call() {
        let params = serde_json::json!({
            "_meta": {
                "codex_approval_kind": "mcp_tool_call"
            }
        });
        assert!(should_auto_accept_mcp_tool_call(Some(&params)));
    }

    #[test]
    fn test_turn_completed_success_for_completed_turn_with_null_error() {
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turn": {
                "id": "turn-1",
                "status": "completed",
                "items": [],
                "error": null
            }
        });

        assert!(turn_completed_success(Some(&params)));
    }

    #[test]
    fn test_turn_completed_success_for_failed_turn() {
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turn": {
                "id": "turn-1",
                "status": "failed",
                "items": [],
                "error": {
                    "message": "something broke"
                }
            }
        });

        assert!(!turn_completed_success(Some(&params)));
    }

    #[test]
    fn test_turn_completed_tokens_used_sums_codex_usage_fields() {
        let params = serde_json::json!({
            "threadId": "thread-1",
            "turn": {
                "id": "turn-1",
                "status": "completed",
                "items": []
            },
            "usage": {
                "input_tokens": 15129,
                "cached_input_tokens": 7552,
                "output_tokens": 20,
                "reasoning_output_tokens": 13
            }
        });

        assert_eq!(turn_completed_tokens_used(Some(&params)), Some(15149));
    }

    #[test]
    fn test_agent_message_delta_text_extracts_streamed_text() {
        let params = serde_json::json!({
            "item": { "id": "msg-1", "type": "agentMessage" },
            "delta": { "text": "Hello " }
        });

        assert_eq!(
            agent_message_delta_text(Some(&params)).as_deref(),
            Some("Hello ")
        );
    }

    #[test]
    fn test_agent_message_delta_text_ignores_non_agent_messages() {
        let params = serde_json::json!({
            "item": { "id": "tool-1", "type": "mcpToolCall" },
            "delta": { "text": "ignored" }
        });

        assert!(agent_message_delta_text(Some(&params)).is_none());
    }

    #[test]
    fn test_codex_error_message_decodes_json_wrapped_error() {
        let params = serde_json::json!({
            "error": {
                "message": "{\"error\":\"The prompt is too long: 203272, model maximum context length: 202752\"}\n"
            }
        });

        assert_eq!(
            codex_error_message(Some(&params)).as_deref(),
            Some("The prompt is too long: 203272, model maximum context length: 202752")
        );
    }

    #[test]
    fn test_codex_error_message_preserves_plain_text_error() {
        let params = serde_json::json!({
            "error": {
                "message": "transport closed unexpectedly"
            }
        });

        assert_eq!(
            codex_error_message(Some(&params)).as_deref(),
            Some("transport closed unexpectedly")
        );
    }

    #[test]
    fn test_codex_command_execution_item_maps_to_tool_events() {
        let session_id = SessionId::new("session-1".to_string());
        let params = serde_json::json!({
            "item": {
                "id": "cmd-1",
                "type": "commandExecution",
                "command": "rg --files crates/brehon-acp"
            }
        });

        let started = codex_item_event(&session_id, Some(&params), "started").expect("started");
        let completed =
            codex_item_event(&session_id, Some(&params), "completed").expect("completed");

        match started {
            SessionEvent::ToolCallStarted {
                tool_id, tool_name, ..
            } => {
                assert_eq!(tool_id, "cmd-1");
                assert_eq!(tool_name, "rg");
            }
            other => panic!("expected started tool event, got {other:?}"),
        }

        match completed {
            SessionEvent::ToolCallCompleted {
                tool_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(tool_id, "cmd-1");
                assert_eq!(tool_name, "rg");
                assert_eq!(status, "completed");
            }
            other => panic!("expected completed tool event, got {other:?}"),
        }
    }

    #[test]
    fn test_codex_brehon_mcp_item_uses_structured_tool_name() {
        let session_id = SessionId::new("session-2".to_string());
        let params = serde_json::json!({
            "item": {
                "id": "tool-1",
                "type": "mcpToolCall",
                "server": "brehon",
                "tool": "task",
                "input": {
                    "action": "list"
                }
            }
        });

        let started = codex_item_event(&session_id, Some(&params), "started").expect("started");
        match started {
            SessionEvent::ToolCallStarted { tool_name, .. } => {
                assert_eq!(tool_name, r#"{"action":"list"}"#);
            }
            other => panic!("expected started tool event, got {other:?}"),
        }
    }

    #[test]
    fn test_codex_mcp_progress_hides_low_value_status_updates() {
        let params = serde_json::json!({
            "item": {
                "server": "brehon",
                "tool": "task",
                "status": "inProgress"
            }
        });

        assert!(describe_mcp_tool_progress(Some(&params)).is_none());
    }

    #[test]
    fn test_codex_mcp_status_is_unhealthy_for_failed_brehon_server() {
        assert!(codex_mcp_status_is_unhealthy(
            "failed",
            Some("tools/list startup timed out")
        ));
        assert!(codex_mcp_status_is_unhealthy("timeout", None));
        assert!(codex_mcp_status_is_unhealthy(
            "starting",
            Some("broken pipe")
        ));
    }

    #[test]
    fn test_codex_mcp_status_is_not_unhealthy_for_ready_brehon_server() {
        assert!(!codex_mcp_status_is_unhealthy("ready", None));
        assert!(!codex_mcp_status_is_unhealthy("starting", None));
        assert!(!codex_mcp_status_is_unhealthy("connected", None));
    }

    #[test]
    fn test_codex_brehon_mcp_health_only_tracks_brehon_server() {
        let mut statuses = HashMap::new();
        statuses.insert(
            "tools".to_string(),
            CodexMcpServerStatus {
                status: "failed".to_string(),
                error: Some("connection refused".to_string()),
            },
        );
        assert!(!codex_brehon_mcp_is_unhealthy(&statuses));

        statuses.insert(
            "brehon".to_string(),
            CodexMcpServerStatus {
                status: "failed".to_string(),
                error: Some("tools/list startup timed out".to_string()),
            },
        );
        assert!(codex_brehon_mcp_is_unhealthy(&statuses));
    }

    #[tokio::test]
    async fn test_connect_with_retry_times_out_stalled_websocket_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let started = tokio::time::Instant::now();
        let err = connect_with_retry_with_timeouts(
            &format!("ws://{addr}"),
            Duration::from_millis(250),
            Duration::from_millis(50),
        )
        .await
        .expect_err("handshake should time out");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "connect retry should stop quickly when the handshake stalls"
        );
        assert!(
            matches!(err, CodexError::WebSocket(ref message) if message.contains("timed out")),
            "expected websocket timeout error, got {err:?}"
        );

        accept_task.abort();
        let _ = accept_task.await;
    }

    #[tokio::test]
    async fn test_remove_pending_request_returns_count_without_deadlocking() {
        let pending_requests: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        {
            let mut pending = pending_requests.lock().await;
            pending.insert("request-1".to_string(), tx1);
            pending.insert("request-2".to_string(), tx2);
        }

        let (removed, remaining) = tokio::time::timeout(
            Duration::from_millis(100),
            remove_pending_request(&pending_requests, "request-1"),
        )
        .await
        .expect("remove_pending_request should not deadlock");

        assert!(removed.is_some(), "expected removed sender");
        assert_eq!(remaining, 1);
        assert!(pending_requests.lock().await.contains_key("request-2"));
    }

    // --- wait_for_response integration tests ---

    async fn test_codex_session_pair() -> (
        CodexWsSession,
        tokio::sync::mpsc::UnboundedSender<Message>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (to_server, mut from_test) = tokio::sync::mpsc::unbounded_channel::<Message>();

        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            loop {
                tokio::select! {
                    msg = ws.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                    if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                                        let method = json.get("method").and_then(|v| v.as_str()).unwrap_or("");
                                        let response_text = match method {
                                            "initialize" => format!(r#"{{"jsonrpc":"2.0","id":"{}","result":{{"protocolVersion":"2025-03-26"}}}}"#, id),
                                            "thread/start" => format!(r#"{{"jsonrpc":"2.0","id":"{}","result":{{"thread":{{"id":"thread-1"}}}}}}"#, id),
                                            "turn/start" => format!(r#"{{"jsonrpc":"2.0","id":"{}","result":{{"turn":{{"id":"turn-1"}}}}}}"#, id),
                                            _ => format!(r#"{{"jsonrpc":"2.0","id":"{}","result":{{}}}}"#, id),
                                        };
                                        let _ = ws.send(Message::Text(response_text.into())).await;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(_)) => break,
                        }
                    }
                    Some(msg) = from_test.recv() => {
                        if ws.send(msg).await.is_err() {
                            break;
                        }
                    }
                    else => break,
                }
            }
        });

        let (client_ws, _) = connect_async(format!("ws://{}", addr)).await.unwrap();
        let (writer, reader) = client_ws.split();

        let pending_requests: PendingRequests = Arc::new(Mutex::new(HashMap::new()));

        let inner = Arc::new(CodexWsSessionInner {
            session_id: SessionId::new("test-session".to_string()),
            spec: SessionSpec::new(AgentId::new("test-agent"), "worker".into(), "/tmp".into()),
            launch_config: CodexLaunchConfig::default(),
            capabilities: AgentCapabilities {
                content_block_types: vec!["text".to_string(), "image".to_string()],
                session_config_options: vec![],
                permission_support: false,
                terminal_support: false,
                tool_call_streaming: ToolCallStreaming::Basic,
            },
            process: Mutex::new(None),
            writer: Mutex::new(writer),
            pending_requests: Arc::clone(&pending_requests),
            thread_id: Mutex::new(Some("thread-1".to_string())),
            streamed_agent_messages: Mutex::new(HashSet::new()),
            mcp_server_statuses: Mutex::new(HashMap::new()),
            event_tx: Mutex::new(None),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
            adapter_event_broadcast: std::sync::Mutex::new(None),
            active_prompt_id: Mutex::new(None),
            active_prompt_turn_id: Mutex::new(None),
            active_prompt_token_attribution: Mutex::new(None),
            tokens_used: AtomicU64::new(0),
            pending_prompt_responses: Mutex::new(HashMap::new()),
            prompt_response_receivers: Mutex::new(HashMap::new()),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner), pending_requests, reader);
        *inner.reader_handle.lock().await = Some(reader_handle);

        let session = CodexWsSession {
            inner,
            created_at: chrono::Utc::now(),
        };

        (session, to_server, server_handle)
    }

    #[tokio::test]
    async fn test_wait_for_response_completes_on_turn_completed() {
        let (session, server_tx, server_handle) = test_codex_session_pair().await;

        let prompt = PromptTurn {
            prompt_id: PromptId::new("prompt-1".to_string()),
            content: "hello".to_string(),
            kind: MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        let handle = session.send_prompt(prompt).await.unwrap();

        // Send turn/completed from the mock server
        let notification = Message::Text(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[{"type":"agentMessage","text":"World"}]},"usage":{"input_tokens":10,"output_tokens":2}}}"#.into(),
        );
        let _ = server_tx.send(notification);

        let result = session
            .wait_for_response(&handle.prompt_id, 5000)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, Some("stop".to_string()));
        assert_eq!(result.response, Some("World".to_string()));
        assert_eq!(result.tokens_used, Some(12));
        assert_eq!(session.stability_counters().await.tokens_used, 12);

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_wait_for_response_after_turn_completed_already_processed() {
        let (session, server_tx, server_handle) = test_codex_session_pair().await;

        let prompt = PromptTurn {
            prompt_id: PromptId::new("prompt-2".to_string()),
            content: "hello".to_string(),
            kind: MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        let handle = session.send_prompt(prompt).await.unwrap();

        // Send turn/completed from the mock server *before* calling wait_for_response
        let notification = Message::Text(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed","items":[{"type":"agentMessage","text":"Already done"}]}}}"#.into(),
        );
        let _ = server_tx.send(notification);

        // Yield to let the reader task process the notification
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        // wait_for_response should still find the result even though turn/completed
        // was processed before it was called
        let result = session
            .wait_for_response(&handle.prompt_id, 5000)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, Some("stop".to_string()));
        assert_eq!(result.response, Some("Already done".to_string()));

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_wait_for_response_times_out() {
        let (session, _server_tx, server_handle) = test_codex_session_pair().await;

        let prompt_id = PromptId::new("prompt-timeout".to_string());
        let (tx, rx) = oneshot::channel::<PromptResult>();

        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(prompt_id.as_str().to_string(), rx);
        session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .insert(prompt_id.as_str().to_string(), tx);

        let result = session.wait_for_response(&prompt_id, 50).await;
        assert!(matches!(result, Err(CodexError::Timeout)));

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_send_prompt_rejects_overlapping_request() {
        let (session, _, server_handle) = test_codex_session_pair().await;

        *session.inner.active_prompt_id.lock().await = Some("active-prompt".to_string());

        let prompt = PromptTurn {
            prompt_id: PromptId::new("prompt-2".to_string()),
            content: "hello".to_string(),
            kind: MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        let result = session.send_prompt(prompt).await;
        assert!(matches!(
            result,
            Err(CodexError::Protocol(ref message))
                if message.contains("Concurrent prompts are not supported by Codex websocket adapter")
        ));

        assert!(session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .is_empty());
        assert!(session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .is_empty());

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_wait_for_response_timeout_clears_active_prompt_id() {
        let (session, _server_tx, server_handle) = test_codex_session_pair().await;

        let first_prompt = PromptTurn {
            prompt_id: PromptId::new("prompt-timeout-1".to_string()),
            content: "first prompt".to_string(),
            kind: MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };
        let first_handle = session.send_prompt(first_prompt).await.unwrap();
        let result = session.wait_for_response(&first_handle.prompt_id, 50).await;
        assert!(matches!(result, Err(CodexError::Timeout)));

        assert!(session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .is_empty());
        assert_eq!(session.inner.active_prompt_id.lock().await.as_deref(), None);
        assert_eq!(
            session.inner.active_prompt_turn_id.lock().await.as_deref(),
            None
        );

        let second_prompt = PromptTurn {
            prompt_id: PromptId::new("prompt-timeout-2".to_string()),
            content: "second prompt".to_string(),
            kind: MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };
        let second_handle = session.send_prompt(second_prompt).await.unwrap();
        assert_eq!(
            session.inner.active_prompt_id.lock().await.as_deref(),
            Some(second_handle.prompt_id.as_str())
        );

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_turn_completed_mismatched_turn_id_is_ignored() {
        let (session, _server_tx, server_handle) = test_codex_session_pair().await;

        let timed_out_prompt_id = PromptId::new("prompt-timeout-1".to_string());
        let (timed_out_tx, timed_out_rx) = oneshot::channel::<PromptResult>();
        session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .insert(timed_out_prompt_id.as_str().to_string(), timed_out_tx);
        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(timed_out_prompt_id.as_str().to_string(), timed_out_rx);

        {
            let mut active_prompt_id = session.inner.active_prompt_id.lock().await;
            *active_prompt_id = Some(timed_out_prompt_id.as_str().to_string());
            let mut active_prompt_turn_id = session.inner.active_prompt_turn_id.lock().await;
            *active_prompt_turn_id = Some("turn-timeout".to_string());
        }

        let timed_out_result = session.wait_for_response(&timed_out_prompt_id, 10).await;
        assert!(matches!(timed_out_result, Err(CodexError::Timeout)));

        let (active_tx, active_rx) = oneshot::channel::<PromptResult>();
        let active_prompt_id = PromptId::new("prompt-active".to_string());
        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(active_prompt_id.as_str().to_string(), active_rx);
        session
            .inner
            .pending_prompt_responses
            .lock()
            .await
            .insert(active_prompt_id.as_str().to_string(), active_tx);
        {
            let mut active_prompt_slot = session.inner.active_prompt_id.lock().await;
            *active_prompt_slot = Some(active_prompt_id.as_str().to_string());
            let mut active_turn_slot = session.inner.active_prompt_turn_id.lock().await;
            *active_turn_slot = Some("turn-active".to_string());
        }

        forward_codex_notification(
            &session.inner,
            CodexNotification {
                method: "turn/completed".to_string(),
                jsonrpc: None,
                params: Some(serde_json::json!({
                    "turn": {
                        "id": "turn-timeout",
                        "status": "completed",
                        "items": [{ "type": "agentMessage", "text": "Wrong completion" }]
                    }
                })),
            },
        )
        .await;

        assert_eq!(
            session.inner.active_prompt_id.lock().await.as_deref(),
            Some(active_prompt_id.as_str())
        );

        let pending_responses = session.inner.pending_prompt_responses.lock().await;
        assert!(pending_responses.contains_key(active_prompt_id.as_str()));
        drop(pending_responses);

        forward_codex_notification(
            &session.inner,
            CodexNotification {
                method: "turn/completed".to_string(),
                jsonrpc: None,
                params: Some(serde_json::json!({
                    "turn": {
                        "id": "turn-active",
                        "status": "completed",
                        "items": [{ "type": "agentMessage", "text": "Right completion" }]
                    }
                })),
            },
        )
        .await;

        let mut active_receiver = session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .remove(active_prompt_id.as_str())
            .unwrap();
        let result = tokio::time::timeout(Duration::from_millis(50), &mut active_receiver)
            .await
            .expect("forward_codex_notification should resolve wait_for_response receiver")
            .expect("active prompt should complete");
        assert_eq!(result.response, Some("Right completion".to_string()));

        assert_eq!(session.inner.active_prompt_id.lock().await.as_deref(), None);
        assert_eq!(
            session.inner.active_prompt_turn_id.lock().await.as_deref(),
            None
        );

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_wait_for_response_sender_dropped() {
        let (session, _server_tx, server_handle) = test_codex_session_pair().await;

        let prompt_id = PromptId::new("prompt-dropped".to_string());
        let (tx, rx) = oneshot::channel::<PromptResult>();
        drop(tx); // Drop sender immediately

        session
            .inner
            .prompt_response_receivers
            .lock()
            .await
            .insert(prompt_id.as_str().to_string(), rx);
        // Intentionally do not insert sender into pending_prompt_responses

        let result = session.wait_for_response(&prompt_id, 500).await;
        assert!(matches!(result, Err(CodexError::Protocol(_))));

        let _ = session.kill().await;
        server_handle.abort();
        let _ = server_handle.await;
    }
}
