use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};

use brehon_adapter_sdk::process::AgentProcess;
use brehon_adapter_sdk::protocol::{
    parse_message, serialize_notification, serialize_request, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse,
};
use brehon_adapter_sdk::session_event::{
    normalize_session_update_value, session_event_to_adapter_event, SessionEvent,
};
use brehon_adapter_sdk::stability_runtime::{
    clear_session_snapshot, persist_session_snapshot, schedule_clear_session_snapshot,
    schedule_persist_session_snapshot,
};
use brehon_adapter_sdk::{AdapterError, AdapterEvent, AdapterResult, AgentAdapter, PromptResult};

type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>;
type PromptResults = Arc<Mutex<HashMap<String, Result<PromptResult, String>>>>;

const COPILOT_ACP_PROTOCOL_VERSION: u32 = 1;
const COPILOT_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const COPILOT_PROMPT_ACCEPT_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// Copilot runtime configuration helpers (moved from brehon-pty)
// ---------------------------------------------------------------------------

/// Build the MCP server configuration that Copilot should register.
pub fn desired_copilot_mcp_config(exe: &str) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "brehon": {
                "type": "stdio",
                "command": exe,
                "args": ["serve"],
                "env": {},
                "tools": ["*"],
            }
        }
    })
}

fn command_exists(command: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return true;
        }

        #[cfg(windows)]
        {
            candidate.with_extension("exe").is_file()
        }

        #[cfg(not(windows))]
        {
            false
        }
    })
}

/// Determine the command and prefix args for launching Copilot.
pub fn copilot_launch_command() -> (String, Vec<String>) {
    if command_exists("copilot") {
        ("copilot".to_string(), Vec::new())
    } else {
        (
            "gh".to_string(),
            vec!["copilot".to_string(), "--".to_string()],
        )
    }
}

fn scrub_copilot_runtime_config(
    mut config: serde_json::Value,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<serde_json::Value, &'static str> {
    if config.is_null() {
        config = serde_json::json!({});
    }

    let Some(root) = config.as_object_mut() else {
        return Err(
            "Failed to update Copilot config: ~/.copilot/config.json is not a JSON object.",
        );
    };

    for key in [
        "hooks",
        "enabledPlugins",
        "extraKnownMarketplaces",
        "marketplaces",
        "trusted_folders",
        "trustedFolders",
    ] {
        root.remove(key);
    }

    root.insert("disableAllHooks".to_string(), serde_json::Value::Bool(true));
    root.insert(
        "banner".to_string(),
        serde_json::Value::String("never".to_string()),
    );

    if let Some(model) = model {
        root.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }

    if let Some(reasoning_effort) = reasoning_effort {
        root.insert(
            "effortLevel".to_string(),
            serde_json::Value::String(reasoning_effort.to_string()),
        );
    }

    Ok(config)
}

fn load_json_config(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_json_config(
    path: &Path,
    value: &serde_json::Value,
) -> std::result::Result<(), &'static str> {
    let parent = path.parent().ok_or("Config path has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|_| "Failed to create config parent directory.")?;
    let content =
        serde_json::to_string_pretty(value).map_err(|_| "Failed to serialize config JSON.")?;
    std::fs::write(path, content).map_err(|_| "Failed to write config file.")?;
    Ok(())
}

/// Prepare a local Copilot runtime directory with isolated config and cache.
pub fn prepare_local_copilot_runtime_with_global_config(
    cwd: &Path,
    exe: &str,
    global_config_path: Option<&Path>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(PathBuf, PathBuf), &'static str> {
    let config_dir = cwd.join(".brehon/factory-runtime/copilot/home");
    let cache_dir = cwd.join(".brehon/factory-runtime/copilot/cache");

    std::fs::create_dir_all(&config_dir)
        .map_err(|_| "Failed to create local Copilot runtime config directory.")?;
    std::fs::create_dir_all(&cache_dir)
        .map_err(|_| "Failed to create local Copilot runtime cache directory.")?;

    let global_config = global_config_path
        .map(Path::to_path_buf)
        .or_else(|| dirs::home_dir().map(|home| home.join(".copilot/config.json")))
        .filter(|path| path.exists())
        .map(|path| load_json_config(&path))
        .unwrap_or_else(|| serde_json::json!({}));

    let config = scrub_copilot_runtime_config(global_config, model, reasoning_effort)?;
    write_json_config(&config_dir.join("config.json"), &config)?;
    write_json_config(
        &config_dir.join("mcp-config.json"),
        &desired_copilot_mcp_config(exe),
    )?;

    Ok((config_dir, cache_dir))
}

/// Prepare a local Copilot runtime directory.
pub fn prepare_local_copilot_runtime(
    cwd: &Path,
    exe: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(PathBuf, PathBuf), &'static str> {
    prepare_local_copilot_runtime_with_global_config(cwd, exe, None, model, reasoning_effort)
}

// ---------------------------------------------------------------------------
// ACP protocol helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "clientCapabilities")]
    client_capabilities: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct InitializeResult {
    #[serde(default, rename = "protocolVersion")]
    protocol_version: Option<u32>,
    #[serde(rename = "agentCapabilities", alias = "capabilities")]
    capabilities: AcpCapabilities,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
struct AcpCapabilities {
    #[serde(default)]
    content_block_types: Vec<String>,
    #[serde(default)]
    session_config_options: Vec<String>,
    #[serde(default)]
    permission_support: bool,
    #[serde(default)]
    terminal_support: bool,
    #[serde(default)]
    tool_call_streaming: String,
    #[serde(default, rename = "promptCapabilities")]
    prompt_capabilities: Option<PromptCapabilities>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
struct PromptCapabilities {
    #[serde(default)]
    image: bool,
    #[serde(default)]
    audio: bool,
    #[serde(default, rename = "embeddedContext")]
    embedded_context: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct NewSessionParams {
    cwd: String,
    #[serde(rename = "mcpServers")]
    mcp_servers: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct NewSessionResult {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct PromptParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
struct AcpPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tokensUsed")]
    tokens_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stopReason")]
    stop_reason: Option<String>,
}

fn create_initialize_request() -> JsonRpcRequest {
    JsonRpcRequest::new(
        "initialize",
        Some(
            serde_json::to_value(InitializeParams {
                protocol_version: COPILOT_ACP_PROTOCOL_VERSION,
                client_capabilities: serde_json::json!({}),
            })
            .unwrap(),
        ),
    )
}

fn create_new_session_request(cwd: &str, mcp_servers: Vec<serde_json::Value>) -> JsonRpcRequest {
    JsonRpcRequest::new(
        "session/new",
        Some(
            serde_json::to_value(NewSessionParams {
                cwd: cwd.to_string(),
                mcp_servers,
            })
            .unwrap(),
        ),
    )
}

fn create_prompt_request(prompt_id: &str, session_id: &str, content: &str) -> JsonRpcRequest {
    JsonRpcRequest::new_with_id(
        prompt_id,
        "session/prompt",
        Some(
            serde_json::to_value(PromptParams {
                session_id: session_id.to_string(),
                prompt: vec![ContentBlock::Text {
                    text: content.to_string(),
                }],
            })
            .unwrap(),
        ),
    )
}

fn create_cancel_notification(session_id: &str) -> JsonRpcNotification {
    JsonRpcNotification::new(
        "session/cancel",
        Some(
            serde_json::to_value(serde_json::json!({
                "sessionId": session_id,
            }))
            .unwrap(),
        ),
    )
}

fn parse_new_session_result(response: &JsonRpcResponse) -> Result<NewSessionResult, String> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| format!("Failed to parse session/new result: {}", e)),
        None => Err("No result in session/new response".to_string()),
    }
}

fn parse_prompt_result(response: &JsonRpcResponse) -> Result<AcpPromptResult, String> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| format!("Failed to parse prompt result: {}", e)),
        None => Ok(AcpPromptResult::default()),
    }
}

fn acp_capabilities_to_agent(cap: AcpCapabilities) -> AgentCapabilities {
    let mut content_block_types = if cap.content_block_types.is_empty() {
        vec!["text".to_string()]
    } else {
        cap.content_block_types
    };

    if let Some(prompt_caps) = cap.prompt_capabilities {
        if prompt_caps.image && !content_block_types.iter().any(|item| item == "image") {
            content_block_types.push("image".to_string());
        }
        if prompt_caps.audio && !content_block_types.iter().any(|item| item == "audio") {
            content_block_types.push("audio".to_string());
        }
        if prompt_caps.embedded_context
            && !content_block_types.iter().any(|item| item == "resource")
        {
            content_block_types.push("resource".to_string());
        }
    }

    let session_config_options = if cap.session_config_options.is_empty() {
        vec!["model".to_string()]
    } else {
        cap.session_config_options
    };

    AgentCapabilities {
        content_block_types,
        session_config_options,
        permission_support: cap.permission_support,
        terminal_support: cap.terminal_support,
        tool_call_streaming: match cap.tool_call_streaming.as_str() {
            "full" => ToolCallStreaming::Full,
            "basic" => ToolCallStreaming::Basic,
            "" => ToolCallStreaming::Basic,
            _ => ToolCallStreaming::Basic,
        },
    }
}

// ---------------------------------------------------------------------------
// Copilot session
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CopilotError {
    #[error("failed to spawn copilot process: {0}")]
    Spawn(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for response: {0}")]
    TimedOut(String),
    #[error("session not running")]
    NotRunning,
}

pub struct CopilotSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    capabilities: AgentCapabilities,
    process: Arc<Mutex<AgentProcess>>,
    event_tx: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    pending_requests: PendingRequests,
    prompt_results: PromptResults,
    prompt_wait_notify: Notify,
    copilot_session_id: Mutex<String>,
    alive: AtomicBool,
    shutdown: AtomicBool,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct CopilotSession {
    inner: Arc<CopilotSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl CopilotSession {
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
    ) -> Result<Self, CopilotError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();

        let process = AgentProcess::spawn_with_env(command, args, &spec.worktree_path, env)
            .await
            .map_err(|e| CopilotError::Spawn(e.to_string()))?;
        let process = Arc::new(Mutex::new(process));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let prompt_results = Arc::new(Mutex::new(HashMap::new()));

        let init_response = send_request_sync(
            &process,
            create_initialize_request(),
            COPILOT_BOOTSTRAP_TIMEOUT,
        )
        .await?;

        if let Some(error) = init_response.error {
            return Err(CopilotError::Protocol(describe_rpc_error(&error)));
        }

        let init_result: InitializeResult = serde_json::from_value(
            init_response
                .result
                .clone()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|e| CopilotError::Protocol(format!("Failed to parse initialize result: {}", e)))?;
        let capabilities = acp_capabilities_to_agent(init_result.capabilities);

        let mcp_servers = parse_mcp_servers_from_env(env);
        let new_session_request = create_new_session_request(&spec.worktree_path, mcp_servers);
        let new_session_response =
            send_request_sync(&process, new_session_request, COPILOT_BOOTSTRAP_TIMEOUT).await?;

        if let Some(error) = new_session_response.error {
            return Err(CopilotError::Protocol(describe_rpc_error(&error)));
        }

        let copilot_session_id = parse_new_session_result(&new_session_response)
            .map_err(CopilotError::Protocol)?
            .session_id;

        let inner = Arc::new(CopilotSessionInner {
            session_id: session_id.clone(),
            spec: spec.clone(),
            capabilities,
            process: Arc::clone(&process),
            event_tx: Mutex::new(event_tx),
            pending_requests: Arc::clone(&pending_requests),
            prompt_results: Arc::clone(&prompt_results),
            prompt_wait_notify: Notify::new(),
            copilot_session_id: Mutex::new(copilot_session_id),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner));
        *inner.reader_handle.lock().await = Some(reader_handle);
        persist_session_snapshot(
            session_id.as_str(),
            brehon_types::StabilityCounters::default(),
        );

        Ok(Self { inner, created_at })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities.clone()
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

    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        brehon_types::StabilityCounters {
            pending_requests: self.inner.pending_requests.lock().await.len(),
            pending_prompt_waiters: self.inner.prompt_results.lock().await.len(),
            ..Default::default()
        }
    }

    fn persist_runtime_stability(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                brehon_types::StabilityCounters {
                    pending_requests: inner.pending_requests.lock().await.len(),
                    pending_prompt_waiters: inner.prompt_results.lock().await.len(),
                    ..Default::default()
                },
            );
        });
    }

    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, CopilotError> {
        let copilot_session_id = self.inner.copilot_session_id.lock().await.clone();
        let prompt_id = prompt.prompt_id.as_str().to_string();
        self.inner.prompt_results.lock().await.remove(&prompt_id);
        self.persist_runtime_stability();
        let request = create_prompt_request(&prompt_id, &copilot_session_id, &prompt.content);
        if let Some(response) = self
            .send_request_with_short_acceptance(request, COPILOT_PROMPT_ACCEPT_TIMEOUT)
            .await?
        {
            if let Some(error) = response.error {
                return Err(CopilotError::Protocol(describe_rpc_error(&error)));
            }
            let prompt_result = parse_prompt_result(&response).map_err(CopilotError::Protocol)?;
            let mut pr = PromptResult::default();
            pr.response = prompt_result.response;
            pr.tokens_used = prompt_result.tokens_used;
            pr.stop_reason = prompt_result.stop_reason;
            self.inner
                .prompt_results
                .lock()
                .await
                .insert(prompt_id, Ok(pr));
            self.inner.prompt_wait_notify.notify_waiters();
            self.persist_runtime_stability();
        }

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at: prompt.sent_at,
        })
    }

    pub async fn cancel_prompt(&self, _prompt_id: &PromptId) -> Result<(), CopilotError> {
        let copilot_session_id = self.inner.copilot_session_id.lock().await.clone();
        let notification = create_cancel_notification(&copilot_session_id);
        let line =
            serialize_notification(&notification).map_err(|e| CopilotError::Protocol(e.message))?;

        let process = self.inner.process.lock().await;
        process
            .send_line(&line)
            .await
            .map_err(|e| CopilotError::Spawn(e.to_string()))
    }

    pub async fn kill(&self) -> Result<(), CopilotError> {
        self.inner.alive.store(false, Ordering::SeqCst);
        self.inner.shutdown.store(true, Ordering::SeqCst);

        let process = self.inner.process.lock().await;
        let result = process
            .kill()
            .await
            .map_err(|e| CopilotError::Spawn(e.to_string()));
        drop(process);

        if let Some(handle) = self.inner.reader_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        clear_session_snapshot(self.inner.session_id.as_str());
        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, CopilotError> {
        let process = self.inner.process.lock().await;
        Ok(
            if process.is_alive() && self.inner.alive.load(Ordering::SeqCst) {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
        )
    }

    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<PromptResult, CopilotError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_id_str = prompt_id.as_str().to_string();

        match timeout(deadline, async {
            loop {
                let notified = self.inner.prompt_wait_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if let Some(result) = self
                    .inner
                    .prompt_results
                    .lock()
                    .await
                    .remove(&prompt_id_str)
                {
                    return result;
                }
                notified.as_mut().await;
            }
        })
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(msg)) => Err(CopilotError::Protocol(msg)),
            Err(_) => Err(CopilotError::TimedOut(prompt_id_str)),
        }
    }

    async fn send_request_with_short_acceptance(
        &self,
        request: JsonRpcRequest,
        accept_timeout: Duration,
    ) -> Result<Option<JsonRpcResponse>, CopilotError> {
        let line = serialize_request(&request).map_err(|e| CopilotError::Protocol(e.message))?;
        let request_id = request.id.clone();
        let method = request.method.clone();
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending_requests
            .lock()
            .await
            .insert(request_id.clone(), tx);
        self.persist_runtime_stability();

        {
            let process = self.inner.process.lock().await;
            if let Err(err) = process.send_line(&line).await {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                return Err(CopilotError::Spawn(err.to_string()));
            }
        }

        match timeout(accept_timeout, rx).await {
            Ok(Ok(response)) => Ok(Some(response)),
            Ok(Err(_)) => Err(CopilotError::Protocol(format!(
                "Copilot request channel closed for {request_id}"
            ))),
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                debug!(request_id = %request_id, method = %method, "Copilot prompt accepted without immediate response");
                Ok(None)
            }
        }
    }
}

async fn send_request_sync(
    process: &Arc<Mutex<AgentProcess>>,
    request: JsonRpcRequest,
    timeout_duration: Duration,
) -> Result<JsonRpcResponse, CopilotError> {
    let line = serialize_request(&request).map_err(|e| CopilotError::Protocol(e.message))?;
    let request_id = request.id.clone();
    let process = process.lock().await;
    process
        .send_line(&line)
        .await
        .map_err(|e| CopilotError::Spawn(e.to_string()))?;

    loop {
        let line = process
            .recv_line(timeout_duration.as_millis() as u64)
            .await
            .map_err(|e| CopilotError::Protocol(e.to_string()))?
            .ok_or_else(|| {
                CopilotError::Protocol("Copilot process exited during bootstrap".to_string())
            })?;
        if line.is_empty() {
            continue;
        }
        match parse_message(&line) {
            Ok(JsonRpcMessage::Response(response)) if response.id == request_id => {
                return Ok(response)
            }
            Ok(JsonRpcMessage::Notification(_)) => continue,
            Ok(JsonRpcMessage::Response(_)) => continue,
            Ok(JsonRpcMessage::Request(request)) => {
                debug!(method = %request.method, "Ignoring Copilot server request during bootstrap");
                continue;
            }
            Err(err) => {
                return Err(CopilotError::Protocol(format!(
                    "Failed to parse Copilot bootstrap response: {}",
                    err.message
                )));
            }
        }
    }
}

fn spawn_reader(inner: Arc<CopilotSessionInner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if inner.shutdown.load(Ordering::SeqCst) {
                debug!(session_id = %inner.session_id, "Copilot reader exiting due to shutdown signal");
                break;
            }

            let next = {
                let process = inner.process.lock().await;
                process.recv_line(100).await
            };

            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(brehon_adapter_sdk::process::ProcessError::Timeout) => {
                    if !inner.alive.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                Err(err) => {
                    warn!(error = %err, "Copilot ACP reader failed");
                    break;
                }
            };

            if line.is_empty() {
                continue;
            }

            match parse_message(&line) {
                Ok(JsonRpcMessage::Response(response)) => {
                    let pending_tx = inner.pending_requests.lock().await.remove(&response.id);
                    if let Some(tx) = pending_tx {
                        if let Err(response) = tx.send(response) {
                            // Receiver dropped (e.g. short-accept timeout fired).
                            // Fall back to storing in prompt_results so waiters don't hang.
                            let result = jsonrpc_response_to_prompt_result(&response);
                            inner
                                .prompt_results
                                .lock()
                                .await
                                .insert(response.id.clone(), result);
                            inner.prompt_wait_notify.notify_waiters();
                        }
                    } else {
                        let result = jsonrpc_response_to_prompt_result(&response);
                        if let Err(ref err) = result {
                            warn!(response_id = %response.id, error = %err, "Copilot prompt response error");
                        }
                        inner
                            .prompt_results
                            .lock()
                            .await
                            .insert(response.id.clone(), result);
                        inner.prompt_wait_notify.notify_waiters();
                    }
                    let pending_requests_len = inner.pending_requests.lock().await.len();
                    let pending_prompt_waiters_len = inner.prompt_results.lock().await.len();
                    schedule_persist_session_snapshot(
                        inner.session_id.as_str().to_string(),
                        brehon_types::StabilityCounters {
                            pending_requests: pending_requests_len,
                            pending_prompt_waiters: pending_prompt_waiters_len,
                            ..Default::default()
                        },
                    );
                }
                Ok(JsonRpcMessage::Notification(notification)) => {
                    forward_copilot_notification(
                        &inner,
                        &notification.method,
                        notification.params.as_ref(),
                    )
                    .await;
                }
                Ok(JsonRpcMessage::Request(request)) => {
                    debug!(method = %request.method, "Ignoring Copilot server request");
                }
                Err(err) => {
                    warn!(error = ?err, raw = %line, "Failed to parse Copilot ACP line");
                }
            }
        }

        inner.alive.store(false, Ordering::SeqCst);
        schedule_clear_session_snapshot(inner.session_id.as_str().to_string());
    })
}

async fn forward_copilot_notification(
    inner: &Arc<CopilotSessionInner>,
    method: &str,
    params: Option<&serde_json::Value>,
) {
    if method != "session/update" {
        return;
    }
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return;
    };

    let event = match normalize_session_update_value(&inner.session_id, update) {
        Ok(Some(event)) => event,
        Ok(None) => return,
        Err(err) => {
            warn!(error = %err, "Failed to normalize Copilot session update");
            return;
        }
    };

    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
}

fn describe_rpc_error(error: &brehon_adapter_sdk::protocol::JsonRpcError) -> String {
    match &error.data {
        Some(data) => format!("{}: {}", error.message, data),
        None => error.message.clone(),
    }
}

/// Convert a JSON-RPC response into a [`PromptResult`] or an error string.
///
/// Checks `response.error` first; if present, returns `Err` with the
/// formatted RPC error. Otherwise parses `response.result` into a
/// [`PromptResult`].
fn jsonrpc_response_to_prompt_result(response: &JsonRpcResponse) -> Result<PromptResult, String> {
    if let Some(ref error) = response.error {
        return Err(describe_rpc_error(error));
    }
    let acp_result = parse_prompt_result(response)
        .map_err(|err| format!("Failed to parse Copilot prompt result: {err}"))?;
    let mut pr = PromptResult::default();
    pr.response = acp_result.response;
    pr.tokens_used = acp_result.tokens_used;
    pr.stop_reason = acp_result.stop_reason;
    Ok(pr)
}

fn parse_mcp_servers_from_env(env: &[(String, String)]) -> Vec<serde_json::Value> {
    env.iter()
        .find_map(|(key, value)| (key == "BREHON_ACP_MCP_SERVERS_JSON").then_some(value))
        .and_then(|value| serde_json::from_str::<Vec<serde_json::Value>>(value).ok())
        .unwrap_or_default()
}

fn copilot_error_to_adapter_error(err: CopilotError) -> AdapterError {
    match err {
        CopilotError::Spawn(msg) => AdapterError::spawn_failed(msg),
        CopilotError::Protocol(msg) => AdapterError::send_failed(msg),
        CopilotError::TimedOut(msg) => AdapterError::timed_out(msg),
        CopilotError::NotRunning => AdapterError::transport_closed("session not running"),
    }
}

// ---------------------------------------------------------------------------
// CopilotAdapter
// ---------------------------------------------------------------------------

/// Configuration for spawning a Copilot adapter session.
#[derive(Clone, Debug)]
pub struct CopilotConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Adapter implementation for the GitHub Copilot CLI.
pub struct CopilotAdapter {
    config: CopilotConfig,
    session: RwLock<Option<CopilotSession>>,
    event_broadcast: tokio::sync::broadcast::Sender<AdapterEvent>,
}

impl CopilotAdapter {
    pub fn new(config: CopilotConfig) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(256);
        Self {
            config,
            session: RwLock::new(None),
            event_broadcast: tx,
        }
    }
}

#[async_trait]
impl AgentAdapter for CopilotAdapter {
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId> {
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let broadcast_tx = self.event_broadcast.clone();

        tokio::spawn(async move {
            while let Some(session_event) = event_rx.recv().await {
                if let Some(adapter_event) = session_event_to_adapter_event(session_event) {
                    let _ = broadcast_tx.send(adapter_event);
                }
            }
        });

        let session = CopilotSession::spawn_with_env(
            spec,
            &self.config.command,
            &self.config.args,
            &self.config.env,
            Some(event_tx),
        )
        .await
        .map_err(copilot_error_to_adapter_error)?;

        let session_id = session.session_id().clone();
        *self.session.write().await = Some(session);
        Ok(session_id)
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        let session = self
            .session
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .send_prompt(prompt)
            .await
            .map_err(copilot_error_to_adapter_error)
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let session = self
            .session
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        let result = session
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(copilot_error_to_adapter_error)?;
        Ok(result)
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = mpsc::channel(256);
        let mut broadcast_rx = self.event_broadcast.subscribe();
        tokio::spawn(async move {
            let mut last_lag_warn: Option<std::time::Instant> = None;
            let mut suppressed_skipped: u64 = 0;
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        let now = std::time::Instant::now();
                        let should_warn = match last_lag_warn {
                            Some(last) => now.duration_since(last) >= Duration::from_secs(5),
                            None => true,
                        };
                        if should_warn {
                            let skipped_total = skipped.saturating_add(suppressed_skipped);
                            warn!(
                                skipped = skipped_total,
                                "Copilot adapter event stream lagged; dropped broadcast events"
                            );
                            last_lag_warn = Some(now);
                            suppressed_skipped = 0;
                        } else {
                            suppressed_skipped = suppressed_skipped.saturating_add(skipped);
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        let session = self.session.write().await.take();
        if let Some(session) = session {
            session
                .kill()
                .await
                .map_err(copilot_error_to_adapter_error)?;
        }
        Ok(())
    }

    fn kind(&self) -> brehon_types::AdapterKind {
        brehon_types::AdapterKind::Copilot
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        let session = self
            .session
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        Ok(session.capabilities())
    }

    async fn session_id(&self) -> SessionId {
        let session = self.session.read().await.as_ref().cloned();
        session
            .map(|s| s.session_id().clone())
            .unwrap_or_else(|| SessionId::new("copilot-unknown"))
    }

    async fn session_info(&self) -> SessionInfo {
        let session = self.session.read().await.as_ref().cloned();
        session
            .map(|s| s.session_info())
            .unwrap_or_else(|| SessionInfo {
                session_id: SessionId::new("copilot-unknown"),
                agent_id: brehon_types::AgentId::new("copilot"),
                role: "worker".to_string(),
                health: HealthStatus::Unknown,
                created_at: chrono::Utc::now(),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec!["model".to_string()],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: ToolCallStreaming::None,
                },
            })
    }

    async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        let session = self.session.read().await.as_ref().cloned();
        if let Some(session) = session {
            session.stability_counters().await
        } else {
            brehon_types::StabilityCounters::default()
        }
    }

    async fn set_config(&self, _option: &str, _value: &str) -> AdapterResult<()> {
        // Copilot does not support dynamic config changes via ACP
        Err(AdapterError::unsupported_operation(
            "set_config is not supported for Copilot sessions",
        ))
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        let session = self
            .session
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .cancel_prompt(prompt)
            .await
            .map_err(copilot_error_to_adapter_error)
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        let session = self
            .session
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .health_check()
            .await
            .map_err(copilot_error_to_adapter_error)
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
            "Terminal input is not supported for Copilot sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for Copilot sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desired_copilot_mcp_config_structure() {
        let config = desired_copilot_mcp_config("/tmp/brehon");
        let servers = config.get("mcpServers").unwrap();
        let brehon = servers.get("brehon").unwrap();
        assert_eq!(brehon.get("type").unwrap(), "stdio");
        assert_eq!(brehon.get("command").unwrap(), "/tmp/brehon");
        let args = brehon.get("args").unwrap().as_array().unwrap();
        assert_eq!(args[0], "serve");
    }

    #[test]
    fn test_copilot_launch_command_returns_executable() {
        let (cmd, args) = copilot_launch_command();
        assert!(cmd == "copilot" || cmd == "gh");
        if cmd == "gh" {
            assert_eq!(args, vec!["copilot", "--"]);
        } else {
            assert!(args.is_empty());
        }
    }

    #[test]
    fn test_scrub_copilot_runtime_config_drops_hooks_and_plugins() {
        let config = serde_json::json!({
            "hooks": { "test": true },
            "enabledPlugins": ["foo"],
            "trusted_folders": ["/tmp"],
            "login": "github",
        });
        let result = scrub_copilot_runtime_config(config, None, None).unwrap();
        let obj = result.as_object().unwrap();
        assert!(!obj.contains_key("hooks"));
        assert!(!obj.contains_key("enabledPlugins"));
        assert!(!obj.contains_key("trusted_folders"));
        assert!(obj.contains_key("login"));
        assert_eq!(obj.get("disableAllHooks").unwrap(), true);
        assert_eq!(obj.get("banner").unwrap(), "never");
    }

    #[test]
    fn test_scrub_copilot_runtime_config_sets_model_and_effort() {
        let config = serde_json::json!({});
        let result = scrub_copilot_runtime_config(config, Some("gpt-4"), Some("high")).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("model").unwrap(), "gpt-4");
        assert_eq!(obj.get("effortLevel").unwrap(), "high");
    }

    #[test]
    fn test_prepare_local_copilot_runtime_creates_dirs() {
        let temp =
            std::env::temp_dir().join(format!("brehon-copilot-test-{}", uuid::Uuid::new_v4()));
        let result = prepare_local_copilot_runtime(&temp, "/tmp/brehon", None, None);
        assert!(result.is_ok());
        let (config_dir, cache_dir) = result.unwrap();
        assert!(config_dir.exists());
        assert!(cache_dir.exists());
        assert!(config_dir.join("config.json").exists());
        assert!(config_dir.join("mcp-config.json").exists());
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_acp_capabilities_conversion_defaults() {
        let cap = AcpCapabilities::default();
        let agent_cap = acp_capabilities_to_agent(cap);
        assert_eq!(agent_cap.content_block_types, vec!["text"]);
        assert_eq!(agent_cap.session_config_options, vec!["model"]);
        assert!(!agent_cap.permission_support);
        assert!(!agent_cap.terminal_support);
    }

    #[test]
    fn test_create_initialize_request() {
        let req = create_initialize_request();
        assert_eq!(req.method, "initialize");
        let params = req.params.unwrap();
        assert_eq!(params["protocolVersion"], 1);
    }

    #[test]
    fn test_create_new_session_request() {
        let req = create_new_session_request("/tmp/work", vec![]);
        assert_eq!(req.method, "session/new");
        let params = req.params.unwrap();
        assert_eq!(params["cwd"], "/tmp/work");
    }

    #[test]
    fn test_create_prompt_request() {
        let req = create_prompt_request("p-1", "s-1", "hello");
        assert_eq!(req.method, "session/prompt");
        let params = req.params.unwrap();
        assert_eq!(params["sessionId"], "s-1");
        assert_eq!(params["prompt"][0]["text"], "hello");
    }

    #[test]
    fn test_copilot_adapter_kind() {
        let adapter = CopilotAdapter::new(CopilotConfig {
            command: "copilot".to_string(),
            args: vec![],
            env: vec![],
        });
        assert!(matches!(adapter.kind(), brehon_types::AdapterKind::Copilot));
    }
}
