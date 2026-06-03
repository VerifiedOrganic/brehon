use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId,
};
use reqwest::header::ACCEPT_ENCODING;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::process::AgentProcess;
use brehon_adapter_sdk::stability_runtime::brehon_root_from_env;
use brehon_adapter_sdk::AdapterEvent;

const DEFAULT_SERVER_READY_TIMEOUT_MS: u64 = 30_000;
const SERVER_READY_TIMEOUT_ENV: &str = "BREHON_OPENCODE_SERVER_READY_TIMEOUT_MS";
const SERVER_READY_POLL_MS: u64 = 100;
const SERVER_READY_HEALTH_TIMEOUT_MS: u64 = 1_000;
const SERVER_PROCESS_EXIT_GRACE_MS: u64 = 100;
const SERVER_PROCESS_OUTPUT_LINES: usize = 64;
const SERVER_SPAWN_ATTEMPTS: usize = 3;
const DEFAULT_TURN_START_TIMEOUT_MS: u64 = 30_000;
const TURN_START_TIMEOUT_ENV: &str = "BREHON_OPENCODE_TURN_START_TIMEOUT_MS";
const PROMPT_RESULT_POLL_MS: u64 = 50;
const SESSION_STATUS_POLL_MS: u64 = 250;
const SESSION_SETTLE_PASSES: usize = 2;
const EVENT_SUBSCRIBE_READY_TIMEOUT_MS: u64 = 1_000;
const SERVER_REQUEST_TIMEOUT_SECS: u64 = 120;
const SERVER_MESSAGE_RETRY_ATTEMPTS: usize = 3;
const SERVER_MESSAGE_RETRY_BACKOFF_MS: u64 = 250;
const SERVER_FETCH_RETRY_ATTEMPTS: usize = 5;
const SERVER_FETCH_RETRY_BACKOFF_MS: u64 = 200;
const TURN_POLL_RECOVERY_TIMEOUT_MS: u64 = 60_000;
const TURN_POLL_RECOVERY_BACKOFF_MS: u64 = 1_000;
const SESSION_MESSAGE_LIMIT: usize = 24;

type PromptResults = Arc<Mutex<HashMap<String, Result<brehon_adapter_sdk::PromptResult, String>>>>;

struct OpenCodeServerEvent {
    message: String,
    failure: bool,
    dedupe_key: String,
}

struct OpenCodeEventSubscription {
    rx: mpsc::Receiver<OpenCodeServerEvent>,
    ready: Option<oneshot::Receiver<bool>>,
    handle: JoinHandle<()>,
}

impl OpenCodeEventSubscription {
    async fn wait_ready(&mut self) {
        let Some(ready) = self.ready.take() else {
            return;
        };
        let _ = timeout(
            Duration::from_millis(EVENT_SUBSCRIBE_READY_TIMEOUT_MS),
            ready,
        )
        .await;
    }
}

impl Drop for OpenCodeEventSubscription {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Debug, Error)]
pub enum OpenCodeError {
    #[error("failed to spawn opencode server: {0}")]
    Spawn(String),
    #[error("failed to locate server url for opencode session")]
    MissingServerUrl,
    #[error("failed to parse server url: {0}")]
    InvalidServerUrl(String),
    #[error("failed to create OpenCode session: {0}")]
    SessionCreate(String),
    #[error("opencode session is not running")]
    NotRunning,
    #[error("failed to execute opencode turn: {0}")]
    Turn(String),
    #[error("failed to contact opencode server: {0}")]
    Http(String),
}

#[derive(Clone)]
struct OpenCodeServerAuth {
    username: String,
    password: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenCodeModelSelection {
    provider_id: String,
    model_id: String,
}

struct OpenCodeServerSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    process: Mutex<Option<AgentProcess>>,
    server_url: String,
    client: Client,
    auth: Option<OpenCodeServerAuth>,
    model: Mutex<Option<OpenCodeModelSelection>>,
    opencode_session_id: Mutex<Option<String>>,
    adapter_event_tx: std::sync::Mutex<Option<mpsc::Sender<AdapterEvent>>>,
    active_prompts: Mutex<HashMap<String, JoinHandle<()>>>,
    prompt_results: PromptResults,
    tokens_used: AtomicU64,
    turn_lock: Mutex<()>,
    alive: AtomicBool,
    capabilities: AgentCapabilities,
}

struct SpawnedOpenCodeServer {
    process: AgentProcess,
    server_url: String,
    client: Client,
    auth: Option<OpenCodeServerAuth>,
    opencode_session_id: String,
}

pub struct OpenCodeServerSession {
    inner: Arc<OpenCodeServerSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct OpenCodeStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// Deserialized from `sessionID` but not inspected — retained so the
    /// struct matches the upstream wire format for debug prints.
    #[serde(rename = "sessionID", default)]
    #[allow(dead_code)]
    session_id: Option<String>,
    #[serde(default)]
    part: Option<OpenCodePart>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct OpenCodePart {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "callID", default)]
    call_id: Option<String>,
    #[serde(rename = "tool", default)]
    tool_name: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    state: Option<OpenCodeToolState>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeToolState {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeHttpPart {
    #[serde(rename = "type", default)]
    part_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    attempt: Option<u64>,
    #[serde(rename = "callID", default)]
    call_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "tool", default)]
    tool_name: Option<String>,
    #[serde(default)]
    state: Option<OpenCodeToolState>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeSessionSummary {
    id: String,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    time: Option<OpenCodeSessionTime>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeSessionTime {
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    updated: Option<i64>,
}

impl OpenCodeServerSession {
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, OpenCodeError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let spawned = spawn_server_with_retries(&spec, command, args, env).await?;
        let model = opencode_model_selection_from_env(env)?;

        let inner = Arc::new(OpenCodeServerSessionInner {
            session_id: session_id.clone(),
            spec,
            process: Mutex::new(Some(spawned.process)),
            server_url: spawned.server_url,
            client: spawned.client,
            auth: spawned.auth,
            model: Mutex::new(model),
            opencode_session_id: Mutex::new(Some(spawned.opencode_session_id)),
            adapter_event_tx: std::sync::Mutex::new(None),
            active_prompts: Mutex::new(HashMap::new()),
            prompt_results: Arc::new(Mutex::new(HashMap::new())),
            tokens_used: AtomicU64::new(0),
            turn_lock: Mutex::new(()),
            alive: AtomicBool::new(true),
            capabilities: AgentCapabilities {
                content_block_types: vec!["text".to_string(), "image".to_string()],
                session_config_options: vec![],
                permission_support: false,
                terminal_support: false,
                tool_call_streaming: brehon_types::ToolCallStreaming::Basic,
            },
        });

        Ok(Self { inner, created_at })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities.clone()
    }

    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        brehon_types::StabilityCounters {
            pending_requests: self.inner.active_prompts.lock().await.len(),
            pending_prompt_waiters: self.inner.prompt_results.lock().await.len(),
            tokens_used: self.inner.tokens_used.load(Ordering::Relaxed),
            ..Default::default()
        }
    }

    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, OpenCodeError> {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return Err(OpenCodeError::NotRunning);
        }

        let prompt_key = prompt.prompt_id.as_str().to_string();
        let prompt_id = prompt.prompt_id.clone();
        let created_at = prompt.sent_at;
        let inner = Arc::clone(&self.inner);
        let removal_key = prompt_key.clone();
        inner.prompt_results.lock().await.remove(&prompt_key);
        let task = tokio::spawn(async move {
            if let Err(err) = run_prompt(inner.clone(), prompt).await {
                let message = format!("OpenCode prompt failed: {err}");
                inner
                    .prompt_results
                    .lock()
                    .await
                    .insert(removal_key.clone(), Err(err.to_string()));
                emit_event(
                    &inner,
                    AdapterEvent::Output {
                        text: format!("{message}\n"),
                    },
                )
                .await;
                emit_event(
                    &inner,
                    AdapterEvent::OperationCompleted {
                        operation: "opencode turn".to_string(),
                        success: false,
                    },
                )
                .await;
            }
            inner.active_prompts.lock().await.remove(&removal_key);
        });

        self.inner
            .active_prompts
            .lock()
            .await
            .insert(prompt_key, task);

        Ok(PromptHandle {
            prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at,
        })
    }

    pub async fn cancel_prompt(&self, prompt_id: &PromptId) -> Result<(), OpenCodeError> {
        let key = prompt_id.as_str().to_string();
        if let Some(handle) = self.inner.active_prompts.lock().await.remove(&key) {
            abort_open_code_session(&self.inner).await?;
            handle.abort();
            return Ok(());
        }
        Err(OpenCodeError::NotRunning)
    }

    /// Terminates the OpenCode session, aborts active prompts, and awaits
    /// all spawned work for deterministic shutdown.
    pub async fn kill(&self) -> Result<(), OpenCodeError> {
        self.inner.alive.store(false, Ordering::SeqCst);

        // Abort and await active prompt tasks so shutdown is deterministic.
        let handles: Vec<_> = self
            .inner
            .active_prompts
            .lock()
            .await
            .drain()
            .map(|(_, h)| h)
            .collect();

        for handle in &handles {
            handle.abort();
        }
        // Await each aborted task with a bounded timeout so no prompt work
        // is left dangling after kill returns.
        for handle in handles {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        }

        let mut process = self.inner.process.lock().await;
        let result = if let Some(proc) = process.take() {
            proc.kill()
                .await
                .map_err(|e| OpenCodeError::Spawn(e.to_string()))
        } else {
            Ok(())
        };
        drop(process);

        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, OpenCodeError> {
        let process = self.inner.process.lock().await;
        let process_alive = process
            .as_ref()
            .map(|proc| proc.is_alive())
            .unwrap_or(false)
            && self.inner.alive.load(Ordering::SeqCst);
        drop(process);
        let server_healthy = server_health(
            &self.inner.client,
            &self.inner.server_url,
            self.inner.auth.as_ref(),
        )
        .await
        .unwrap_or(false);
        let session_healthy = self.inner.opencode_session_id.lock().await.is_some();
        Ok(if process_alive && server_healthy && session_healthy {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unhealthy
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

    pub async fn set_config(&self, option: &str, value: &str) -> Result<(), OpenCodeError> {
        match option.trim_end_matches('!') {
            "model" => {
                let selection =
                    parse_opencode_model_selection(value).map_err(OpenCodeError::Turn)?;
                *self.inner.model.lock().await = Some(selection);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<brehon_adapter_sdk::PromptResult, OpenCodeError> {
        let prompt_key = prompt_id.as_str().to_string();
        let wait = async {
            loop {
                let result = self.inner.prompt_results.lock().await.remove(&prompt_key);
                if let Some(result) = result {
                    return result.map_err(OpenCodeError::Turn);
                }

                if !self.inner.alive.load(Ordering::SeqCst) {
                    return Err(OpenCodeError::NotRunning);
                }

                sleep(Duration::from_millis(PROMPT_RESULT_POLL_MS)).await;
            }
        };

        timeout(Duration::from_millis(timeout_ms), wait)
            .await
            .map_err(|_| {
                OpenCodeError::Turn(format!(
                    "timeout waiting for OpenCode response to {}",
                    prompt_id.as_str()
                ))
            })?
    }

    pub fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = mpsc::channel(128);
        if let Ok(mut guard) = self.inner.adapter_event_tx.lock() {
            *guard = Some(tx);
        }
        rx
    }
}

#[async_trait::async_trait]
impl brehon_adapter_sdk::AgentAdapter for OpenCodeServerSession {
    async fn spawn(&self, _spec: SessionSpec) -> brehon_adapter_sdk::AdapterResult<SessionId> {
        Err(brehon_adapter_sdk::AdapterError::unsupported_operation(
            "OpenCodeServerSession is already spawned; use spawn_with_env instead",
        ))
    }

    async fn send_prompt(
        &self,
        prompt: PromptTurn,
    ) -> brehon_adapter_sdk::AdapterResult<PromptHandle> {
        self.send_prompt(prompt)
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::send_failed(e.to_string()))
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> brehon_adapter_sdk::AdapterResult<brehon_adapter_sdk::PromptResult> {
        self.wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::timed_out(e.to_string()))
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        self.events()
    }

    async fn terminate(&self) -> brehon_adapter_sdk::AdapterResult<()> {
        self.kill()
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::spawn_failed(e.to_string()))
    }

    fn kind(&self) -> brehon_types::AdapterKind {
        brehon_types::AdapterKind::Acp
    }

    async fn capabilities(&self) -> brehon_adapter_sdk::AdapterResult<AgentCapabilities> {
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

    async fn set_config(&self, option: &str, value: &str) -> brehon_adapter_sdk::AdapterResult<()> {
        self.set_config(option, value)
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::send_failed(e.to_string()))
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> brehon_adapter_sdk::AdapterResult<()> {
        self.cancel_prompt(prompt)
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::send_failed(e.to_string()))
    }

    async fn health_check(&self) -> brehon_adapter_sdk::AdapterResult<HealthStatus> {
        self.health_check()
            .await
            .map_err(|e| brehon_adapter_sdk::AdapterError::send_failed(e.to_string()))
    }

    async fn attach_terminal(
        &self,
        _cols: u16,
        _rows: u16,
    ) -> brehon_adapter_sdk::AdapterResult<Option<TerminalId>> {
        Ok(None)
    }

    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        _input: Vec<u8>,
    ) -> brehon_adapter_sdk::AdapterResult<()> {
        Err(brehon_adapter_sdk::AdapterError::unsupported_operation(
            "Terminal input is not supported for OpenCode sessions",
        ))
    }

    async fn resolve_permission(
        &self,
        _permission_id: &str,
        _approved: bool,
    ) -> brehon_adapter_sdk::AdapterResult<()> {
        Err(brehon_adapter_sdk::AdapterError::unsupported_operation(
            "Permission resolution is not supported for OpenCode sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn spawn_server_with_retries(
    spec: &SessionSpec,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<SpawnedOpenCodeServer, OpenCodeError> {
    let mut launch_args = args.to_vec();
    let mut launch_env = env.to_vec();
    let mut last_error: Option<OpenCodeError> = None;

    for attempt in 0..SERVER_SPAWN_ATTEMPTS {
        if attempt > 0 {
            let port = allocate_loopback_port()?;
            if !set_launch_loopback_port(&mut launch_args, &mut launch_env, port) {
                break;
            }
        }

        match spawn_server_once(spec, command, &launch_args, &launch_env).await {
            Ok(spawned) => return Ok(spawned),
            Err(err) => {
                let should_retry = attempt + 1 < SERVER_SPAWN_ATTEMPTS
                    && launch_has_port(&launch_args)
                    && should_retry_spawn_with_fresh_port(&err);
                last_error = Some(err);
                if !should_retry {
                    break;
                }
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| OpenCodeError::Spawn("failed to spawn OpenCode server".to_string())))
}

async fn spawn_server_once(
    spec: &SessionSpec,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> Result<SpawnedOpenCodeServer, OpenCodeError> {
    let server_url = server_url_from_launch(args, env)?;
    let auth = server_auth_from_env(env);

    let process = AgentProcess::spawn_with_env(command, args, &spec.worktree_path, env)
        .await
        .map_err(|e| OpenCodeError::Spawn(e.to_string()))?;

    let client = Client::builder()
        .timeout(Duration::from_secs(SERVER_REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| OpenCodeError::Http(e.to_string()))?;

    if let Err(err) = wait_for_server(&client, &server_url, auth.as_ref(), Some(&process)).await {
        let _ = process.kill().await;
        return Err(err);
    }

    let opencode_session_id =
        match create_server_session(&client, &server_url, auth.as_ref(), spec).await {
            Ok(session_id) => session_id,
            Err(err) => {
                let output = process_output_summary(&process).await;
                let _ = process.kill().await;
                return Err(attach_process_output(err, output.as_deref()));
            }
        };

    Ok(SpawnedOpenCodeServer {
        process,
        server_url,
        client,
        auth,
        opencode_session_id,
    })
}

fn allocate_loopback_port() -> Result<u16, OpenCodeError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| OpenCodeError::Spawn(format!("failed to allocate retry port: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| OpenCodeError::Spawn(format!("failed to read retry port: {err}")))?
        .port();
    drop(listener);
    Ok(port)
}

fn launch_has_port(args: &[String]) -> bool {
    args.iter()
        .enumerate()
        .any(|(idx, arg)| arg == "--port" && args.get(idx + 1).is_some())
        || args.iter().any(|arg| arg.starts_with("--port="))
}

fn set_launch_loopback_port(
    args: &mut [String],
    env: &mut Vec<(String, String)>,
    port: u16,
) -> bool {
    let mut changed = false;
    for idx in 0..args.len() {
        if args[idx] == "--port" {
            if let Some(value) = args.get_mut(idx + 1) {
                *value = port.to_string();
                changed = true;
            }
        } else if args[idx].starts_with("--port=") {
            args[idx] = format!("--port={port}");
            changed = true;
        }
    }

    if changed {
        upsert_env_value(
            env,
            "BREHON_OPENCODE_SERVER_URL",
            &format!("http://127.0.0.1:{port}"),
        );
    }
    changed
}

fn upsert_env_value(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn should_retry_spawn_with_fresh_port(err: &OpenCodeError) -> bool {
    let OpenCodeError::Spawn(message) = err else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("port")
        && (message.contains("in use")
            || message.contains("address already in use")
            || message.contains("eaddrinuse")
            || message.contains("failed to start server"))
}

async fn run_prompt(
    inner: Arc<OpenCodeServerSessionInner>,
    prompt: PromptTurn,
) -> Result<(), OpenCodeError> {
    let _guard = inner.turn_lock.lock().await;
    let mut response_text = String::new();
    let mut prompt_tokens_used = 0u64;
    let mut turn_error: Option<String> = None;
    let task_token_target = infer_prompt_token_target(&inner, &prompt.content);

    emit_event(
        &inner,
        AdapterEvent::OperationStarted {
            operation: "opencode turn".to_string(),
        },
    )
    .await;
    let opencode_session_id = inner
        .opencode_session_id
        .lock()
        .await
        .clone()
        .ok_or(OpenCodeError::NotRunning)?;
    let mut seen_messages = fetch_seen_message_parts(&inner, &opencode_session_id)
        .await
        .unwrap_or_default();
    let mut server_events = subscribe_open_code_events(&inner, &opencode_session_id);
    server_events.wait_ready().await;
    let session_id = send_server_message(&inner, &opencode_session_id, &prompt.content).await?;
    let turn_started_at = tokio::time::Instant::now();
    let turn_activity_timeout = turn_start_timeout();
    let mut progress = OpenCodeTurnProgress::default();
    let mut poll_recovery = OpenCodeTurnPollRecovery::default();
    let mut emitted_server_failures = HashSet::new();

    loop {
        let mut saw_server_event_activity = false;
        while let Ok(server_event) = server_events.rx.try_recv() {
            saw_server_event_activity = true;
            if server_event.failure && turn_error.is_none() {
                turn_error = Some(server_event.message.clone());
            }
            if server_event.failure
                && !emitted_server_failures.insert(server_event.dedupe_key.clone())
            {
                continue;
            }
            let normalized = AdapterEvent::Output {
                text: output_line(server_event.message),
            };
            if let AdapterEvent::Output { text, .. } = &normalized {
                if !response_text.is_empty() && !text.starts_with('\n') {
                    response_text.push('\n');
                }
                response_text.push_str(text);
            }
            emit_event(&inner, normalized).await;
        }

        let (events, saw_model_activity, tokens_delta) = match fetch_new_message_events(
            &inner,
            &session_id,
            &mut seen_messages,
            &prompt.content,
        )
        .await
        {
            Ok(result) => result,
            Err(err) if poll_recovery.should_retry(&err) => {
                sleep(Duration::from_millis(TURN_POLL_RECOVERY_BACKOFF_MS)).await;
                continue;
            }
            Err(err) => return Err(err),
        };
        if tokens_delta > 0 {
            prompt_tokens_used = prompt_tokens_used.saturating_add(tokens_delta);
            inner.tokens_used.fetch_add(tokens_delta, Ordering::Relaxed);
            if let Some(task_id) = task_token_target.as_deref() {
                persist_task_token_delta(&inner, task_id, tokens_delta);
            }
        }
        for normalized in events {
            if turn_error.is_none() {
                turn_error = opencode_event_failure_message(&normalized);
            }
            if let AdapterEvent::Output { text, .. } = &normalized {
                if !response_text.is_empty() && !text.starts_with('\n') {
                    response_text.push('\n');
                }
                response_text.push_str(text);
            }
            emit_event(&inner, normalized).await;
        }

        let session_busy = match fetch_session_busy(&inner, &session_id).await {
            Ok(session_busy) => session_busy,
            Err(err) if poll_recovery.should_retry(&err) => {
                sleep(Duration::from_millis(TURN_POLL_RECOVERY_BACKOFF_MS)).await;
                continue;
            }
            Err(err) => return Err(err),
        };
        poll_recovery.record_success();
        if progress.observe(
            session_busy,
            saw_model_activity || saw_server_event_activity,
        ) {
            break;
        }
        if !progress.saw_activity() && turn_started_at.elapsed() >= turn_activity_timeout {
            let details = no_activity_timeout_details(&inner, &session_id, &prompt.content).await;
            return Err(OpenCodeError::Turn(format!(
                "OpenCode accepted prompt_async for session {session_id} but no assistant/tool activity appeared within {}ms; {details}",
                turn_activity_timeout.as_millis(),
            )));
        }

        sleep(Duration::from_millis(SESSION_STATUS_POLL_MS)).await;
    }

    emit_event(
        &inner,
        AdapterEvent::OperationCompleted {
            operation: "opencode turn".to_string(),
            success: turn_error.is_none(),
        },
    )
    .await;

    inner.prompt_results.lock().await.insert(
        prompt.prompt_id.as_str().to_string(),
        if let Some(error) = turn_error {
            Err(error)
        } else {
            let mut result = brehon_adapter_sdk::PromptResult::default();
            result.response = if response_text.trim().is_empty() {
                None
            } else {
                Some(response_text)
            };
            result.tokens_used = (prompt_tokens_used > 0).then_some(prompt_tokens_used);
            result.stop_reason = Some("completed".to_string());
            Ok(result)
        },
    );

    Ok(())
}

#[derive(Default)]
struct OpenCodeTurnProgress {
    saw_activity: bool,
    settle_passes: usize,
}

impl OpenCodeTurnProgress {
    fn saw_activity(&self) -> bool {
        self.saw_activity
    }

    fn observe(&mut self, session_busy: bool, saw_model_activity: bool) -> bool {
        if saw_model_activity {
            self.saw_activity = true;
        }

        if session_busy {
            self.settle_passes = 0;
        } else if saw_model_activity {
            self.settle_passes = 1;
        } else if self.saw_activity {
            self.settle_passes = self.settle_passes.saturating_add(1);
        } else {
            self.settle_passes = 0;
        }

        self.saw_activity && !session_busy && self.settle_passes >= SESSION_SETTLE_PASSES
    }
}

#[derive(Default)]
struct OpenCodeTurnPollRecovery {
    first_error_at: Option<tokio::time::Instant>,
}

impl OpenCodeTurnPollRecovery {
    fn should_retry(&mut self, err: &OpenCodeError) -> bool {
        if !should_retry_turn_error(err) {
            return false;
        }

        let now = tokio::time::Instant::now();
        let first_error_at = *self.first_error_at.get_or_insert(now);
        now.duration_since(first_error_at) < Duration::from_millis(TURN_POLL_RECOVERY_TIMEOUT_MS)
    }

    fn record_success(&mut self) {
        self.first_error_at = None;
    }
}

async fn emit_event(inner: &Arc<OpenCodeServerSessionInner>, event: AdapterEvent) {
    // Hoist the clone so the `adapter_event_tx` lock is not held across
    // `tx.send(event).await` — otherwise a full event channel serializes all
    // event emission through this lock and creates unnecessary back-pressure.
    let event_tx = inner.adapter_event_tx.lock().ok().and_then(|g| g.clone());
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
}

fn infer_prompt_token_target(
    inner: &OpenCodeServerSessionInner,
    prompt_content: &str,
) -> Option<String> {
    brehon_root_from_env().and_then(|root| {
        brehon_types::infer_task_token_target(
            &root,
            inner.spec.agent_id.as_str(),
            &inner.spec.role,
            prompt_content,
        )
    })
}

fn persist_task_token_delta(inner: &OpenCodeServerSessionInner, task_id: &str, tokens_delta: u64) {
    let Some(root) = brehon_root_from_env() else {
        return;
    };
    if let Err(err) = brehon_types::record_task_token_usage(&root, task_id, tokens_delta) {
        tracing::warn!(
            session_id = %inner.session_id,
            task_id,
            tokens_delta,
            error = %err,
            "Failed to persist OpenCode task token usage"
        );
    }
}

async fn process_output_summary(process: &AgentProcess) -> Option<String> {
    sleep(Duration::from_millis(SERVER_PROCESS_EXIT_GRACE_MS)).await;
    let stderr = process
        .drain_stderr_lines(SERVER_PROCESS_OUTPUT_LINES)
        .await;
    let stdout = process
        .drain_stdout_lines(SERVER_PROCESS_OUTPUT_LINES)
        .await;

    let mut sections = Vec::new();
    if !stderr.is_empty() {
        sections.push(format!("stderr:\n{}", stderr.join("\n")));
    }
    if !stdout.is_empty() {
        sections.push(format!("stdout:\n{}", stdout.join("\n")));
    }

    let mut log_paths = HashSet::new();
    for line in &stderr {
        let Some(path) = opencode_log_path_from_line(line) else {
            continue;
        };
        if !log_paths.insert(path.clone()) {
            continue;
        }
        if let Some(tail) = read_text_tail(&path, 16 * 1024) {
            sections.push(format!("opencode log {}:\n{}", path.display(), tail));
        }
    }

    (!sections.is_empty()).then(|| sections.join("\n"))
}

fn opencode_log_path_from_line(line: &str) -> Option<std::path::PathBuf> {
    let (_, rest) = line.split_once("check log file at ")?;
    let path = rest
        .split(" for more details")
        .next()
        .unwrap_or(rest)
        .trim();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

fn read_text_tail(path: &std::path::Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).to_string();
    if start > 0 {
        if let Some(idx) = text.find('\n') {
            text = text[idx + 1..].to_string();
        }
        text.insert_str(0, "[log tail truncated]\n");
    }
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn attach_process_output(err: OpenCodeError, output: Option<&str>) -> OpenCodeError {
    match err {
        OpenCodeError::Spawn(message) => OpenCodeError::Spawn(with_process_output(message, output)),
        OpenCodeError::SessionCreate(message) => {
            OpenCodeError::SessionCreate(with_process_output(message, output))
        }
        OpenCodeError::Turn(message) => OpenCodeError::Turn(with_process_output(message, output)),
        OpenCodeError::Http(message) => OpenCodeError::Http(with_process_output(message, output)),
        other => other,
    }
}

fn with_process_output(mut message: String, output: Option<&str>) -> String {
    if let Some(output) = output.map(str::trim).filter(|output| !output.is_empty()) {
        message.push_str("\nOpenCode process output:\n");
        message.push_str(output);
    }
    message
}

fn server_auth_from_env(env: &[(String, String)]) -> Option<OpenCodeServerAuth> {
    let password = env
        .iter()
        .find(|(key, _)| key == "OPENCODE_SERVER_PASSWORD")
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let username = env
        .iter()
        .find(|(key, _)| key == "OPENCODE_SERVER_USERNAME")
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "brehon".to_string());
    Some(OpenCodeServerAuth { username, password })
}

fn opencode_model_selection_from_env(
    env: &[(String, String)],
) -> Result<Option<OpenCodeModelSelection>, OpenCodeError> {
    let Some((_, value)) = env
        .iter()
        .find(|(key, _)| key == "BREHON_AGENT_MODEL")
        .filter(|(_, value)| !value.trim().is_empty())
    else {
        return Ok(None);
    };

    parse_opencode_model_selection(value)
        .map(Some)
        .map_err(|err| OpenCodeError::Spawn(format!("invalid BREHON_AGENT_MODEL: {err}")))
}

fn parse_opencode_model_selection(value: &str) -> Result<OpenCodeModelSelection, String> {
    let trimmed = value.trim();
    let Some((provider_id, model_id)) = trimmed.split_once('/') else {
        return Err(format!(
            "OpenCode model '{trimmed}' must use provider/model format"
        ));
    };
    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() || model_id.contains('/') {
        return Err(format!(
            "OpenCode model '{trimmed}' must use provider/model format"
        ));
    }
    Ok(OpenCodeModelSelection {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    })
}

fn opencode_prompt_body(prompt: &str, model: Option<&OpenCodeModelSelection>) -> Value {
    let mut body = serde_json::Map::new();
    if let Some(model) = model {
        body.insert(
            "model".to_string(),
            serde_json::json!({
                "providerID": &model.provider_id,
                "modelID": &model.model_id,
            }),
        );
    }
    body.insert(
        "parts".to_string(),
        serde_json::json!([
            {
                "type": "text",
                "text": prompt,
            }
        ]),
    );
    Value::Object(body)
}

fn server_request(
    client: &Client,
    auth: Option<&OpenCodeServerAuth>,
    method: reqwest::Method,
    url: String,
) -> RequestBuilder {
    let builder = client
        .request(method, url)
        .header(ACCEPT_ENCODING, "identity");
    if let Some(auth) = auth {
        builder.basic_auth(&auth.username, Some(&auth.password))
    } else {
        builder
    }
}

async fn response_body_string(response: Response, context: &str) -> Result<String, OpenCodeError> {
    let bytes = response
        .bytes()
        .await
        .map_err(|e| OpenCodeError::Http(format!("{context}: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn subscribe_open_code_events(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
) -> OpenCodeEventSubscription {
    let client = inner.client.clone();
    let auth = inner.auth.clone();
    let server_url = inner.server_url.clone();
    let opencode_session_id = opencode_session_id.to_string();
    let (tx, rx) = mpsc::channel(16);
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        let response = match server_request(
            &client,
            auth.as_ref(),
            reqwest::Method::GET,
            format!("{server_url}/event"),
        )
        .send()
        .await
        {
            Ok(response) => response,
            Err(_) => {
                let _ = ready_tx.send(false);
                return;
            }
        };

        if !response.status().is_success() {
            let _ = ready_tx.send(false);
            return;
        }
        let _ = ready_tx.send(true);

        let mut response = response;
        let mut buffer = String::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) | Err(_) => break,
            };
            buffer.push_str(
                &String::from_utf8_lossy(&chunk)
                    .replace("\r\n", "\n")
                    .replace('\r', "\n"),
            );
            while let Some(event) = next_sse_json_event(&mut buffer) {
                if let Some(server_event) =
                    normalize_open_code_server_event(&opencode_session_id, &event)
                {
                    if tx.send(server_event).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    OpenCodeEventSubscription {
        rx,
        ready: Some(ready_rx),
        handle,
    }
}

fn next_sse_json_event(buffer: &mut String) -> Option<Value> {
    let end = buffer.find("\n\n")?;
    let raw = buffer[..end].to_string();
    buffer.drain(..end + 2);
    let data = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&data).ok()
}

fn normalize_open_code_server_event(
    opencode_session_id: &str,
    event: &Value,
) -> Option<OpenCodeServerEvent> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    let properties = event.get("properties").unwrap_or(event);
    if !opencode_event_matches_session(properties, opencode_session_id) {
        return None;
    }

    match event_type {
        "session.error" => {
            let error_value = properties.get("error").or_else(|| event.get("error"))?;
            let error = opencode_error_text(error_value)?;
            Some(OpenCodeServerEvent {
                message: format!("OpenCode session error: {error}"),
                failure: true,
                dedupe_key: opencode_error_dedupe_key(error_value),
            })
        }
        "message.updated" => opencode_message_error(properties).map(|error| OpenCodeServerEvent {
            message: error.message,
            failure: true,
            dedupe_key: error.dedupe_key,
        }),
        _ => None,
    }
}

fn opencode_event_matches_session(value: &Value, opencode_session_id: &str) -> bool {
    value
        .get("sessionID")
        .or_else(|| value.get("sessionId"))
        .or_else(|| value.get("session_id"))
        .and_then(Value::as_str)
        .map(|session_id| session_id == opencode_session_id)
        .unwrap_or(true)
}

fn output_line(message: String) -> String {
    if message.ends_with('\n') {
        message
    } else {
        format!("{message}\n")
    }
}

fn looks_like_html_shell(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with("<!doctype html") || trimmed.starts_with("<html")
}

async fn server_health(
    client: &Client,
    server_url: &str,
    auth: Option<&OpenCodeServerAuth>,
) -> Result<bool, OpenCodeError> {
    let response = server_request(
        client,
        auth,
        reqwest::Method::GET,
        format!("{server_url}/global/health"),
    )
    .send()
    .await
    .map_err(|e| OpenCodeError::Http(e.to_string()))?;
    Ok(response.status().is_success())
}

async fn create_server_session(
    client: &Client,
    server_url: &str,
    auth: Option<&OpenCodeServerAuth>,
    spec: &SessionSpec,
) -> Result<String, OpenCodeError> {
    let response = server_request(
        client,
        auth,
        reqwest::Method::POST,
        format!("{server_url}/session"),
    )
    .json(&serde_json::json!({
        "title": format!("{} {}", spec.role, spec.agent_id.as_str()),
    }))
    .send()
    .await
    .map_err(|e| OpenCodeError::SessionCreate(e.to_string()))?;

    let status = response.status();
    let body = response_body_string(response, "session create body")
        .await
        .map_err(|e| OpenCodeError::SessionCreate(e.to_string()))?;
    if !status.is_success() {
        return Err(OpenCodeError::SessionCreate(format!(
            "session create failed with {status}: {body}"
        )));
    }

    let value: Value =
        serde_json::from_str(&body).map_err(|e| OpenCodeError::SessionCreate(e.to_string()))?;
    extract_string_field(&value, &["id", "sessionID", "sessionId"])
        .or_else(|| {
            value.get("session").and_then(|session| {
                extract_string_field(session, &["id", "sessionID", "sessionId"])
            })
        })
        .ok_or_else(|| {
            OpenCodeError::SessionCreate(format!("missing session id in response: {body}"))
        })
}

async fn abort_open_code_session(
    inner: &Arc<OpenCodeServerSessionInner>,
) -> Result<(), OpenCodeError> {
    let Some(session_id) = inner.opencode_session_id.lock().await.clone() else {
        return Ok(());
    };
    let response = server_request(
        &inner.client,
        inner.auth.as_ref(),
        reqwest::Method::POST,
        format!("{}/session/{session_id}/abort", inner.server_url),
    )
    .send()
    .await
    .map_err(|e| OpenCodeError::Http(e.to_string()))?;
    if response.status().is_success() || response.status() == StatusCode::CONFLICT {
        Ok(())
    } else {
        let status = response.status();
        let body = response_body_string(response, "abort response body")
            .await
            .unwrap_or_default();
        Err(OpenCodeError::Http(format!(
            "abort failed with {status}: {body}"
        )))
    }
}

async fn send_server_message(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
    prompt: &str,
) -> Result<String, OpenCodeError> {
    let mut session_id = opencode_session_id.to_string();
    let mut last_error: Option<OpenCodeError> = None;

    for attempt in 0..SERVER_MESSAGE_RETRY_ATTEMPTS {
        match send_server_message_once(inner, &session_id, prompt).await {
            Ok(()) => return Ok(session_id),
            Err(err) => {
                let should_retry =
                    attempt + 1 < SERVER_MESSAGE_RETRY_ATTEMPTS && should_retry_turn_error(&err);
                last_error = Some(err);
                if !should_retry {
                    break;
                }

                session_id = recreate_server_session(inner).await?;
                sleep(Duration::from_millis(SERVER_MESSAGE_RETRY_BACKOFF_MS)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        OpenCodeError::Turn("OpenCode message failed for an unknown reason".to_string())
    }))
}

async fn send_server_message_once(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
    prompt: &str,
) -> Result<(), OpenCodeError> {
    let model = inner.model.lock().await.clone();
    let body = opencode_prompt_body(prompt, model.as_ref());
    let response = server_request(
        &inner.client,
        inner.auth.as_ref(),
        reqwest::Method::POST,
        format!(
            "{}/session/{}/prompt_async",
            inner.server_url, opencode_session_id
        ),
    )
    .json(&body)
    .send()
    .await
    .map_err(|e| OpenCodeError::Turn(e.to_string()))?;

    let status = response.status();
    let body = response_body_string(response, "prompt_async response body")
        .await
        .map_err(|e| OpenCodeError::Turn(e.to_string()))?;

    if !status.is_success() {
        return Err(OpenCodeError::Turn(format!(
            "OpenCode prompt_async failed with {status}: {body}"
        )));
    }

    Ok(())
}

async fn recreate_server_session(
    inner: &Arc<OpenCodeServerSessionInner>,
) -> Result<String, OpenCodeError> {
    wait_for_server(&inner.client, &inner.server_url, inner.auth.as_ref(), None).await?;
    let new_session_id = create_server_session(
        &inner.client,
        &inner.server_url,
        inner.auth.as_ref(),
        &inner.spec,
    )
    .await?;
    *inner.opencode_session_id.lock().await = Some(new_session_id.clone());
    Ok(new_session_id)
}

fn should_retry_turn_error(err: &OpenCodeError) -> bool {
    let message = match err {
        OpenCodeError::Turn(message) | OpenCodeError::Http(message) => message.to_ascii_lowercase(),
        OpenCodeError::NotRunning => return false,
        OpenCodeError::Spawn(_)
        | OpenCodeError::MissingServerUrl
        | OpenCodeError::InvalidServerUrl(_)
        | OpenCodeError::SessionCreate(_) => return false,
    };

    message.contains("error decoding response body")
        || message.contains("connection reset")
        || message.contains("broken pipe")
        || message.contains("unexpected eof")
        || message.contains("channel closed")
        || message.contains("timed out")
        || message.contains("timeout")
        || message.contains("404")
        || message.contains("not found")
        || message.contains("500")
        || message.contains("internal server error")
        || message.contains("unknownerror")
        || message.contains("unexpected server error")
        || message.contains("502")
        || message.contains("503")
        || message.contains("504")
}

fn extract_string_field(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
    })
}

fn opencode_step_success(part: &OpenCodeHttpPart) -> bool {
    if let Some(status) = part
        .state
        .as_ref()
        .and_then(|state| state.status.as_deref())
    {
        return !opencode_status_is_failure(status);
    }
    part.reason
        .as_deref()
        .map(|reason| !opencode_status_is_failure(reason))
        .unwrap_or(true)
}

fn opencode_status_is_failure(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "error" | "failed" | "failure" | "cancelled" | "canceled"
    ) || normalized.contains("error")
        || normalized.contains("failed")
}

fn opencode_retry_error_message(part: &OpenCodeHttpPart) -> Option<String> {
    let error = part.error.as_ref().and_then(opencode_error_text)?;
    Some(match part.attempt {
        Some(attempt) => format!("OpenCode retry attempt {attempt} failed: {error}"),
        None => format!("OpenCode retry failed: {error}"),
    })
}

struct OpenCodeErrorMessage {
    message: String,
    dedupe_key: String,
}

fn opencode_error_text(value: &Value) -> Option<String> {
    if let Some(text) = value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    if let Some(object) = value.as_object() {
        let mut parts = Vec::new();
        push_unique_error_part(&mut parts, object_string_field(object, "name"));
        push_unique_error_part(
            &mut parts,
            object
                .get("statusCode")
                .or_else(|| object.get("status_code"))
                .or_else(|| object.get("status"))
                .and_then(Value::as_u64)
                .map(|status| format!("status {status}")),
        );
        for field in [
            "message", "error", "detail", "details", "body", "data", "cause", "metadata",
        ] {
            if let Some(field_value) = object.get(field) {
                push_unique_error_part(&mut parts, opencode_error_text(field_value));
            }
        }
        if let Some(response_body) = object
            .get("responseBody")
            .or_else(|| object.get("response_body"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            if let Some(parsed) = parse_json_error_response(response_body) {
                push_unique_error_part(&mut parts, Some(parsed));
            }
            push_unique_error_part(&mut parts, Some(format!("response body: {response_body}")));
        }
        if !parts.is_empty() {
            if parts.len() == 1 && object.contains_key("name") {
                parts.push(format!(
                    "no detail in OpenCode error payload; raw_error={}",
                    compact_json(value, 700)
                ));
            }
            return Some(parts.join(": "));
        }
    }
    Some(compact_json(value, 700)).filter(|text| text != "null")
}

fn opencode_message_error(message: &Value) -> Option<OpenCodeErrorMessage> {
    let info = message_info_value(message);
    let error = info
        .get("error")
        .or_else(|| message.get("error"))
        .or_else(|| message.get("message").and_then(|value| value.get("error")))?;
    let text = opencode_error_text(error)?;
    let role = message_role(message).unwrap_or("message");
    Some(OpenCodeErrorMessage {
        message: format!("OpenCode {role} error: {text}"),
        dedupe_key: opencode_error_dedupe_key(error),
    })
}

fn opencode_message_error_message(message: &Value) -> Option<String> {
    opencode_message_error(message).map(|error| error.message)
}

fn opencode_error_dedupe_key(error: &Value) -> String {
    opencode_error_text(error).unwrap_or_else(|| compact_json(error, 700))
}

fn push_unique_error_part(parts: &mut Vec<String>, text: Option<String>) {
    let Some(text) = text
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "null")
    else {
        return;
    };
    if !parts.iter().any(|part| part == &text) {
        parts.push(text);
    }
}

fn object_string_field(object: &serde_json::Map<String, Value>, field: &str) -> Option<String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn parse_json_error_response(response_body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(response_body).ok()?;
    value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn compact_json(value: &Value, max_chars: usize) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn opencode_event_failure_message(event: &AdapterEvent) -> Option<String> {
    match event {
        AdapterEvent::Output { text, .. } if text.starts_with("OpenCode retry ") => {
            Some(text.trim().to_string())
        }
        AdapterEvent::Output { text, .. } if text.starts_with("OpenCode session error: ") => {
            Some(text.trim().to_string())
        }
        AdapterEvent::Output { text, .. }
            if text.starts_with("OpenCode ") && text.contains(" error: ") =>
        {
            Some(text.trim().to_string())
        }
        AdapterEvent::OperationCompleted {
            operation,
            success: false,
            ..
        } => Some(format!("OpenCode {operation} failed")),
        AdapterEvent::ToolCallCompleted {
            tool_name, status, ..
        } if opencode_status_is_failure(status) => Some(format!(
            "OpenCode tool {tool_name} failed with status {status}"
        )),
        _ => None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn normalize_message_response(session_id: &SessionId, response: &Value) -> Vec<AdapterEvent> {
    if let Some(messages) = extract_messages(response) {
        let mut result = Vec::new();
        for message in messages {
            result.extend(normalize_message_value(session_id, &message));
        }
        if !result.is_empty() {
            return result;
        }
    }

    let result = normalize_message_value(session_id, response);
    if !result.is_empty() {
        return result;
    }

    let mut result = Vec::new();
    if let Some(text) = response
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        result.push(AdapterEvent::Output {
            text: text.to_string(),
        });
    } else {
        result.push(AdapterEvent::Progress {
            message: "OpenCode turn completed".to_string(),
            percent: None,
        });
    }

    result
}

#[cfg_attr(not(test), allow(dead_code))]
fn message_tokens_used(response: &Value) -> u64 {
    let parts = response
        .get("parts")
        .or_else(|| response.get("message").and_then(|m| m.get("parts")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    parts
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("step-finish" | "step_finish")
            )
        })
        .filter_map(|part| part.get("tokens").and_then(tokens_from_value))
        .sum()
}

fn tokens_from_value(value: &Value) -> Option<u64> {
    token_field(
        value,
        &[
            "total",
            "tokensUsed",
            "tokens_used",
            "totalTokens",
            "total_tokens",
        ],
    )
    .or_else(|| {
        sum_token_fields(
            value,
            &[
                "input",
                "inputTokens",
                "input_tokens",
                "output",
                "outputTokens",
                "output_tokens",
                "reasoning",
                "reasoningTokens",
                "reasoning_tokens",
            ],
        )
    })
}

fn token_field(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(token_value))
}

fn sum_token_fields(value: &Value, fields: &[&str]) -> Option<u64> {
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

fn token_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
}

fn normalize_message_value(_session_id: &SessionId, response: &Value) -> Vec<AdapterEvent> {
    if !message_should_emit(response) {
        return Vec::new();
    }

    let mut result = Vec::new();
    let parts = response
        .get("parts")
        .or_else(|| response.get("message").and_then(|m| m.get("parts")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for part_value in parts {
        let Ok(part) = serde_json::from_value::<OpenCodeHttpPart>(part_value) else {
            continue;
        };
        match part.part_type.as_deref() {
            Some("retry") => {
                if let Some(text) = opencode_retry_error_message(&part) {
                    result.push(AdapterEvent::Output { text });
                }
                continue;
            }
            Some("step-start" | "step_start") => {
                result.push(AdapterEvent::OperationStarted {
                    operation: "step".to_string(),
                });
                continue;
            }
            Some("step-finish" | "step_finish") => {
                result.push(AdapterEvent::OperationCompleted {
                    operation: "step".to_string(),
                    success: opencode_step_success(&part),
                });
                continue;
            }
            _ => {}
        }

        let is_tool_part = opencode_http_part_is_tool_part(&part);
        if !is_tool_part {
            if let Some(text) = part
                .text
                .as_deref()
                .or(part.content.as_deref())
                .filter(|text| !text.is_empty())
            {
                result.push(AdapterEvent::Output {
                    text: text.to_string(),
                });
            }
        }

        let tool_id = part
            .call_id
            .as_deref()
            .or(part.id.as_deref())
            .unwrap_or("tool")
            .to_string();
        let tool_name = part
            .tool_name
            .as_deref()
            .or(part.part_type.as_deref())
            .or_else(|| part.state.as_ref().and_then(|state| state.title.as_deref()))
            .unwrap_or("tool")
            .to_string();
        if is_tool_part {
            let started_details = opencode_tool_details(&part, true);
            result.push(AdapterEvent::ToolCallStarted {
                tool_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                details: started_details,
            });
            let status = part
                .state
                .as_ref()
                .and_then(|state| state.status.clone())
                .unwrap_or_else(|| "completed".to_string());
            if !matches!(status.as_str(), "started" | "running" | "in_progress") {
                let completed_details = opencode_tool_details(&part, false);
                result.push(AdapterEvent::ToolCallCompleted {
                    tool_id,
                    tool_name,
                    status,
                    details: completed_details,
                });
            }
        }
    }

    if result.is_empty() {
        let maybe_text = response
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| response.get("content"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        if let Some(text) = maybe_text {
            result.push(AdapterEvent::Output {
                text: text.to_string(),
            });
        }
    }

    result
}

fn opencode_http_part_is_tool_part(part: &OpenCodeHttpPart) -> bool {
    part.call_id.is_some()
        || part.tool_name.is_some()
        || part.state.is_some()
        || matches!(part.part_type.as_deref(), Some("tool" | "tool_use"))
}

fn opencode_tool_details(part: &OpenCodeHttpPart, started: bool) -> Option<Value> {
    let mut object = serde_json::Map::new();
    if started {
        if let Some(input) = part
            .input
            .as_ref()
            .or_else(|| part.state.as_ref().and_then(|state| state.input.as_ref()))
        {
            object.insert("input".to_string(), input.clone());
        }
    } else if let Some(output) = part
        .output
        .as_ref()
        .or_else(|| part.state.as_ref().and_then(|state| state.output.as_ref()))
        .or_else(|| part.state.as_ref().and_then(|state| state.result.as_ref()))
    {
        object.insert("output".to_string(), output.clone());
    } else if let Some(error) = part
        .error
        .as_ref()
        .or_else(|| part.state.as_ref().and_then(|state| state.error.as_ref()))
    {
        object.insert("error".to_string(), error.clone());
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

fn message_should_emit(message: &Value) -> bool {
    !matches!(
        message_role(message),
        Some("user" | "system" | "tool" | "developer")
    )
}

fn message_info_value(message: &Value) -> &Value {
    message
        .get("info")
        .or_else(|| message.get("message"))
        .unwrap_or(message)
}

fn message_role(message: &Value) -> Option<&str> {
    message_info_value(message)
        .get("role")
        .and_then(Value::as_str)
        .or_else(|| message.get("role").and_then(Value::as_str))
        .or_else(|| message.get("type").and_then(Value::as_str))
        .or_else(|| {
            message
                .get("message")
                .and_then(|value| value.get("role"))
                .and_then(Value::as_str)
        })
}

fn extract_messages(response: &Value) -> Option<Vec<Value>> {
    response
        .as_array()
        .cloned()
        .or_else(|| response.get("messages").and_then(Value::as_array).cloned())
}

#[derive(Default)]
struct OpenCodeSeenMessageParts {
    event_keys: HashSet<String>,
    text_parts: HashMap<String, String>,
}

async fn fetch_seen_message_parts(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
) -> Result<OpenCodeSeenMessageParts, OpenCodeError> {
    let messages = fetch_session_messages(inner, opencode_session_id).await?;
    let mut seen = OpenCodeSeenMessageParts::default();
    for (message_index, message) in messages.iter().enumerate() {
        record_seen_message_parts(message, message_index, &mut seen);
    }
    Ok(seen)
}

async fn fetch_new_message_events(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
    seen_messages: &mut OpenCodeSeenMessageParts,
    prompt_echo: &str,
) -> Result<(Vec<AdapterEvent>, bool, u64), OpenCodeError> {
    let messages = fetch_session_messages(inner, opencode_session_id).await?;
    let mut events = Vec::new();
    let mut saw_model_activity = false;
    let mut tokens_used = 0u64;
    for (message_index, message) in messages.into_iter().enumerate() {
        if !message_should_emit(&message) {
            continue;
        }
        let (mut message_events, message_tokens, message_activity) =
            normalize_new_message_parts(&message, message_index, seen_messages, prompt_echo);
        if message_activity || message_tokens > 0 {
            saw_model_activity = true;
        }
        tokens_used = tokens_used.saturating_add(message_tokens);
        events.append(&mut message_events);
    }
    Ok((events, saw_model_activity, tokens_used))
}

fn record_seen_message_parts(
    message: &Value,
    message_index: usize,
    seen: &mut OpenCodeSeenMessageParts,
) {
    if !message_should_emit(message) {
        return;
    }

    if opencode_message_error_message(message).is_some() {
        seen.event_keys.insert(format!(
            "{}:message-error",
            message_identity(message, message_index)
        ));
    }

    let parts = message_parts(message);
    if parts.is_empty() {
        if let Some(text) = message_content_text(message) {
            seen.text_parts.insert(
                format!("{}:content", message_identity(message, message_index)),
                text.to_string(),
            );
        }
        return;
    }

    for (part_index, part_value) in parts.iter().enumerate() {
        let Ok(part) = serde_json::from_value::<OpenCodeHttpPart>(part_value.clone()) else {
            continue;
        };
        let base_key = message_part_base_key(message, message_index, part_value, part_index);
        match part.part_type.as_deref() {
            Some("retry") => {
                if part.error.is_some() {
                    seen.event_keys.insert(format!("{base_key}:retry-error"));
                }
            }
            Some("step-start" | "step_start") => {
                seen.event_keys.insert(format!("{base_key}:step-start"));
            }
            Some("step-finish" | "step_finish") => {
                seen.event_keys.insert(format!("{base_key}:step-finish"));
            }
            _ if opencode_http_part_is_tool_part(&part) => {
                seen.event_keys.insert(format!("{base_key}:tool-start"));
                let status = part
                    .state
                    .as_ref()
                    .and_then(|state| state.status.as_deref())
                    .unwrap_or("completed");
                if !matches!(status, "started" | "running" | "in_progress") {
                    seen.event_keys
                        .insert(format!("{base_key}:tool-complete:{status}"));
                }
            }
            _ => {
                if let Some(text) = part
                    .text
                    .as_deref()
                    .or(part.content.as_deref())
                    .filter(|text| !text.is_empty())
                {
                    seen.text_parts
                        .insert(format!("{base_key}:text"), text.to_string());
                }
            }
        }
    }
}

fn normalize_new_message_parts(
    message: &Value,
    message_index: usize,
    seen: &mut OpenCodeSeenMessageParts,
    prompt_echo: &str,
) -> (Vec<AdapterEvent>, u64, bool) {
    let mut events = Vec::new();
    let mut tokens_used = 0u64;
    let mut saw_model_activity = false;
    let parts = message_parts(message);
    let message_id = message_identity(message, message_index);

    if let Some(text) = opencode_message_error_message(message) {
        if seen
            .event_keys
            .insert(format!("{message_id}:message-error"))
        {
            events.push(AdapterEvent::Output { text });
            saw_model_activity = true;
        }
    }

    if parts.is_empty() {
        if let Some(text) = message_content_text(message) {
            let before = events.len();
            push_text_delta(&mut events, seen, format!("{message_id}:content"), text);
            if events.len() > before && !text_is_prompt_echo(text, prompt_echo) {
                saw_model_activity = true;
            }
        }
        return (events, tokens_used, saw_model_activity);
    }

    for (part_index, part_value) in parts.iter().enumerate() {
        let Ok(part) = serde_json::from_value::<OpenCodeHttpPart>(part_value.clone()) else {
            continue;
        };
        let base_key = message_part_base_key(message, message_index, part_value, part_index);
        match part.part_type.as_deref() {
            Some("retry") => {
                if let Some(text) = opencode_retry_error_message(&part) {
                    if seen.event_keys.insert(format!("{base_key}:retry-error")) {
                        events.push(AdapterEvent::Output { text });
                        saw_model_activity = true;
                    }
                }
                continue;
            }
            Some("step-start" | "step_start") => {
                if seen.event_keys.insert(format!("{base_key}:step-start")) {
                    events.push(AdapterEvent::OperationStarted {
                        operation: "step".to_string(),
                    });
                    saw_model_activity = true;
                }
                continue;
            }
            Some("step-finish" | "step_finish") => {
                if seen.event_keys.insert(format!("{base_key}:step-finish")) {
                    tokens_used = tokens_used.saturating_add(
                        part_value
                            .get("tokens")
                            .and_then(tokens_from_value)
                            .unwrap_or(0),
                    );
                    events.push(AdapterEvent::OperationCompleted {
                        operation: "step".to_string(),
                        success: opencode_step_success(&part),
                    });
                    saw_model_activity = true;
                }
                continue;
            }
            _ => {}
        }

        if opencode_http_part_is_tool_part(&part) {
            let tool_id = part
                .call_id
                .as_deref()
                .or(part.id.as_deref())
                .unwrap_or("tool")
                .to_string();
            let tool_name = part
                .tool_name
                .as_deref()
                .or(part.part_type.as_deref())
                .or_else(|| part.state.as_ref().and_then(|state| state.title.as_deref()))
                .unwrap_or("tool")
                .to_string();
            if seen.event_keys.insert(format!("{base_key}:tool-start")) {
                events.push(AdapterEvent::ToolCallStarted {
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                    details: opencode_tool_details(&part, true),
                });
                saw_model_activity = true;
            }
            let status = part
                .state
                .as_ref()
                .and_then(|state| state.status.clone())
                .unwrap_or_else(|| "completed".to_string());
            if !matches!(status.as_str(), "started" | "running" | "in_progress")
                && seen
                    .event_keys
                    .insert(format!("{base_key}:tool-complete:{status}"))
            {
                events.push(AdapterEvent::ToolCallCompleted {
                    tool_id,
                    tool_name,
                    status,
                    details: opencode_tool_details(&part, false),
                });
                saw_model_activity = true;
            }
        } else if let Some(text) = part
            .text
            .as_deref()
            .or(part.content.as_deref())
            .filter(|text| !text.is_empty())
        {
            let before = events.len();
            push_text_delta(&mut events, seen, format!("{base_key}:text"), text);
            if events.len() > before && !text_is_prompt_echo(text, prompt_echo) {
                saw_model_activity = true;
            }
        }
    }

    (events, tokens_used, saw_model_activity)
}

fn text_is_prompt_echo(text: &str, prompt: &str) -> bool {
    let prompt = normalized_prompt_text(prompt);
    if prompt.len() < 64 {
        return false;
    }
    let text = normalized_prompt_text(text);
    !text.is_empty() && (text == prompt || prompt.starts_with(text.as_str()))
}

fn normalized_prompt_text(text: &str) -> String {
    text.trim().replace("\r\n", "\n").replace('\r', "\n")
}

fn push_text_delta(
    events: &mut Vec<AdapterEvent>,
    seen: &mut OpenCodeSeenMessageParts,
    key: String,
    text: &str,
) {
    match seen.text_parts.get_mut(&key) {
        Some(previous) if text.starts_with(previous.as_str()) => {
            if text.len() > previous.len() {
                let delta = text[previous.len()..].to_string();
                *previous = text.to_string();
                if !delta.is_empty() {
                    events.push(AdapterEvent::Output { text: delta });
                }
            }
        }
        Some(previous) => {
            if previous != text {
                *previous = text.to_string();
            }
        }
        None => {
            seen.text_parts.insert(key, text.to_string());
            events.push(AdapterEvent::Output {
                text: text.to_string(),
            });
        }
    }
}

fn message_parts(message: &Value) -> Vec<Value> {
    message
        .get("parts")
        .or_else(|| message.get("message").and_then(|m| m.get("parts")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn message_content_text(message: &Value) -> Option<&str> {
    message
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| message.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn message_identity(message: &Value, message_index: usize) -> String {
    value_string_field(
        message,
        &["id", "messageID", "messageId", "message_id", "uuid"],
    )
    .or_else(|| {
        value_string_field(
            message_info_value(message),
            &["id", "messageID", "messageId", "message_id", "uuid"],
        )
    })
    .or_else(|| {
        message.get("message").and_then(|nested| {
            value_string_field(
                nested,
                &["id", "messageID", "messageId", "message_id", "uuid"],
            )
        })
    })
    .unwrap_or_else(|| format!("index-{message_index}"))
}

fn message_part_base_key(
    message: &Value,
    message_index: usize,
    part: &Value,
    part_index: usize,
) -> String {
    let message_id = message_identity(message, message_index);
    let part_id = value_string_field(
        part,
        &[
            "id",
            "callID",
            "callId",
            "toolCallID",
            "toolCallId",
            "tool_call_id",
        ],
    )
    .unwrap_or_else(|| format!("index-{part_index}"));
    let part_type = part.get("type").and_then(Value::as_str).unwrap_or("part");
    format!("{message_id}:{part_type}:{part_id}")
}

fn value_string_field(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

async fn no_activity_timeout_details(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
    prompt: &str,
) -> String {
    let configured_model = inner
        .model
        .lock()
        .await
        .as_ref()
        .map(opencode_model_selection_label)
        .unwrap_or_else(|| "default".to_string());
    let messages = fetch_session_messages(inner, opencode_session_id)
        .await
        .unwrap_or_default();
    let mut role_counts: HashMap<String, usize> = HashMap::new();
    let mut models = HashSet::new();
    let mut errors = Vec::new();
    let mut prompt_echo_seen = false;

    for message in &messages {
        let role = message_role(message).unwrap_or("unknown").to_string();
        *role_counts.entry(role).or_default() += 1;
        if let Some(model) = message_model_label(message) {
            models.insert(model);
        }
        if let Some(error) = opencode_message_error_message(message) {
            errors.push(error);
        }
        for part in message_parts(message) {
            if let Some(text) = part
                .get("text")
                .or_else(|| part.get("content"))
                .and_then(Value::as_str)
            {
                if text_is_prompt_echo(text, prompt) {
                    prompt_echo_seen = true;
                }
            }
        }
    }

    let mut role_counts = role_counts.into_iter().collect::<Vec<_>>();
    role_counts.sort_by(|(left, _), (right, _)| left.cmp(right));
    let roles = role_counts
        .into_iter()
        .map(|(role, count)| format!("{role}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut models = models.into_iter().collect::<Vec<_>>();
    models.sort();
    let mut details = format!(
        "configured_model={configured_model}; prompt_chars={}; messages={}; roles={}; observed_models={}; prompt_echo_seen={}",
        prompt.chars().count(),
        messages.len(),
        if roles.is_empty() { "none" } else { &roles },
        if models.is_empty() {
            "none".to_string()
        } else {
            models.join(",")
        },
        prompt_echo_seen,
    );
    if !errors.is_empty() {
        details.push_str("; message_errors=");
        details.push_str(&errors.join(" | "));
    }
    details
}

fn opencode_model_selection_label(model: &OpenCodeModelSelection) -> String {
    format!("{}/{}", model.provider_id, model.model_id)
}

fn message_model_label(message: &Value) -> Option<String> {
    let info = message_info_value(message);
    if let Some(model) = info.get("model") {
        let provider = model
            .get("providerID")
            .or_else(|| model.get("provider_id"))
            .and_then(Value::as_str)?;
        let model_id = model
            .get("modelID")
            .or_else(|| model.get("modelId"))
            .or_else(|| model.get("model_id"))
            .or_else(|| model.get("id"))
            .and_then(Value::as_str)?;
        return Some(format!("{provider}/{model_id}"));
    }
    let provider = info
        .get("providerID")
        .or_else(|| info.get("provider_id"))
        .and_then(Value::as_str)?;
    let model_id = info
        .get("modelID")
        .or_else(|| info.get("modelId"))
        .or_else(|| info.get("model_id"))
        .and_then(Value::as_str)?;
    Some(format!("{provider}/{model_id}"))
}

async fn fetch_session_messages(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
) -> Result<Vec<Value>, OpenCodeError> {
    let mut session_id = opencode_session_id.to_string();
    let mut last_error: Option<OpenCodeError> = None;

    for attempt in 0..SERVER_FETCH_RETRY_ATTEMPTS {
        let response = match server_request(
            &inner.client,
            inner.auth.as_ref(),
            reqwest::Method::GET,
            format!(
                "{}/session/{}/message?limit={}",
                inner.server_url, session_id, SESSION_MESSAGE_LIMIT
            ),
        )
        .send()
        .await
        {
            Ok(response) => response,
            Err(err) => {
                let wrapped = OpenCodeError::Http(err.to_string());
                let retryable =
                    attempt + 1 < SERVER_FETCH_RETRY_ATTEMPTS && should_retry_turn_error(&wrapped);
                last_error = Some(wrapped);
                if retryable {
                    sleep(Duration::from_millis(SERVER_FETCH_RETRY_BACKOFF_MS)).await;
                    continue;
                }
                break;
            }
        };

        let status = response.status();
        let body = match response_body_string(response, "session message body").await {
            Ok(body) => body,
            Err(err) => {
                let retryable =
                    attempt + 1 < SERVER_FETCH_RETRY_ATTEMPTS && should_retry_turn_error(&err);
                last_error = Some(err);
                if retryable {
                    sleep(Duration::from_millis(SERVER_FETCH_RETRY_BACKOFF_MS)).await;
                    continue;
                }
                break;
            }
        };

        if status == StatusCode::NOT_FOUND || looks_like_html_shell(&body) {
            if let Some(latest) = resolve_latest_matching_session_id(inner).await? {
                if latest != session_id {
                    session_id = latest.clone();
                    *inner.opencode_session_id.lock().await = Some(latest);
                    sleep(Duration::from_millis(SERVER_FETCH_RETRY_BACKOFF_MS)).await;
                    continue;
                }
            }
        }

        if !status.is_success() {
            let err = OpenCodeError::Http(format!(
                "session message fetch failed with {status}: {body}"
            ));
            let retryable =
                attempt + 1 < SERVER_FETCH_RETRY_ATTEMPTS && should_retry_turn_error(&err);
            last_error = Some(err);
            if retryable {
                sleep(Duration::from_millis(SERVER_FETCH_RETRY_BACKOFF_MS)).await;
                continue;
            }
            break;
        }

        let value: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(err) => {
                let wrapped = OpenCodeError::Http(format!(
                    "failed to parse OpenCode session messages: {err}: {body}"
                ));
                let retryable = attempt + 1 < SERVER_FETCH_RETRY_ATTEMPTS
                    && (should_retry_turn_error(&wrapped) || looks_like_html_shell(&body));
                last_error = Some(wrapped);
                if retryable {
                    sleep(Duration::from_millis(SERVER_FETCH_RETRY_BACKOFF_MS)).await;
                    continue;
                }
                break;
            }
        };

        return Ok(extract_messages(&value).unwrap_or_default());
    }

    Err(last_error.unwrap_or_else(|| {
        OpenCodeError::Http("session message fetch failed for an unknown reason".to_string())
    }))
}

async fn fetch_session_busy(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
) -> Result<bool, OpenCodeError> {
    if let Ok(Some(busy)) = fetch_session_busy_direct(inner, opencode_session_id).await {
        return Ok(busy);
    }

    let response = server_request(
        &inner.client,
        inner.auth.as_ref(),
        reqwest::Method::GET,
        format!("{}/session/status", inner.server_url),
    )
    .send()
    .await
    .map_err(|e| OpenCodeError::Http(e.to_string()))?;

    let status = response.status();
    let body = response_body_string(response, "session status body").await?;
    if !status.is_success() {
        return Err(OpenCodeError::Http(format!(
            "session status fetch failed with {status}: {body}"
        )));
    }

    let value: Value = serde_json::from_str(&body).map_err(|e| {
        OpenCodeError::Http(format!(
            "failed to parse OpenCode session status: {e}: {body}"
        ))
    })?;

    if let Some(busy) = parse_session_busy_value(&value, Some(opencode_session_id)) {
        return Ok(busy);
    }

    Ok(false)
}

async fn fetch_session_busy_direct(
    inner: &Arc<OpenCodeServerSessionInner>,
    opencode_session_id: &str,
) -> Result<Option<bool>, OpenCodeError> {
    let response = server_request(
        &inner.client,
        inner.auth.as_ref(),
        reqwest::Method::GET,
        format!(
            "{}/session/{}/status",
            inner.server_url, opencode_session_id
        ),
    )
    .send()
    .await
    .map_err(|e| OpenCodeError::Http(e.to_string()))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let status = response.status();
    let body = response_body_string(response, "direct session status body").await?;
    if looks_like_html_shell(&body) {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(OpenCodeError::Http(format!(
            "session status fetch failed with {status}: {body}"
        )));
    }

    let value: Value = serde_json::from_str(&body).map_err(|e| {
        OpenCodeError::Http(format!(
            "failed to parse OpenCode session status: {e}: {body}"
        ))
    })?;
    Ok(parse_session_busy_value(&value, Some(opencode_session_id)))
}

fn parse_session_busy_value(value: &Value, expected_session_id: Option<&str>) -> Option<bool> {
    if let Some(array) = value.as_array() {
        for entry in array {
            let matches_session = expected_session_id.is_none_or(|expected| {
                extract_string_field(entry, &["id", "sessionID", "sessionId"]).as_deref()
                    == Some(expected)
            });
            if matches_session {
                if let Some(busy) = parse_single_status(entry) {
                    return Some(busy);
                }
            }
        }
        return None;
    }

    if let Some(object) = value.as_object() {
        if let Some(expected) = expected_session_id {
            if let Some(entry) = object.get(expected) {
                return parse_single_status(entry);
            }
        }
    }

    if let Some(sessions) = value.get("sessions").and_then(Value::as_array) {
        return parse_session_busy_value(&Value::Array(sessions.clone()), expected_session_id);
    }

    parse_single_status(value)
}

fn parse_single_status(value: &Value) -> Option<bool> {
    if let Some(busy) = value.get("busy").and_then(Value::as_bool) {
        return Some(busy);
    }
    let status = extract_string_field(value, &["type", "status", "state"])?;
    let normalized = status.trim().to_ascii_lowercase();
    Some(matches!(
        normalized.as_str(),
        "busy" | "running" | "working" | "processing" | "active"
    ))
}

async fn resolve_latest_matching_session_id(
    inner: &Arc<OpenCodeServerSessionInner>,
) -> Result<Option<String>, OpenCodeError> {
    let response = server_request(
        &inner.client,
        inner.auth.as_ref(),
        reqwest::Method::GET,
        format!("{}/session", inner.server_url),
    )
    .send()
    .await
    .map_err(|e| OpenCodeError::Http(e.to_string()))?;

    let status = response.status();
    let body = response_body_string(response, "session listing body").await?;
    if !status.is_success() {
        return Err(OpenCodeError::Http(format!(
            "session listing failed with {status}: {body}"
        )));
    }

    let sessions: Vec<OpenCodeSessionSummary> = serde_json::from_str(&body).map_err(|e| {
        OpenCodeError::Http(format!(
            "failed to parse OpenCode session listing: {e}: {body}"
        ))
    })?;

    let expected_dir = inner.spec.worktree_path.clone();
    let expected_title = format!("{} {}", inner.spec.role, inner.spec.agent_id.as_str());

    Ok(sessions
        .into_iter()
        .filter(|session| session.directory.as_deref() == Some(expected_dir.as_str()))
        .filter(|session| session.title.as_deref() == Some(expected_title.as_str()))
        .max_by_key(|session| {
            session
                .time
                .as_ref()
                .and_then(|time| time.updated.or(time.created))
                .unwrap_or(0)
        })
        .map(|session| session.id))
}

#[cfg(test)]
fn normalize_stream_event(
    _session_id: &SessionId,
    event: &OpenCodeStreamEvent,
) -> Vec<AdapterEvent> {
    let mut result = Vec::new();
    match event.event_type.as_str() {
        "step_start" => result.push(AdapterEvent::OperationStarted {
            operation: "step".to_string(),
        }),
        "step_finish" => result.push(AdapterEvent::OperationCompleted {
            operation: "step".to_string(),
            success: event
                .part
                .as_ref()
                .and_then(|part| part.reason.as_deref())
                .map(|reason| reason != "error")
                .unwrap_or(true),
        }),
        "text" => {
            if let Some(text) = event
                .part
                .as_ref()
                .and_then(|part| part.text.as_deref())
                .filter(|text| !text.is_empty())
            {
                result.push(AdapterEvent::Output {
                    text: text.to_string(),
                });
            }
        }
        "tool_use" => {
            if let Some(part) = &event.part {
                let tool_id = part
                    .call_id
                    .as_deref()
                    .or(part.id.as_deref())
                    .unwrap_or("tool")
                    .to_string();
                let tool_name = part
                    .tool_name
                    .as_deref()
                    .or_else(|| part.state.as_ref().and_then(|state| state.title.as_deref()))
                    .unwrap_or("tool")
                    .to_string();
                result.push(AdapterEvent::ToolCallStarted {
                    tool_id: tool_id.clone(),
                    tool_name: tool_name.clone(),
                    details: None,
                });
                result.push(AdapterEvent::ToolCallCompleted {
                    tool_id,
                    tool_name,
                    status: part
                        .state
                        .as_ref()
                        .and_then(|state| state.status.clone())
                        .unwrap_or_else(|| "completed".to_string()),
                    details: None,
                });
            }
        }
        "reasoning" => {}
        other => {
            if let Some(text) = event
                .part
                .as_ref()
                .and_then(|part| part.text.as_deref())
                .filter(|text| !text.is_empty())
            {
                result.push(AdapterEvent::Output {
                    text: text.to_string(),
                });
            } else if other != "session.status" {
                result.push(AdapterEvent::Progress {
                    message: format!("OpenCode event: {other}"),
                    percent: None,
                });
            }
        }
    }
    result
}

fn server_url_from_launch(
    args: &[String],
    env: &[(String, String)],
) -> Result<String, OpenCodeError> {
    if let Some((_, value)) = env
        .iter()
        .find(|(key, _)| key == "BREHON_OPENCODE_SERVER_URL")
        .filter(|(_, value)| !value.trim().is_empty())
    {
        return Ok(value.clone());
    }

    let mut idx = 0usize;
    while idx < args.len() {
        if args[idx] == "--port" {
            if let Some(port) = args.get(idx + 1) {
                return Ok(format!("http://127.0.0.1:{port}"));
            }
        }
        idx += 1;
    }

    Err(OpenCodeError::MissingServerUrl)
}

async fn wait_for_server(
    client: &Client,
    server_url: &str,
    auth: Option<&OpenCodeServerAuth>,
    process: Option<&AgentProcess>,
) -> Result<(), OpenCodeError> {
    let _ = parse_host_port(server_url)?;
    let deadline = server_ready_timeout();
    let started_at = tokio::time::Instant::now();
    let mut last_health_error: Option<String> = None;
    loop {
        if let Some(process) = process {
            if !process.is_alive() {
                let output = process_output_summary(process).await;
                return Err(OpenCodeError::Spawn(with_process_output(
                    format!("OpenCode server process exited before readiness at {server_url}"),
                    output.as_deref(),
                )));
            }
        }

        match timeout(
            Duration::from_millis(SERVER_READY_HEALTH_TIMEOUT_MS),
            server_health(client, server_url, auth),
        )
        .await
        {
            Ok(Ok(true)) => return Ok(()),
            Ok(Ok(false)) => {}
            Ok(Err(err)) => last_health_error = Some(err.to_string()),
            Err(_) => {
                last_health_error = Some(format!(
                    "health check timed out after {SERVER_READY_HEALTH_TIMEOUT_MS}ms"
                ));
            }
        }

        if started_at.elapsed() >= deadline {
            let output = match process {
                Some(process) => process_output_summary(process).await,
                None => None,
            };
            let mut message = format!(
                "timed out waiting for OpenCode server at {server_url} after {}ms",
                deadline.as_millis()
            );
            if let Some(err) = last_health_error {
                message.push_str(&format!("; last health error: {err}"));
            }
            return Err(OpenCodeError::Spawn(with_process_output(
                message,
                output.as_deref(),
            )));
        }

        sleep(Duration::from_millis(SERVER_READY_POLL_MS)).await;
    }
}

fn server_ready_timeout() -> Duration {
    let timeout_env = std::env::var(SERVER_READY_TIMEOUT_ENV).ok();
    server_ready_timeout_from_env_value(timeout_env.as_deref())
}

fn server_ready_timeout_from_env_value(value: Option<&str>) -> Duration {
    value
        .and_then(parse_server_ready_timeout_ms)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS))
}

fn parse_server_ready_timeout_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|timeout_ms| *timeout_ms > 0)
}

fn turn_start_timeout() -> Duration {
    let timeout_env = std::env::var(TURN_START_TIMEOUT_ENV).ok();
    turn_start_timeout_from_env_value(timeout_env.as_deref())
}

fn turn_start_timeout_from_env_value(value: Option<&str>) -> Duration {
    value
        .and_then(parse_server_ready_timeout_ms)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS))
}

fn parse_host_port(server_url: &str) -> Result<(String, u16), OpenCodeError> {
    let stripped = server_url
        .strip_prefix("http://")
        .ok_or_else(|| OpenCodeError::InvalidServerUrl(server_url.to_string()))?;
    let host_port = stripped
        .split('/')
        .next()
        .ok_or_else(|| OpenCodeError::InvalidServerUrl(server_url.to_string()))?;
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| OpenCodeError::InvalidServerUrl(server_url.to_string()))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| OpenCodeError::InvalidServerUrl(server_url.to_string()))?;
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_url_from_env_wins() {
        let url = server_url_from_launch(
            &[
                "serve".to_string(),
                "--port".to_string(),
                "43100".to_string(),
            ],
            &[(
                "BREHON_OPENCODE_SERVER_URL".to_string(),
                "http://127.0.0.1:4999".to_string(),
            )],
        )
        .expect("server url");
        assert_eq!(url, "http://127.0.0.1:4999");
    }

    #[test]
    fn test_server_url_from_args() {
        let url = server_url_from_launch(
            &[
                "serve".to_string(),
                "--port".to_string(),
                "43100".to_string(),
            ],
            &[],
        )
        .expect("server url");
        assert_eq!(url, "http://127.0.0.1:43100");
    }

    #[test]
    fn test_server_ready_timeout_uses_env_value_or_default() {
        assert_eq!(
            server_ready_timeout_from_env_value(Some("45000")),
            Duration::from_millis(45_000)
        );
        assert_eq!(
            server_ready_timeout_from_env_value(Some(" 1200 ")),
            Duration::from_millis(1_200)
        );
        assert_eq!(
            server_ready_timeout_from_env_value(None),
            Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
        );
        assert_eq!(
            server_ready_timeout_from_env_value(Some("0")),
            Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
        );
        assert_eq!(
            server_ready_timeout_from_env_value(Some("invalid")),
            Duration::from_millis(DEFAULT_SERVER_READY_TIMEOUT_MS)
        );
    }

    #[test]
    fn test_turn_start_timeout_uses_env_value_or_default() {
        assert_eq!(
            turn_start_timeout_from_env_value(Some("60000")),
            Duration::from_millis(60_000)
        );
        assert_eq!(
            turn_start_timeout_from_env_value(Some(" 1500 ")),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            turn_start_timeout_from_env_value(None),
            Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
        );
        assert_eq!(
            turn_start_timeout_from_env_value(Some("0")),
            Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
        );
        assert_eq!(
            turn_start_timeout_from_env_value(Some("invalid")),
            Duration::from_millis(DEFAULT_TURN_START_TIMEOUT_MS)
        );
    }

    #[test]
    fn test_should_retry_turn_error_treats_opencode_500_as_retryable() {
        assert!(should_retry_turn_error(&OpenCodeError::Http(
            "session message fetch failed with 500 Internal Server Error: \
             {\"name\":\"UnknownError\",\"data\":{\"message\":\"Unexpected server error\"}}"
                .to_string()
        )));
        assert!(should_retry_turn_error(&OpenCodeError::Turn(
            "OpenCode prompt_async failed with 500 Internal Server Error: \
             {\"name\":\"UnknownError\"}"
                .to_string()
        )));
        assert!(!should_retry_turn_error(&OpenCodeError::Turn(
            "OpenCode prompt_async failed with 400 Bad Request: invalid model".to_string()
        )));
    }

    #[test]
    fn test_turn_poll_recovery_stops_after_budget() {
        let mut recovery = OpenCodeTurnPollRecovery::default();
        let err = OpenCodeError::Http(
            "session message fetch failed with 500 Internal Server Error".to_string(),
        );

        assert!(recovery.should_retry(&err));

        recovery.first_error_at = Some(
            tokio::time::Instant::now()
                .checked_sub(Duration::from_millis(TURN_POLL_RECOVERY_TIMEOUT_MS + 1))
                .expect("expired instant"),
        );
        assert!(!recovery.should_retry(&err));

        recovery.record_success();
        assert!(recovery.should_retry(&err));
    }

    #[test]
    fn test_turn_progress_does_not_complete_before_activity() {
        let mut progress = OpenCodeTurnProgress::default();

        assert!(!progress.observe(false, false));
        assert!(!progress.observe(false, false));
        assert!(!progress.observe(false, false));
        assert!(!progress.saw_activity());

        assert!(!progress.observe(true, false));
        assert!(!progress.saw_activity());
        assert!(!progress.observe(false, false));
        assert!(!progress.saw_activity());
        assert!(!progress.observe(false, true));
        assert!(progress.saw_activity());
        assert!(progress.observe(false, false));
    }

    #[test]
    fn test_turn_progress_settles_after_new_messages() {
        let mut progress = OpenCodeTurnProgress::default();

        assert!(!progress.observe(false, true));
        assert!(progress.saw_activity());
        assert!(progress.observe(false, false));
    }

    #[test]
    fn test_set_launch_loopback_port_updates_args_and_server_url_env() {
        let mut args = vec![
            "serve".to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "43100".to_string(),
        ];
        let mut env = vec![
            (
                "BREHON_OPENCODE_SERVER_URL".to_string(),
                "http://127.0.0.1:43100".to_string(),
            ),
            ("OTHER".to_string(), "value".to_string()),
        ];

        assert!(set_launch_loopback_port(&mut args, &mut env, 43210));
        assert_eq!(args[4], "43210");
        assert_eq!(
            server_url_from_launch(&args, &env).expect("server url"),
            "http://127.0.0.1:43210"
        );
        assert!(env
            .iter()
            .any(|(key, value)| { key == "OTHER" && value == "value" }));
    }

    #[test]
    fn test_should_retry_spawn_with_fresh_port_only_for_port_bind_failures() {
        assert!(should_retry_spawn_with_fresh_port(&OpenCodeError::Spawn(
            "OpenCode server process exited before readiness\n\
             OpenCode process output:\n\
             opencode log /tmp/opencode.log:\n\
             Error: Failed to start server. Is port 43100 in use?"
                .to_string()
        )));
        assert!(!should_retry_spawn_with_fresh_port(&OpenCodeError::Spawn(
            "timed out waiting for OpenCode server at http://127.0.0.1:43100 after 30000ms"
                .to_string()
        )));
        assert!(!should_retry_spawn_with_fresh_port(
            &OpenCodeError::SessionCreate("session create failed".to_string())
        ));
    }

    #[test]
    fn test_normalize_stream_event_maps_tool_use() {
        let session_id = SessionId::new("brehon-session");
        let event: OpenCodeStreamEvent = serde_json::from_str(
            r#"{
              "type": "tool_use",
              "sessionID": "ses_1",
              "part": {
                "type": "tool",
                "tool": "bash",
                "callID": "call_123",
                "state": {
                  "status": "completed",
                  "output": "/tmp\n",
                  "title": "Prints current working directory"
                }
              }
            }"#,
        )
        .expect("parse stream event");

        let normalized = normalize_stream_event(&session_id, &event);
        assert_eq!(normalized.len(), 2);
        assert!(matches!(
            &normalized[0],
            AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "bash"
        ));
        assert!(matches!(
            &normalized[1],
            AdapterEvent::ToolCallCompleted { status, .. } if status == "completed"
        ));
    }

    #[test]
    fn test_server_auth_from_env_extracts_credentials() {
        let auth = server_auth_from_env(&[
            ("OPENCODE_SERVER_USERNAME".to_string(), "brehon".to_string()),
            ("OPENCODE_SERVER_PASSWORD".to_string(), "secret".to_string()),
        ])
        .expect("auth");
        assert_eq!(auth.username, "brehon");
        assert_eq!(auth.password, "secret");
    }

    #[test]
    fn test_parse_opencode_model_selection_requires_provider_model() {
        let model =
            parse_opencode_model_selection(" deepseek/deepseek-v4-pro[1m] ").expect("model");
        assert_eq!(model.provider_id, "deepseek");
        assert_eq!(model.model_id, "deepseek-v4-pro[1m]");

        assert!(parse_opencode_model_selection("deepseek-v4-pro").is_err());
        assert!(parse_opencode_model_selection("deepseek/model/extra").is_err());
        assert!(parse_opencode_model_selection("/deepseek-v4-pro").is_err());
    }

    #[test]
    fn test_opencode_prompt_body_includes_model_selection() {
        let model = parse_opencode_model_selection("deepseek/deepseek-v4-pro[1m]").expect("model");
        let body = opencode_prompt_body("review this", Some(&model));

        assert_eq!(body["model"]["providerID"], "deepseek");
        assert_eq!(body["model"]["modelID"], "deepseek-v4-pro[1m]");
        assert_eq!(body["parts"][0]["type"], "text");
        assert_eq!(body["parts"][0]["text"], "review this");
    }

    #[test]
    fn test_sse_event_surfaces_session_error() {
        let event = serde_json::json!({
            "type": "session.error",
            "properties": {
                "sessionID": "ses_123",
                "error": {
                    "name": "APIError",
                    "statusCode": 400,
                    "message": "model rejected",
                    "responseBody": "{\"error\":\"bad model\"}"
                }
            }
        });

        let normalized = normalize_open_code_server_event("ses_123", &event).expect("event");

        assert!(normalized.failure);
        assert!(normalized.message.contains("OpenCode session error"));
        assert!(normalized.message.contains("status 400"));
        assert!(normalized.message.contains("model rejected"));
        assert!(normalized.message.contains("bad model"));
    }

    #[test]
    fn test_sse_event_name_only_error_keeps_raw_payload() {
        let event = serde_json::json!({
            "type": "session.error",
            "properties": {
                "sessionID": "ses_123",
                "error": {
                    "name": "APIError"
                }
            }
        });

        let normalized = normalize_open_code_server_event("ses_123", &event).expect("event");

        assert!(normalized.failure);
        assert!(normalized.message.contains("APIError"));
        assert!(normalized
            .message
            .contains("no detail in OpenCode error payload"));
        assert!(normalized
            .message
            .contains("raw_error={\"name\":\"APIError\"}"));
    }

    #[test]
    fn test_normalize_new_message_parts_surfaces_assistant_info_error() {
        let mut seen = OpenCodeSeenMessageParts::default();
        let message = serde_json::json!({
            "info": {
                "id": "msg-1",
                "role": "assistant",
                "error": {
                    "message": "The supported API model names are deepseek-v4-pro or deepseek-v4-flash"
                }
            },
            "parts": []
        });

        let (events, tokens, saw_activity) =
            normalize_new_message_parts(&message, 0, &mut seen, "");

        assert_eq!(tokens, 0);
        assert!(saw_activity);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. }
                if text.contains("OpenCode assistant error")
                    && text.contains("supported API model names")
        )));
    }

    #[test]
    fn test_normalize_message_response_reads_text_parts() {
        let session_id = SessionId::new("brehon-session");
        let response = serde_json::json!({
            "parts": [
                { "type": "text", "text": "hello" },
                { "type": "text", "text": "world" }
            ]
        });
        let normalized = normalize_message_response(&session_id, &response);
        assert_eq!(normalized.len(), 2);
        assert!(matches!(
            &normalized[0],
            AdapterEvent::Output { text, .. } if text == "hello"
        ));
        assert!(matches!(
            &normalized[1],
            AdapterEvent::Output { text, .. } if text == "world"
        ));
    }

    #[test]
    fn test_normalize_message_response_surfaces_retry_error() {
        let session_id = SessionId::new("brehon-session");
        let response = serde_json::json!({
            "role": "assistant",
            "parts": [
                {
                    "type": "retry",
                    "attempt": 2,
                    "error": {
                        "message": "The supported API model names are deepseek-v4-pro or deepseek-v4-flash"
                    }
                }
            ]
        });

        let events = normalize_message_response(&session_id, &response);

        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. }
                if text.contains("OpenCode retry attempt 2 failed")
                    && text.contains("supported API model names")
        )));
    }

    #[test]
    fn test_normalize_message_response_marks_error_step_failed() {
        let session_id = SessionId::new("brehon-session");
        let response = serde_json::json!({
            "role": "assistant",
            "parts": [
                { "type": "step-finish", "reason": "error" }
            ]
        });

        let events = normalize_message_response(&session_id, &response);

        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::OperationCompleted { operation, success: false, .. }
                if operation == "step"
        )));
    }

    #[test]
    fn test_message_tokens_used_reads_step_finish_total() {
        let response = serde_json::json!({
            "parts": [
                { "type": "text", "text": "OK" },
                {
                    "type": "step-finish",
                    "tokens": {
                        "total": 10058,
                        "input": 10039,
                        "output": 2,
                        "reasoning": 17
                    }
                }
            ]
        });

        assert_eq!(message_tokens_used(&response), 10058);
    }

    #[test]
    fn test_parse_session_busy_value_matches_array_entry() {
        let value: Value = serde_json::from_str(
            r#"[
              {"id":"ses_other","type":"idle"},
              {"id":"ses_123","type":"busy"}
            ]"#,
        )
        .expect("status payload");

        assert_eq!(
            parse_session_busy_value(&value, Some("ses_123")),
            Some(true)
        );
        assert_eq!(
            parse_session_busy_value(&value, Some("ses_other")),
            Some(false)
        );
    }

    #[test]
    fn test_parse_session_busy_value_matches_object_map_entry() {
        let value: Value = serde_json::from_str(
            r#"{
              "ses_other": {"type":"idle"},
              "ses_123": {"type":"busy"}
            }"#,
        )
        .expect("status payload");

        assert_eq!(
            parse_session_busy_value(&value, Some("ses_123")),
            Some(true)
        );
        assert_eq!(
            parse_session_busy_value(&value, Some("ses_other")),
            Some(false)
        );
    }

    #[test]
    fn test_looks_like_html_shell_detects_opencode_app_shell() {
        assert!(looks_like_html_shell(
            "<!doctype html><html><body></body></html>"
        ));
        assert!(looks_like_html_shell("<html><body></body></html>"));
        assert!(!looks_like_html_shell("{\"healthy\":true}"));
    }

    #[test]
    fn test_normalize_message_response_prefers_assistant_messages_from_history() {
        let session_id = SessionId::new("brehon-session");
        let value: Value = serde_json::from_str(
            r#"[
              {"role":"user","parts":[{"type":"text","text":"user prompt"}]},
              {"role":"assistant","parts":[
                {"type":"step-start"},
                {"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","output":"fn main() {}\n","title":"read"}},
                {"type":"text","text":"done"}
              ]}
            ]"#,
        )
        .expect("message history");

        let events = normalize_message_response(&session_id, &value);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::OperationStarted { operation, .. } if operation == "step"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "read"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "read"
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. } if text.contains("fn main()")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. } if text.contains("user prompt")
        )));
    }

    #[test]
    fn test_normalize_message_response_suppresses_tool_body_content() {
        let session_id = SessionId::new("brehon-session");
        let response = serde_json::json!({
            "role": "assistant",
            "parts": [
                {
                    "type": "tool",
                    "tool": "read",
                    "callID": "call_1",
                    "content": "tool body copied through content",
                    "state": {
                        "status": "completed",
                        "output": "fn leaked() {}\n",
                        "title": "read"
                    }
                },
                { "type": "text", "text": "done" }
            ]
        });

        let events = normalize_message_response(&session_id, &response);

        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "read"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "read"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. } if text == "done"
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::Output { text, .. }
                if text.contains("fn leaked") || text.contains("tool body copied")
        )));
    }

    #[test]
    fn test_normalize_new_message_parts_emits_only_text_delta_for_mutated_message() {
        let mut seen = OpenCodeSeenMessageParts::default();
        let first = serde_json::json!({
            "id": "msg-1",
            "role": "assistant",
            "parts": [
                {"id": "part-1", "type": "text", "text": "first"}
            ]
        });
        let second = serde_json::json!({
            "id": "msg-1",
            "role": "assistant",
            "parts": [
                {"id": "part-1", "type": "text", "text": "first second"}
            ]
        });

        let (events, tokens, saw_activity) = normalize_new_message_parts(&first, 0, &mut seen, "");
        assert_eq!(tokens, 0);
        assert!(saw_activity);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AdapterEvent::Output { text, .. } if text == "first"
        ));

        let (events, tokens, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
        assert_eq!(tokens, 0);
        assert!(saw_activity);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AdapterEvent::Output { text, .. } if text == " second"
        ));

        let (events, tokens, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
        assert_eq!(tokens, 0);
        assert!(!saw_activity);
        assert!(events.is_empty());
    }

    #[test]
    fn test_normalize_new_message_parts_does_not_count_prompt_echo_as_activity() {
        let mut seen = OpenCodeSeenMessageParts::default();
        let prompt = "Review context\n".repeat(8);
        let echoed_prompt = serde_json::json!({
            "id": "msg-1",
            "parts": [
                {"id": "part-1", "type": "text", "text": prompt.clone()}
            ]
        });

        let (events, tokens, saw_activity) =
            normalize_new_message_parts(&echoed_prompt, 0, &mut seen, &prompt);

        assert_eq!(tokens, 0);
        assert_eq!(events.len(), 1);
        assert!(!saw_activity);
    }

    #[test]
    fn test_normalize_new_message_parts_does_not_replay_tool_events_when_text_updates() {
        let mut seen = OpenCodeSeenMessageParts::default();
        let first = serde_json::json!({
            "id": "msg-1",
            "role": "assistant",
            "parts": [
                {"type": "step-start"},
                {
                    "type": "tool",
                    "tool": "bash",
                    "callID": "call-1",
                    "state": {
                        "status": "completed",
                        "output": "exit_code: 0\nok\n",
                        "title": "bash"
                    }
                },
                {"id": "part-2", "type": "text", "text": "done"}
            ]
        });
        let second = serde_json::json!({
            "id": "msg-1",
            "role": "assistant",
            "parts": [
                {"type": "step-start"},
                {
                    "type": "tool",
                    "tool": "bash",
                    "callID": "call-1",
                    "state": {
                        "status": "completed",
                        "output": "exit_code: 0\nok\n",
                        "title": "bash"
                    }
                },
                {"id": "part-2", "type": "text", "text": "done now"}
            ]
        });

        let (events, _, saw_activity) = normalize_new_message_parts(&first, 0, &mut seen, "");
        assert!(saw_activity);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallStarted { tool_name, .. } if tool_name == "bash"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ToolCallCompleted { tool_name, .. } if tool_name == "bash"
        )));

        let (events, _, saw_activity) = normalize_new_message_parts(&second, 0, &mut seen, "");
        assert!(saw_activity);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AdapterEvent::Output { text, .. } if text == " now"
        ));
    }
}
