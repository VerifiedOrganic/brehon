use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};

use brehon_adapter_sdk::session_event::session_event_to_adapter_event;
use brehon_adapter_sdk::{AdapterError, AdapterEvent, AdapterResult, AgentAdapter, PromptResult};

use crate::process::AgentProcess;
use crate::protocol::{
    parse_message, serialize_notification, serialize_request, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse,
};
use crate::stability_runtime::{
    clear_session_snapshot, persist_session_snapshot, schedule_clear_session_snapshot,
    schedule_persist_session_snapshot,
};
use crate::updates::{normalize_session_update_value, SessionEvent};

type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>;
type PromptResults = Arc<Mutex<HashMap<String, Result<crate::acp_types::PromptResult, String>>>>;
type PromptCompleters =
    Arc<Mutex<HashMap<String, oneshot::Sender<Result<crate::acp_types::PromptResult, String>>>>>;
type PromptResultKeys = Arc<Mutex<VecDeque<String>>>;

const GEMINI_ACP_PROTOCOL_VERSION: u32 = 1;
const GEMINI_METHOD_INITIALIZE: &str = "initialize";
const GEMINI_METHOD_SESSION_NEW: &str = "session/new";
const GEMINI_METHOD_SESSION_PROMPT: &str = "session/prompt";
const GEMINI_METHOD_SESSION_CANCEL: &str = "session/cancel";
const GEMINI_METHOD_SESSION_SET_MODE: &str = "session/set_mode";
const GEMINI_METHOD_SESSION_SET_MODEL: &str = "session/set_model";
const GEMINI_METHOD_SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
const GEMINI_PERMISSION_CANCELLED: &str = "cancelled";
const GEMINI_PERMISSION_SELECTED: &str = "selected";
const BREHON_GEMINI_ALLOW_YOLO_ENV: &str = "BREHON_GEMINI_ALLOW_YOLO";
const GEMINI_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const GEMINI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const GEMINI_PROMPT_ACCEPT_TIMEOUT: Duration = Duration::from_millis(1500);
const GEMINI_YOLO_MODE_ID: &str = "yolo";
const GEMINI_MAX_CACHED_PROMPT_RESULTS: usize = 64;

/// Returns true if `BREHON_GEMINI_ALLOW_YOLO=1` is in the environment.
/// Requires an exact value of `"1"` to resist accidental enabling via truthy strings.
fn gemini_allows_privileged_mode(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(key, value)| key == BREHON_GEMINI_ALLOW_YOLO_ENV && value == "1")
}

#[derive(Debug, thiserror::Error)]
pub enum GeminiError {
    #[error("failed to spawn gemini process: {0}")]
    Spawn(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for response to {0}")]
    TimedOut(String),
    #[error("session not running")]
    #[allow(dead_code)]
    NotRunning,
}

pub struct GeminiSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    capabilities: AgentCapabilities,
    process: Arc<Mutex<AgentProcess>>,
    event_tx: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    pending_requests: PendingRequests,
    prompt_results: PromptResults,
    prompt_completers: PromptCompleters,
    prompt_result_keys: PromptResultKeys,
    gemini_session_id: Mutex<String>,
    tokens_used: AtomicU64,
    active_prompt_token_attribution: Mutex<Option<ActivePromptTokenAttribution>>,
    alive: AtomicBool,
    /// Shutdown flag: when set, the reader loop exits promptly rather than
    /// waiting for the next recv_line timeout.
    shutdown: AtomicBool,
    /// Tracked JoinHandle for the session reader, enabling deterministic
    /// cancellation and await during session shutdown.
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct GeminiSession {
    inner: Arc<GeminiSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
struct ActivePromptTokenAttribution {
    prompt_id: String,
    task_id: String,
}

impl GeminiSession {
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<SessionEvent>>,
    ) -> Result<Self, GeminiError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let allow_privileged_mode = gemini_allows_privileged_mode(env);

        let process = AgentProcess::spawn_with_env(command, args, &spec.worktree_path, env)
            .await
            .map_err(|e| GeminiError::Spawn(e.to_string()))?;
        let process = Arc::new(Mutex::new(process));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let prompt_results = Arc::new(Mutex::new(HashMap::new()));
        let prompt_completers = Arc::new(Mutex::new(HashMap::new()));
        let prompt_result_keys = Arc::new(Mutex::new(VecDeque::new()));

        let init_response = send_request_sync(
            &process,
            build_initialize_request(),
            GEMINI_BOOTSTRAP_TIMEOUT,
        )
        .await?;

        if let Some(error) = init_response.error {
            return Err(GeminiError::Protocol(describe_rpc_error(&error)));
        }

        let capabilities = gemini_capabilities(init_response.result.as_ref());

        let new_session_response = send_request_sync(
            &process,
            build_new_session_request(&spec.worktree_path),
            GEMINI_BOOTSTRAP_TIMEOUT,
        )
        .await?;

        if let Some(error) = new_session_response.error {
            return Err(GeminiError::Protocol(describe_rpc_error(&error)));
        }

        let gemini_session_id = new_session_response
            .result
            .as_ref()
            .and_then(|result| result.get("sessionId"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GeminiError::Protocol("Gemini newSession missing sessionId".to_string())
            })?
            .to_string();

        if allow_privileged_mode
            && supports_mode(new_session_response.result.as_ref(), GEMINI_YOLO_MODE_ID)
        {
            let mode_response = send_request_sync(
                &process,
                build_set_mode_request(&gemini_session_id, GEMINI_YOLO_MODE_ID),
                GEMINI_BOOTSTRAP_TIMEOUT,
            )
            .await?;

            if let Some(error) = mode_response.error {
                let reason = if is_untrusted_folder_mode_error(&error) {
                    "untrusted folder"
                } else {
                    "mode switch error"
                };
                warn!(
                    session_id = %session_id,
                    reason,
                    error = %describe_rpc_error(&error),
                    "Gemini rejected default mode switch; continuing in current mode"
                );
            }
        }

        let inner = Arc::new(GeminiSessionInner {
            session_id: session_id.clone(),
            spec: spec.clone(),
            capabilities,
            process: Arc::clone(&process),
            event_tx: Mutex::new(event_tx),
            pending_requests: Arc::clone(&pending_requests),
            prompt_results: Arc::clone(&prompt_results),
            prompt_completers: Arc::clone(&prompt_completers),
            prompt_result_keys: Arc::clone(&prompt_result_keys),
            gemini_session_id: Mutex::new(gemini_session_id),
            tokens_used: AtomicU64::new(0),
            active_prompt_token_attribution: Mutex::new(None),
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
            pending_prompt_waiters: self.inner.prompt_completers.lock().await.len(),
            tokens_used: self.inner.tokens_used.load(Ordering::Relaxed),
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
                    pending_prompt_waiters: inner.prompt_completers.lock().await.len(),
                    tokens_used: inner.tokens_used.load(Ordering::Relaxed),
                    ..Default::default()
                },
            );
        });
    }

    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, GeminiError> {
        let gemini_session_id = self.inner.gemini_session_id.lock().await.clone();
        let prompt_id = prompt.prompt_id.as_str().to_string();
        let _ = pop_prompt_result(&self.inner, &prompt_id).await;
        self.inner.prompt_completers.lock().await.remove(&prompt_id);
        set_prompt_token_attribution(&self.inner, &prompt_id, &prompt.content).await;
        self.persist_runtime_stability();
        let request = build_prompt_request(&prompt_id, &gemini_session_id, &prompt.content);
        let accepted = match self
            .send_request_with_short_acceptance(request, GEMINI_PROMPT_ACCEPT_TIMEOUT)
            .await
        {
            Ok(accepted) => accepted,
            Err(err) => {
                // If prompt send/acceptance fails while a caller is already waiting
                // (or waits immediately after), route the failure through the same
                // completion/cache path so wait_for_response does not hang until timeout.
                route_prompt_result(&self.inner, prompt_id.clone(), Err(err.to_string())).await;
                self.persist_runtime_stability();
                return Err(err);
            }
        };
        if let Some(response) = accepted {
            if let Some(error) = response.error {
                record_prompt_result_tokens(&self.inner, &prompt_id, None).await;
                return Err(GeminiError::Protocol(describe_rpc_error(&error)));
            }
            let prompt_result =
                super::acp_types::parse_prompt_result(&response).map_err(GeminiError::Protocol)?;
            route_prompt_result(&self.inner, prompt_id, Ok(prompt_result)).await;
            self.persist_runtime_stability();
        }

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at: prompt.sent_at,
        })
    }

    pub async fn cancel_prompt(&self, _prompt_id: &PromptId) -> Result<(), GeminiError> {
        let gemini_session_id = self.inner.gemini_session_id.lock().await.clone();
        let notification = build_cancel_notification(&gemini_session_id);
        let line =
            serialize_notification(&notification).map_err(|e| GeminiError::Protocol(e.message))?;

        let process = self.inner.process.lock().await;
        process
            .send_line(&line)
            .await
            .map_err(|e| GeminiError::Spawn(e.to_string()))
    }

    pub async fn set_config(&self, option: &str, value: &str) -> Result<(), GeminiError> {
        let gemini_session_id = self.inner.gemini_session_id.lock().await.clone();
        let method = match option {
            "mode" => GEMINI_METHOD_SESSION_SET_MODE,
            "model" => GEMINI_METHOD_SESSION_SET_MODEL,
            _ => return Ok(()),
        };
        let key = if option == "mode" {
            "modeId"
        } else {
            "modelId"
        };
        let request = JsonRpcRequest::new(
            method,
            Some(serde_json::json!({
                "sessionId": gemini_session_id,
                key: value,
            })),
        );
        let response = self.send_request(request).await?;
        if let Some(error) = response.error {
            return Err(GeminiError::Protocol(describe_rpc_error(&error)));
        }
        Ok(())
    }

    /// Terminates the Gemini session and awaits all spawned work for
    /// deterministic shutdown.
    pub async fn kill(&self) -> Result<(), GeminiError> {
        self.inner.alive.store(false, Ordering::SeqCst);
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.pending_requests.lock().await.clear();
        self.inner.prompt_completers.lock().await.clear();
        self.inner.prompt_results.lock().await.clear();
        self.inner.prompt_result_keys.lock().await.clear();
        self.inner
            .active_prompt_token_attribution
            .lock()
            .await
            .take();
        self.persist_runtime_stability();

        let process = self.inner.process.lock().await;
        let result = process
            .kill()
            .await
            .map_err(|e| GeminiError::Spawn(e.to_string()));
        drop(process);

        // Await the reader task with a bounded timeout for deterministic shutdown.
        if let Some(handle) = self.inner.reader_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        clear_session_snapshot(self.inner.session_id.as_str());
        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, GeminiError> {
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
    ) -> Result<crate::acp_types::PromptResult, GeminiError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_id_str = prompt_id.as_str().to_string();

        if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
            return result.map_err(GeminiError::Protocol);
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut completers = self.inner.prompt_completers.lock().await;
            if !self.inner.alive.load(Ordering::SeqCst) {
                return Err(GeminiError::NotRunning);
            }
            if completers.contains_key(&prompt_id_str) {
                return Err(GeminiError::Protocol(format!(
                    "already waiting for response to {prompt_id_str}"
                )));
            }
            completers.insert(prompt_id_str.clone(), tx);
        }

        // Handle race where result arrived after the fast path but before registration.
        if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
            self.inner
                .prompt_completers
                .lock()
                .await
                .remove(&prompt_id_str);
            return result.map_err(GeminiError::Protocol);
        }

        match timeout(deadline, rx).await {
            Ok(Ok(result)) => result.map_err(GeminiError::Protocol),
            Ok(Err(_)) => {
                self.inner
                    .prompt_completers
                    .lock()
                    .await
                    .remove(&prompt_id_str);
                if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                    return result.map_err(GeminiError::Protocol);
                }
                if !self.inner.alive.load(Ordering::SeqCst) {
                    return Err(GeminiError::NotRunning);
                }
                Err(GeminiError::Protocol(format!(
                    "Gemini response waiter closed for {prompt_id_str}"
                )))
            }
            Err(_) => {
                self.inner
                    .prompt_completers
                    .lock()
                    .await
                    .remove(&prompt_id_str);
                Err(GeminiError::TimedOut(prompt_id_str))
            }
        }
    }

    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, GeminiError> {
        let line = serialize_request(&request).map_err(|e| GeminiError::Protocol(e.message))?;
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
                return Err(GeminiError::Spawn(err.to_string()));
            }
        }

        match timeout(GEMINI_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(GeminiError::Protocol(format!(
                "Gemini request channel closed for {request_id}"
            ))),
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                Err(GeminiError::Protocol(format!(
                    "Process timeout waiting for Gemini response to {method}"
                )))
            }
        }
    }

    async fn send_request_with_short_acceptance(
        &self,
        request: JsonRpcRequest,
        accept_timeout: Duration,
    ) -> Result<Option<JsonRpcResponse>, GeminiError> {
        let line = serialize_request(&request).map_err(|e| GeminiError::Protocol(e.message))?;
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
                return Err(GeminiError::Spawn(err.to_string()));
            }
        }

        match timeout(accept_timeout, rx).await {
            Ok(Ok(response)) => Ok(Some(response)),
            Ok(Err(_)) => Err(GeminiError::Protocol(format!(
                "Gemini request channel closed for {request_id}"
            ))),
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                debug!(request_id = %request_id, method = %method, "Gemini prompt accepted without immediate response");
                Ok(None)
            }
        }
    }
}

async fn route_prompt_result(
    inner: &Arc<GeminiSessionInner>,
    prompt_id: String,
    result: Result<crate::acp_types::PromptResult, String>,
) {
    record_prompt_result_tokens(
        inner,
        &prompt_id,
        result.as_ref().ok().and_then(|result| result.tokens_used),
    )
    .await;

    let completer = inner.prompt_completers.lock().await.remove(&prompt_id);
    let mut result = Some(result);

    if let Some(tx) = completer {
        let to_send = result.take().expect("prompt result should be present");
        match tx.send(to_send) {
            Ok(()) => return,
            Err(unsent) => {
                // Waiter disappeared after we removed it from prompt_completers.
                // Preserve completion by restoring and caching the result.
                result = Some(unsent);
            }
        }
    }

    if let Some(result) = result {
        cache_prompt_result(inner, prompt_id, result).await;
    }
}

async fn set_prompt_token_attribution(
    inner: &Arc<GeminiSessionInner>,
    prompt_id: &str,
    prompt_content: &str,
) {
    let task_id = crate::stability_runtime::brehon_root_from_env().and_then(|root| {
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

async fn record_prompt_result_tokens(
    inner: &Arc<GeminiSessionInner>,
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
        let mut attribution = inner.active_prompt_token_attribution.lock().await;
        if attribution
            .as_ref()
            .is_some_and(|active| active.prompt_id == prompt_id)
        {
            attribution.take();
        }
    }
}

fn persist_task_token_delta(inner: &GeminiSessionInner, task_id: &str, tokens_delta: u64) {
    let Some(root) = crate::stability_runtime::brehon_root_from_env() else {
        return;
    };
    if let Err(err) = brehon_types::record_task_token_usage(&root, task_id, tokens_delta) {
        warn!(
            session_id = %inner.session_id,
            task_id,
            tokens_delta,
            error = %err,
            "Failed to persist Gemini task token usage"
        );
    }
}

async fn pop_prompt_result(
    inner: &Arc<GeminiSessionInner>,
    prompt_id: &str,
) -> Option<Result<crate::acp_types::PromptResult, String>> {
    let mut prompt_results = inner.prompt_results.lock().await;
    let mut prompt_result_keys = inner.prompt_result_keys.lock().await;
    let result = prompt_results.remove(prompt_id);
    if result.is_some() {
        prompt_result_keys.retain(|key| key != prompt_id);
    }
    result
}

async fn cache_prompt_result(
    inner: &Arc<GeminiSessionInner>,
    prompt_id: String,
    result: Result<crate::acp_types::PromptResult, String>,
) {
    let mut prompt_results = inner.prompt_results.lock().await;
    let mut prompt_result_keys = inner.prompt_result_keys.lock().await;

    if !prompt_results.contains_key(&prompt_id) {
        prompt_result_keys.push_back(prompt_id.clone());
    }
    prompt_results.insert(prompt_id, result);

    while prompt_results.len() > GEMINI_MAX_CACHED_PROMPT_RESULTS {
        let Some(oldest_prompt_id) = prompt_result_keys.pop_front() else {
            break;
        };
        prompt_results.remove(&oldest_prompt_id);
    }
}

async fn send_request_sync(
    process: &Arc<Mutex<AgentProcess>>,
    request: JsonRpcRequest,
    timeout_duration: Duration,
) -> Result<JsonRpcResponse, GeminiError> {
    let line = serialize_request(&request).map_err(|e| GeminiError::Protocol(e.message))?;
    let request_id = request.id.clone();
    let mut process = process.lock().await;
    process
        .send_line(&line)
        .await
        .map_err(|e| GeminiError::Spawn(e.to_string()))?;

    loop {
        let line = process
            .recv_line(timeout_duration.as_millis() as u64)
            .await
            .map_err(|e| GeminiError::Protocol(e.to_string()))?
            .ok_or_else(|| {
                GeminiError::Protocol("Gemini process exited during bootstrap".to_string())
            })?;
        if line.is_empty() {
            continue;
        }
        if !line.trim_start().starts_with('{') {
            continue;
        }
        match parse_message(&line) {
            Ok(JsonRpcMessage::Response(response)) if response.id == request_id => {
                return Ok(response);
            }
            Ok(JsonRpcMessage::Notification(_)) => continue,
            Ok(JsonRpcMessage::Response(_)) => continue,
            Ok(JsonRpcMessage::Request(request)) => {
                debug!(method = %request.method, "Ignoring Gemini server request during bootstrap");
                continue;
            }
            Err(err) => {
                return Err(GeminiError::Protocol(format!(
                    "Failed to parse Gemini bootstrap response: {}",
                    err.message
                )));
            }
        }
    }
}

fn spawn_reader(inner: Arc<GeminiSessionInner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if inner.shutdown.load(Ordering::SeqCst) {
                debug!(session_id = %inner.session_id, "Gemini reader exiting due to shutdown signal");
                break;
            }

            let next = {
                let mut process = inner.process.lock().await;
                process.recv_line(100).await
            };

            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(super::process::ProcessError::Timeout) => {
                    if !inner.alive.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                Err(err) => {
                    warn!(error = %err, "Gemini ACP reader failed");
                    break;
                }
            };

            if line.is_empty() {
                continue;
            }

            match parse_message(&line) {
                Ok(JsonRpcMessage::Response(response)) => {
                    // CRITICAL: Do NOT inline the `.lock().await.remove(...)` call
                    // inside an `if let` scrutinee. In Rust Edition 2021, temporaries
                    // in the scrutinee of `if let` / `match` live through the entire
                    // if-let/match expression, which means the `MutexGuard` temporary
                    // from `.lock().await` would still be held across the body — and
                    // the body calls `.lock().await` again, reentering a non-reentrant
                    // `tokio::sync::Mutex` and wedging the reader task forever. That
                    // deadlock silently stops all future response routing for this
                    // Gemini session, which in turn makes `send_prompt` hang on its
                    // own `pending_requests.lock().await.insert(...)` for every
                    // subsequent prompt. Hoist the lookup into its own statement so
                    // the guard is dropped at the semicolon.
                    let pending_tx = inner.pending_requests.lock().await.remove(&response.id);
                    if let Some(tx) = pending_tx {
                        let _ = tx.send(response);
                    } else {
                        let prompt_result = super::acp_types::parse_prompt_result(&response)
                            .map_err(|err| format!("Failed to parse Gemini prompt result: {err}"));
                        if let Err(err) = &prompt_result {
                            warn!(response_id = %response.id, error = %err, "Gemini prompt response error");
                        }
                        route_prompt_result(&inner, response.id.clone(), prompt_result).await;
                    }
                    let pending_requests_len = inner.pending_requests.lock().await.len();
                    let pending_prompt_waiters_len = inner.prompt_completers.lock().await.len();
                    schedule_persist_session_snapshot(
                        inner.session_id.as_str().to_string(),
                        brehon_types::StabilityCounters {
                            pending_requests: pending_requests_len,
                            pending_prompt_waiters: pending_prompt_waiters_len,
                            tokens_used: inner.tokens_used.load(Ordering::Relaxed),
                            ..Default::default()
                        },
                    );
                }
                Ok(JsonRpcMessage::Notification(notification)) => {
                    forward_gemini_notification(
                        &inner,
                        &notification.method,
                        notification.params.as_ref(),
                    )
                    .await;
                }
                Ok(JsonRpcMessage::Request(request)) => {
                    handle_gemini_request(&inner, request).await;
                }
                Err(err) => {
                    warn!(error = ?err, raw = %line, "Failed to parse Gemini ACP line");
                }
            }
        }

        inner.alive.store(false, Ordering::SeqCst);
        schedule_clear_session_snapshot(inner.session_id.as_str().to_string());
    })
}

async fn forward_gemini_notification(
    inner: &Arc<GeminiSessionInner>,
    method: &str,
    params: Option<&serde_json::Value>,
) {
    if method != "session/update" {
        return;
    }
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return;
    };

    let Some(kind) = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };

    let event = gemini_update_to_session_event(&inner.session_id, kind, update);

    let Some(event) = event else {
        return;
    };

    // Hoist the clone out of the `if let` scrutinee so the `event_tx` MutexGuard
    // is not held across the `tx.send(event).await` below. Holding the guard
    // across a send would serialize all update forwarding through this lock and
    // risk stalling if the event channel is full (see also the deadlock-prone
    // pattern fixed in the reader loop).
    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
}

fn gemini_update_to_session_event(
    session_id: &SessionId,
    kind: &str,
    update: &serde_json::Value,
) -> Option<SessionEvent> {
    match kind {
        "available_commands_update"
        | "current_mode_update"
        | "plan"
        | "usage_update"
        | "user_message_chunk" => None,
        other => normalize_session_update_value(session_id, update)
            .ok()
            .flatten()
            .or_else(|| {
                Some(SessionEvent::Progress {
                    session_id: session_id.clone(),
                    message: format!("Gemini update: {other}"),
                    percent: None,
                })
            }),
    }
}

async fn handle_gemini_request(inner: &Arc<GeminiSessionInner>, request: JsonRpcRequest) {
    if request.method != GEMINI_METHOD_SESSION_REQUEST_PERMISSION {
        debug!(method = %request.method, "Ignoring unsupported Gemini server request");
        return;
    }

    let outcome = permission_outcome(request.params.as_ref());
    let response = JsonRpcResponse::success(
        request.id,
        serde_json::json!({
            "outcome": outcome,
        }),
    );

    let line = match serde_json::to_string(&response) {
        Ok(line) => line,
        Err(err) => {
            warn!(error = %err, "Failed to serialize Gemini permission response");
            return;
        }
    };

    if let Some(message) = permission_progress_message(&outcome) {
        let event_tx = inner.event_tx.lock().await.clone();
        if let Some(tx) = event_tx {
            let _ = tx
                .send(SessionEvent::Progress {
                    session_id: inner.session_id.clone(),
                    message,
                    percent: None,
                })
                .await;
        }
    }

    let process = inner.process.lock().await;
    if let Err(err) = process.send_line(&line).await {
        warn!(error = %err, "Failed to send Gemini permission response");
    }
}

fn build_initialize_request() -> JsonRpcRequest {
    JsonRpcRequest::new(
        GEMINI_METHOD_INITIALIZE,
        Some(serde_json::json!({
            "protocolVersion": GEMINI_ACP_PROTOCOL_VERSION,
            "clientCapabilities": {}
        })),
    )
}

fn build_new_session_request(cwd: &str) -> JsonRpcRequest {
    JsonRpcRequest::new(
        GEMINI_METHOD_SESSION_NEW,
        Some(serde_json::json!({
            "cwd": cwd,
            "mcpServers": [],
        })),
    )
}

fn build_prompt_request(prompt_id: &str, session_id: &str, content: &str) -> JsonRpcRequest {
    JsonRpcRequest::new_with_id(
        prompt_id,
        GEMINI_METHOD_SESSION_PROMPT,
        Some(serde_json::json!({
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": content,
                }
            ]
        })),
    )
}

fn build_set_mode_request(session_id: &str, mode_id: &str) -> JsonRpcRequest {
    JsonRpcRequest::new(
        GEMINI_METHOD_SESSION_SET_MODE,
        Some(serde_json::json!({
            "sessionId": session_id,
            "modeId": mode_id,
        })),
    )
}

fn build_cancel_notification(session_id: &str) -> JsonRpcNotification {
    JsonRpcNotification::new(
        GEMINI_METHOD_SESSION_CANCEL,
        Some(serde_json::json!({
            "sessionId": session_id,
        })),
    )
}

fn permission_outcome(params: Option<&serde_json::Value>) -> serde_json::Value {
    let selected = params
        .and_then(|params| params.get("options"))
        .and_then(serde_json::Value::as_array)
        .and_then(|options| {
            options.iter().find_map(|option| {
                let kind = option.get("kind").and_then(serde_json::Value::as_str)?;
                let option_id = option.get("optionId").and_then(serde_json::Value::as_str)?;
                if kind.starts_with("allow_") {
                    Some(option_id.to_string())
                } else {
                    None
                }
            })
        });

    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": GEMINI_PERMISSION_SELECTED,
            "optionId": option_id,
        }),
        None => serde_json::json!({
            "outcome": GEMINI_PERMISSION_CANCELLED,
        }),
    }
}

fn permission_progress_message(outcome: &serde_json::Value) -> Option<String> {
    match outcome.get("outcome").and_then(serde_json::Value::as_str) {
        Some(GEMINI_PERMISSION_SELECTED) => outcome
            .get("optionId")
            .and_then(serde_json::Value::as_str)
            .map(|option_id| format!("Auto-approved Gemini permission request: {option_id}")),
        Some(GEMINI_PERMISSION_CANCELLED) => Some("Rejected Gemini permission request".to_string()),
        _ => None,
    }
}

fn gemini_capabilities(result: Option<&serde_json::Value>) -> AgentCapabilities {
    let prompt_capabilities = result
        .and_then(|result| result.get("agentCapabilities"))
        .and_then(|caps| caps.get("promptCapabilities"));

    let mut content_block_types = vec!["text".to_string()];
    if prompt_capabilities
        .and_then(|caps| caps.get("image"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        content_block_types.push("image".to_string());
    }
    if prompt_capabilities
        .and_then(|caps| caps.get("audio"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        content_block_types.push("audio".to_string());
    }

    AgentCapabilities {
        content_block_types,
        session_config_options: vec!["mode".to_string(), "model".to_string()],
        permission_support: true,
        terminal_support: false,
        tool_call_streaming: ToolCallStreaming::Basic,
    }
}

fn supports_mode(result: Option<&serde_json::Value>, mode_id: &str) -> bool {
    result
        .and_then(|result| result.get("modes"))
        .and_then(|modes| modes.get("availableModes"))
        .and_then(serde_json::Value::as_array)
        .map(|modes| {
            modes
                .iter()
                .any(|mode| mode.get("id").and_then(serde_json::Value::as_str) == Some(mode_id))
        })
        .unwrap_or(false)
}

fn is_untrusted_folder_mode_error(error: &super::protocol::JsonRpcError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("details"))
        .and_then(serde_json::Value::as_str)
        .map(|details| {
            details
                .to_ascii_lowercase()
                .contains("cannot enable privileged approval modes in an untrusted folder")
        })
        .unwrap_or(false)
}

fn describe_rpc_error(error: &crate::protocol::JsonRpcError) -> String {
    match &error.data {
        Some(data) => format!("{}: {}", error.message, data),
        None => error.message.clone(),
    }
}

fn acp_prompt_result_to_sdk(result: crate::acp_types::PromptResult) -> PromptResult {
    let mut pr = PromptResult::default();
    pr.response = result.response;
    pr.tokens_used = result.tokens_used;
    pr.stop_reason = result.stop_reason;
    pr
}

fn gemini_error_to_adapter_error(err: GeminiError) -> AdapterError {
    match err {
        GeminiError::Spawn(msg) => AdapterError::spawn_failed(msg),
        GeminiError::Protocol(msg) => AdapterError::send_failed(msg),
        GeminiError::TimedOut(msg) => AdapterError::timed_out(msg),
        GeminiError::NotRunning => AdapterError::transport_closed("session not running"),
    }
}

/// Configuration for spawning a Gemini adapter session.
#[derive(Clone, Debug)]
pub struct GeminiConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Adapter implementation for the Gemini CLI.
pub struct GeminiAdapter {
    config: GeminiConfig,
    session: RwLock<Option<GeminiSession>>,
    event_broadcast: tokio::sync::broadcast::Sender<AdapterEvent>,
}

impl GeminiAdapter {
    pub fn new(config: GeminiConfig) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(256);
        Self {
            config,
            session: RwLock::new(None),
            event_broadcast: tx,
        }
    }
}

#[async_trait]
impl AgentAdapter for GeminiAdapter {
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

        let session = GeminiSession::spawn_with_env(
            spec,
            &self.config.command,
            &self.config.args,
            &self.config.env,
            Some(event_tx),
        )
        .await
        .map_err(gemini_error_to_adapter_error)?;

        let session_id = session.session_id().clone();
        *self.session.write().await = Some(session);
        Ok(session_id)
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .send_prompt(prompt)
            .await
            .map_err(gemini_error_to_adapter_error)
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        let result = session
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(gemini_error_to_adapter_error)?;
        Ok(acp_prompt_result_to_sdk(result))
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = mpsc::channel(256);
        let mut broadcast_rx = self.event_broadcast.subscribe();
        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        let session = {
            let mut session = self.session.write().await;
            session.take()
        };
        if let Some(session) = session {
            session
                .kill()
                .await
                .map_err(gemini_error_to_adapter_error)?;
        }
        Ok(())
    }

    fn kind(&self) -> brehon_types::AdapterKind {
        brehon_types::AdapterKind::Acp
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        let session = self.session.read().await;
        let session = session
            .as_ref()
            .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        Ok(session.capabilities())
    }

    async fn session_id(&self) -> SessionId {
        let session = self.session.read().await;
        session
            .as_ref()
            .map(|s| s.session_id().clone())
            .unwrap_or_else(|| SessionId::new("gemini-unknown"))
    }

    async fn session_info(&self) -> SessionInfo {
        let session = self.session.read().await;
        session
            .as_ref()
            .map(|s| s.session_info())
            .unwrap_or_else(|| SessionInfo {
                session_id: SessionId::new("gemini-unknown"),
                agent_id: brehon_types::AgentId::new("gemini"),
                role: "worker".to_string(),
                health: HealthStatus::Unknown,
                created_at: chrono::Utc::now(),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec![],
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

    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .set_config(option, value)
            .await
            .map_err(gemini_error_to_adapter_error)
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .cancel_prompt(prompt)
            .await
            .map_err(gemini_error_to_adapter_error)
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .health_check()
            .await
            .map_err(gemini_error_to_adapter_error)
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
            "Terminal input is not supported for Gemini sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for Gemini sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::time::Duration;

    const TEST_HELPER_ENV: &str = "BREHON_GEMINI_TEST_HELPER";
    const TEST_SET_MODE_MARKER_ENV: &str = "BREHON_GEMINI_TEST_SET_MODE_MARKER_PATH";

    #[test]
    #[ignore = "Spawned by integration tests as a mock Gemini ACP subprocess helper"]
    fn gemini_wait_for_response_helper() {
        if std::env::var_os(TEST_HELPER_ENV).is_none() {
            return;
        }

        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        for line in stdin.lock().lines() {
            let line = line.expect("read stdin line");
            if line.trim().is_empty() {
                continue;
            }

            let message: serde_json::Value = serde_json::from_str(&line).expect("parse JSON-RPC");
            let method = message
                .get("method")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let id = message
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();

            let result = match method {
                GEMINI_METHOD_INITIALIZE => serde_json::json!({
                    "agentCapabilities": {
                        "promptCapabilities": {
                            "image": false,
                            "audio": false
                        }
                    }
                }),
                GEMINI_METHOD_SESSION_NEW => serde_json::json!({
                    "sessionId": "mock-session",
                    "modes": {
                        "availableModes": [
                            { "id": "default" },
                            { "id": "yolo" }
                        ]
                    }
                }),
                GEMINI_METHOD_SESSION_SET_MODE => {
                    if let Some(marker_path) = std::env::var_os(TEST_SET_MODE_MARKER_ENV) {
                        let _ = std::fs::write(marker_path, "");
                    }
                    serde_json::json!({})
                }
                GEMINI_METHOD_SESSION_PROMPT => {
                    // Delay long enough so wait_for_response registers a waiter,
                    // but short enough to keep this integration test fast.
                    std::thread::sleep(Duration::from_millis(150));
                    serde_json::json!({
                        "response": "mock response",
                        "tokensUsed": 7,
                        "stopReason": "stop"
                    })
                }
                _ => serde_json::json!({}),
            };

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            });
            writeln!(stdout, "{response}").expect("write response");
            stdout.flush().expect("flush response");
        }
    }

    async fn spawn_mock_gemini_session() -> GeminiSession {
        spawn_mock_gemini_session_with_env(&[]).await
    }

    async fn spawn_mock_gemini_session_with_env(extra_env: &[(String, String)]) -> GeminiSession {
        let current_exe = std::env::current_exe().expect("test binary path");
        let mut env = vec![(TEST_HELPER_ENV.to_string(), "1".to_string())];
        env.extend(extra_env.iter().cloned());
        GeminiSession::spawn_with_env(
            SessionSpec::new(
                brehon_types::AgentId::new("gemini-test"),
                "worker".to_string(),
                std::env::temp_dir().to_string_lossy().to_string(),
            ),
            current_exe.to_str().expect("path"),
            &[
                "gemini_wait_for_response_helper".to_string(),
                "--ignored".to_string(),
                "--nocapture".to_string(),
            ],
            &env,
            None,
        )
        .await
        .expect("spawn mock gemini session")
    }

    #[tokio::test]
    async fn test_spawn_with_env_yolo_contract_sends_set_mode() {
        let marker = std::env::temp_dir().join(format!("gemini-set-mode-{}", uuid::Uuid::new_v4()));
        let session = spawn_mock_gemini_session_with_env(&[
            (BREHON_GEMINI_ALLOW_YOLO_ENV.to_string(), "1".to_string()),
            (
                TEST_SET_MODE_MARKER_ENV.to_string(),
                marker.to_string_lossy().to_string(),
            ),
        ])
        .await;
        assert!(
            marker.exists(),
            "session/set_mode should be sent when BREHON_GEMINI_ALLOW_YOLO=1"
        );
        session.kill().await.expect("kill mock session");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn test_spawn_with_env_without_yolo_contract_skips_set_mode() {
        let marker = std::env::temp_dir().join(format!("gemini-set-mode-{}", uuid::Uuid::new_v4()));
        let session = spawn_mock_gemini_session_with_env(&[(
            TEST_SET_MODE_MARKER_ENV.to_string(),
            marker.to_string_lossy().to_string(),
        )])
        .await;
        assert!(
            !marker.exists(),
            "session/set_mode should NOT be sent when BREHON_GEMINI_ALLOW_YOLO is absent"
        );
        session.kill().await.expect("kill mock session");
    }

    #[tokio::test]
    async fn test_wait_for_response_integration_completes_before_timeout() {
        let session = spawn_mock_gemini_session().await;
        let prompt_id = PromptId::new("prompt-integration");
        let prompt = PromptTurn {
            prompt_id: prompt_id.clone(),
            content: "hello".to_string(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        let send_task = tokio::spawn({
            let session = session.clone();
            async move { session.send_prompt(prompt).await }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let result = session
            .wait_for_response(&prompt_id, 1000)
            .await
            .expect("wait_for_response should complete");

        assert_eq!(result.response.as_deref(), Some("mock response"));

        send_task
            .await
            .expect("send prompt task should join")
            .expect("send_prompt should succeed");
        session.kill().await.expect("kill mock session");
    }

    #[tokio::test]
    async fn test_wait_for_response_after_kill_returns_not_running() {
        let session = spawn_mock_gemini_session().await;
        session.kill().await.expect("kill mock session");

        let err = session
            .wait_for_response(&PromptId::new("after-kill"), 50)
            .await
            .expect_err("wait_for_response should fail after kill");

        assert!(
            matches!(err, GeminiError::NotRunning),
            "expected GeminiError::NotRunning, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_wait_for_response_returns_cached_send_failure() {
        let session = spawn_mock_gemini_session().await;

        {
            let process = session.inner.process.lock().await;
            process
                .kill()
                .await
                .expect("kill mock helper process directly");
        }

        let prompt_id = PromptId::new("prompt-send-fail");
        let prompt = PromptTurn {
            prompt_id: prompt_id.clone(),
            content: "hello".to_string(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: chrono::Utc::now(),
        };

        session
            .send_prompt(prompt)
            .await
            .expect_err("send_prompt should fail once helper process is killed");

        let err = session
            .wait_for_response(&prompt_id, 200)
            .await
            .expect_err("wait_for_response should return the cached send failure");

        assert!(
            matches!(err, GeminiError::Protocol(_)),
            "expected GeminiError::Protocol from cached send failure, got {err:?}"
        );

        let _ = session.kill().await;
    }

    #[tokio::test]
    async fn test_route_prompt_result_caches_when_waiter_send_fails() {
        let session = spawn_mock_gemini_session().await;
        let prompt_id = "dropped-waiter";
        let (tx, rx) = oneshot::channel();
        session
            .inner
            .prompt_completers
            .lock()
            .await
            .insert(prompt_id.to_string(), tx);
        drop(rx);

        route_prompt_result(
            &session.inner,
            prompt_id.to_string(),
            Ok(crate::acp_types::PromptResult {
                response: Some("cached fallback".to_string()),
                tokens_used: Some(1),
                stop_reason: Some("stop".to_string()),
            }),
        )
        .await;

        let cached = pop_prompt_result(&session.inner, prompt_id)
            .await
            .expect("result should be cached when waiter receiver is dropped")
            .expect("cached prompt result should be successful");
        assert_eq!(cached.response.as_deref(), Some("cached fallback"));

        session.kill().await.expect("kill mock session");
    }

    #[tokio::test]
    async fn test_prompt_result_cache_is_bounded() {
        let session = spawn_mock_gemini_session().await;
        let overflow_count = GEMINI_MAX_CACHED_PROMPT_RESULTS + 10;
        for index in 0..overflow_count {
            route_prompt_result(
                &session.inner,
                format!("prompt-{index}"),
                Ok(crate::acp_types::PromptResult {
                    response: Some(format!("response-{index}")),
                    tokens_used: Some(index as u64),
                    stop_reason: Some("stop".to_string()),
                }),
            )
            .await;
        }

        {
            let prompt_results = session.inner.prompt_results.lock().await;
            assert_eq!(prompt_results.len(), GEMINI_MAX_CACHED_PROMPT_RESULTS);
            assert!(!prompt_results.contains_key("prompt-0"));
            assert!(prompt_results.contains_key(&format!("prompt-{}", overflow_count - 1)));
        }

        {
            let prompt_result_keys = session.inner.prompt_result_keys.lock().await;
            assert_eq!(prompt_result_keys.len(), GEMINI_MAX_CACHED_PROMPT_RESULTS);
        }

        session.kill().await.expect("kill mock session");
    }

    #[test]
    fn test_gemini_error_to_adapter_error_maps_timed_out() {
        assert_eq!(
            gemini_error_to_adapter_error(GeminiError::TimedOut("prompt-1".to_string())),
            AdapterError::timed_out("prompt-1")
        );
    }

    #[test]
    fn test_gemini_capabilities_from_prompt_capabilities() {
        let result = serde_json::json!({
            "agentCapabilities": {
                "promptCapabilities": {
                    "image": true,
                    "audio": true
                }
            }
        });

        let caps = gemini_capabilities(Some(&result));
        assert_eq!(caps.content_block_types, vec!["text", "image", "audio"]);
        assert!(caps.permission_support);
        assert!(!caps.terminal_support);
    }

    #[test]
    fn test_build_new_session_request_uses_acp_method_namespace() {
        let request = build_new_session_request("/tmp/work");
        assert_eq!(request.method, GEMINI_METHOD_SESSION_NEW);
        assert_eq!(request.params.unwrap()["cwd"], "/tmp/work");
    }

    #[test]
    fn test_build_prompt_request_uses_acp_method_namespace() {
        let request = build_prompt_request("p-1", "s-1", "hello");
        assert_eq!(request.method, GEMINI_METHOD_SESSION_PROMPT);
        let params = request.params.unwrap();
        assert_eq!(params["sessionId"], "s-1");
        assert_eq!(params["prompt"][0]["text"], "hello");
    }

    #[test]
    fn test_supports_mode_detects_available_mode() {
        let result = serde_json::json!({
            "modes": {
                "availableModes": [
                    { "id": "default" },
                    { "id": "yolo" }
                ]
            }
        });

        assert!(supports_mode(Some(&result), "yolo"));
        assert!(!supports_mode(Some(&result), "plan"));
    }

    #[test]
    fn test_gemini_privileged_mode_requires_explicit_env_contract() {
        assert!(!gemini_allows_privileged_mode(&[]));
        assert!(!gemini_allows_privileged_mode(&[(
            BREHON_GEMINI_ALLOW_YOLO_ENV.to_string(),
            "0".to_string(),
        )]));
        assert!(gemini_allows_privileged_mode(&[(
            BREHON_GEMINI_ALLOW_YOLO_ENV.to_string(),
            "1".to_string(),
        )]));
    }

    #[test]
    fn test_is_untrusted_folder_mode_error_detects_expected_gemini_rejection() {
        let error = crate::protocol::JsonRpcError {
            code: -32603,
            message: "Internal error".to_string(),
            data: Some(serde_json::json!({
                "details": "Cannot enable privileged approval modes in an untrusted folder."
            })),
        };

        assert!(is_untrusted_folder_mode_error(&error));
    }

    #[test]
    fn test_permission_outcome_prefers_allow_option() {
        let params = serde_json::json!({
            "options": [
                { "optionId": "cancel", "kind": "reject_once" },
                { "optionId": "proceed_once", "kind": "allow_once" }
            ]
        });

        assert_eq!(
            permission_outcome(Some(&params)),
            serde_json::json!({
                "outcome": "selected",
                "optionId": "proceed_once",
            })
        );
    }

    #[test]
    fn test_build_cancel_notification_uses_acp_method_namespace() {
        let notification = build_cancel_notification("session-1");
        assert_eq!(notification.method, GEMINI_METHOD_SESSION_CANCEL);
        assert_eq!(notification.params.unwrap()["sessionId"], "session-1");
    }

    #[test]
    fn test_gemini_tool_call_update_preserves_tool_name() {
        let session_id = SessionId::new("session-1".to_string());
        let update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-123",
            "title": "agent",
            "status": "completed"
        });

        let event = gemini_update_to_session_event(&session_id, "tool_call_update", &update)
            .expect("event");

        match event {
            SessionEvent::ToolCallCompleted {
                tool_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(tool_id, "tool-123");
                assert_eq!(tool_name, "agent");
                assert_eq!(status, "completed");
            }
            other => panic!("expected completed tool call, got {other:?}"),
        }
    }

    #[test]
    fn test_gemini_operation_started_uses_standard_acp_normalizer() {
        let session_id = SessionId::new("session-2".to_string());
        let update = serde_json::json!({
            "sessionUpdate": "operation_started",
            "operation": "turn"
        });

        let event = gemini_update_to_session_event(&session_id, "operation_started", &update)
            .expect("event");

        match event {
            SessionEvent::OperationStarted { operation, .. } => {
                assert_eq!(operation, "turn");
            }
            other => panic!("expected started operation, got {other:?}"),
        }
    }

    #[test]
    fn test_gemini_progress_uses_standard_acp_normalizer() {
        let session_id = SessionId::new("session-3".to_string());
        let update = serde_json::json!({
            "sessionUpdate": "progress",
            "message": "Inspecting file",
            "percent": 42
        });

        let event =
            gemini_update_to_session_event(&session_id, "progress", &update).expect("event");

        match event {
            SessionEvent::Progress {
                message, percent, ..
            } => {
                assert_eq!(message, "Inspecting file");
                assert_eq!(percent, Some(42));
            }
            other => panic!("expected progress event, got {other:?}"),
        }
    }
}
