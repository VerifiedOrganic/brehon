use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_adapter_sdk::direct_tools::{CodingToolBridge, CompositeToolBridge, DirectToolBridge};
use brehon_adapter_sdk::AdapterEvent;
use brehon_types::{
    build_native_agent_system_prompt, AgentCapabilities, HealthStatus, PromptHandle, PromptId,
    PromptTurn, SessionId, SessionInfo, SessionSpec, ToolCallStreaming,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::stability::{clear_session_snapshot, persist_session_snapshot};

const REQUEST_TIMEOUT_SECS: u64 = 120;
const MAX_TOOL_ROUNDS: usize = 24;
const MAX_HISTORY_MESSAGES: usize = 60;
const MAX_CACHED_PROMPT_RESULTS: usize = 64;

#[derive(Debug, Error)]
pub enum OpenAiCompatibleError {
    #[error("missing base_url for OpenAI-compatible session")]
    MissingBaseUrl,
    #[error("invalid header '{0}'")]
    InvalidHeader(String),
    #[error("http client error: {0}")]
    Http(String),
    #[error("session not running")]
    NotRunning,
    #[error("timeout waiting for response to {0}")]
    Timeout(String),
    #[error("request failed: {0}")]
    Request(String),
    #[error("assistant requested too many consecutive tool rounds")]
    ToolLoopExceeded,
}

struct OpenAiCompatibleSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    client: Client,
    base_url: String,
    headers: HeaderMap,
    model: Mutex<Option<String>>,
    messages: Mutex<Vec<Value>>,
    tool_bridge: Arc<dyn DirectToolBridge>,
    active_prompts: Mutex<HashMap<String, JoinHandle<()>>>,
    event_tx: Mutex<Option<mpsc::Sender<AdapterEvent>>>,
    alive: AtomicBool,
    capabilities: AgentCapabilities,
    turn_lock: Mutex<()>,
    prompt_completers: Mutex<HashMap<String, oneshot::Sender<brehon_adapter_sdk::PromptResult>>>,
    prompt_results: Mutex<HashMap<String, brehon_adapter_sdk::PromptResult>>,
    prompt_result_keys: Mutex<VecDeque<String>>,
}

#[derive(Clone)]
pub struct OpenAiCompatibleSession {
    inner: Arc<OpenAiCompatibleSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
struct AssistantTurn {
    content: Option<String>,
    tool_calls: Vec<AssistantToolCall>,
    history_message: Value,
}

#[derive(Debug)]
struct AssistantToolCall {
    id: String,
    name: String,
    arguments: Value,
}

impl OpenAiCompatibleSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        spec: SessionSpec,
        base_url: Option<String>,
        api_key_env: Option<String>,
        extra_headers: Vec<(String, String)>,
        model: Option<String>,
        tool_prefix: Option<String>,
        tool_bridge: Option<Arc<dyn DirectToolBridge>>,
        event_tx: Option<mpsc::Sender<AdapterEvent>>,
    ) -> Result<Self, OpenAiCompatibleError> {
        let base_url = base_url
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .ok_or(OpenAiCompatibleError::MissingBaseUrl)?;

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|err| OpenAiCompatibleError::Http(err.to_string()))?;

        let headers = build_headers(api_key_env.as_deref(), &extra_headers)?;
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let tool_prefix = tool_prefix.unwrap_or_else(|| "mcp_brehon_".to_string());
        let tool_bridge = CompositeToolBridge::new(match tool_bridge {
            Some(tool_bridge) => vec![
                tool_bridge,
                CodingToolBridge::new(PathBuf::from(&spec.worktree_path)),
            ],
            None => vec![CodingToolBridge::new(PathBuf::from(&spec.worktree_path))],
        });

        let session = Self {
            inner: Arc::new(OpenAiCompatibleSessionInner {
                session_id,
                tool_bridge,
                messages: Mutex::new(vec![system_message(&spec, &tool_prefix)]),
                spec,
                client,
                base_url,
                headers,
                model: Mutex::new(model),
                active_prompts: Mutex::new(HashMap::new()),
                event_tx: Mutex::new(event_tx),
                alive: AtomicBool::new(true),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec!["model".to_string()],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: ToolCallStreaming::Basic,
                },
                turn_lock: Mutex::new(()),
                prompt_completers: Mutex::new(HashMap::new()),
                prompt_results: Mutex::new(HashMap::new()),
                prompt_result_keys: Mutex::new(VecDeque::new()),
            }),
            created_at,
        };
        persist_session_snapshot(
            session.inner.session_id.as_str(),
            brehon_types::StabilityCounters::default(),
        );
        Ok(session)
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
            ..Default::default()
        }
    }

    fn persist_runtime_stability(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            persist_session_snapshot(
                inner.session_id.as_str(),
                brehon_types::StabilityCounters {
                    pending_requests: inner.active_prompts.lock().await.len(),
                    ..Default::default()
                },
            );
        });
    }

    pub async fn send_prompt(
        &self,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, OpenAiCompatibleError> {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return Err(OpenAiCompatibleError::NotRunning);
        }

        let prompt_key = prompt.prompt_id.as_str().to_string();
        let prompt_id = prompt.prompt_id.clone();
        let created_at = prompt.sent_at;
        let inner = Arc::clone(&self.inner);
        let removal_key = prompt_key.clone();

        let task = tokio::spawn(async move {
            let result = run_prompt(inner.clone(), prompt).await;
            let success = result.is_ok();
            let response_text = result.as_ref().ok().cloned().unwrap_or_default();
            if let Err(err) = &result {
                emit_event(
                    &inner,
                    AdapterEvent::Output {
                        text: format!("OpenAI-compatible prompt failed: {err}\n"),
                    },
                )
                .await;
                emit_event(
                    &inner,
                    AdapterEvent::OperationCompleted {
                        operation: "openai-compatible turn".to_string(),
                        success: false,
                    },
                )
                .await;
            }
            let prompt_result = {
                let mut prompt_result = brehon_adapter_sdk::PromptResult::default();
                prompt_result.stop_reason = if success {
                    Some("stop".to_string())
                } else {
                    Some("error".to_string())
                };
                prompt_result.response = if response_text.trim().is_empty() {
                    None
                } else {
                    Some(response_text)
                };
                prompt_result
            };

            // Cache before notifying/removing active state so waiters can always
            // observe a monotonic completion state.
            cache_prompt_result(&inner, removal_key.clone(), prompt_result.clone()).await;
            if let Some(completer) = inner.prompt_completers.lock().await.remove(&removal_key) {
                let _ = completer.send(prompt_result);
            }

            let pending_requests = {
                let mut active = inner.active_prompts.lock().await;
                active.remove(&removal_key);
                active.len()
            };
            persist_session_snapshot(
                inner.session_id.as_str(),
                brehon_types::StabilityCounters {
                    pending_requests,
                    ..Default::default()
                },
            );
        });

        self.inner
            .active_prompts
            .lock()
            .await
            .insert(prompt_key, task);
        self.persist_runtime_stability();

        Ok(PromptHandle {
            prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at,
        })
    }

    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<brehon_adapter_sdk::PromptResult, OpenAiCompatibleError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_id_str = prompt_id.as_str().to_string();

        match tokio::time::timeout(deadline, async {
            loop {
                // Fast path: result already available
                if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                    return Ok(result);
                }

                // If the prompt is no longer active and not in results, it was cancelled.
                if !self
                    .inner
                    .active_prompts
                    .lock()
                    .await
                    .contains_key(&prompt_id_str)
                {
                    // Completion may have been cached after the fast-path pop above
                    // but before this active check; prefer returning it over NotRunning.
                    if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                        return Ok(result);
                    }
                    return Err(OpenAiCompatibleError::NotRunning);
                }

                // Register a oneshot to be woken when the prompt completes
                let (tx, rx) = oneshot::channel();
                {
                    let mut completers = self.inner.prompt_completers.lock().await;
                    if completers.contains_key(&prompt_id_str) {
                        return Err(OpenAiCompatibleError::Request(format!(
                            "already waiting for response to {prompt_id_str}"
                        )));
                    }
                    completers.insert(prompt_id_str.clone(), tx);
                }

                // Double-check in case the prompt completed between the two locks above
                if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                    self.inner
                        .prompt_completers
                        .lock()
                        .await
                        .remove(&prompt_id_str);
                    return Ok(result);
                }

                // Re-check active state after registration to close a cancel race:
                // cancel_prompt can remove active state between the earlier
                // active_prompts check and completer insertion.
                if !self
                    .inner
                    .active_prompts
                    .lock()
                    .await
                    .contains_key(&prompt_id_str)
                {
                    self.inner
                        .prompt_completers
                        .lock()
                        .await
                        .remove(&prompt_id_str);
                    if let Some(result) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                        return Ok(result);
                    }
                    return Err(OpenAiCompatibleError::NotRunning);
                }

                match rx.await {
                    Ok(result) => {
                        // Drain the cached completion result on the normal waiter path
                        // so prompt_results does not accumulate entries for consumed prompts.
                        if let Some(cached) = pop_prompt_result(&self.inner, &prompt_id_str).await {
                            return Ok(cached);
                        }
                        return Ok(result);
                    }
                    Err(_) => {
                        // Sender dropped without sending — prompt may have been
                        // cancelled or a race occurred. Loop around to recheck state.
                        self.inner
                            .prompt_completers
                            .lock()
                            .await
                            .remove(&prompt_id_str);
                        continue;
                    }
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                self.inner
                    .prompt_completers
                    .lock()
                    .await
                    .remove(&prompt_id_str);
                Err(OpenAiCompatibleError::Timeout(prompt_id_str))
            }
        }
    }

    pub async fn cancel_prompt(&self, prompt_id: &PromptId) -> Result<(), OpenAiCompatibleError> {
        let key = prompt_id.as_str().to_string();
        let handle = self.inner.active_prompts.lock().await.remove(&key);
        let removed_completer = self
            .inner
            .prompt_completers
            .lock()
            .await
            .remove(&key)
            .is_some();
        let removed_result = pop_prompt_result(&self.inner, &key).await.is_some();
        if let Some(handle) = handle {
            handle.abort();
            self.persist_runtime_stability();
            return Ok(());
        }
        if removed_completer || removed_result {
            // No active task handle, but cleanup did remove orphaned wait/result
            // state, so cancellation had an observable effect.
            return Ok(());
        }
        Err(OpenAiCompatibleError::NotRunning)
    }

    pub async fn kill(&self) -> Result<(), OpenAiCompatibleError> {
        self.inner.alive.store(false, Ordering::SeqCst);
        let mut active = self.inner.active_prompts.lock().await;
        for (_, handle) in active.drain() {
            handle.abort();
        }
        self.inner.prompt_completers.lock().await.clear();
        clear_prompt_results(&self.inner).await;
        clear_session_snapshot(self.inner.session_id.as_str());
        Ok(())
    }

    pub async fn health_check(&self) -> Result<HealthStatus, OpenAiCompatibleError> {
        if !self.inner.alive.load(Ordering::SeqCst) {
            return Ok(HealthStatus::Unhealthy);
        }

        let response = self
            .inner
            .client
            .get(format!("{}/models", self.inner.base_url))
            .headers(self.inner.headers.clone())
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => Ok(HealthStatus::Healthy),
            Ok(_) => Ok(HealthStatus::Unhealthy),
            Err(err) => Err(OpenAiCompatibleError::Http(err.to_string())),
        }
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

    pub async fn set_config(&self, option: &str, value: &str) -> Result<(), OpenAiCompatibleError> {
        match option.trim_end_matches('!') {
            "model" => {
                *self.inner.model.lock().await = Some(value.to_string());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Set or replace the event channel sender for this session.
    pub async fn set_event_tx(&self, tx: Option<mpsc::Sender<AdapterEvent>>) {
        *self.inner.event_tx.lock().await = tx;
    }
}

fn build_headers(
    api_key_env: Option<&str>,
    extra_headers: &[(String, String)],
) -> Result<HeaderMap, OpenAiCompatibleError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    if let Some(api_key_env) = api_key_env.filter(|value| !value.trim().is_empty()) {
        if let Ok(api_key) = std::env::var(api_key_env) {
            if !api_key.trim().is_empty() {
                let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .map_err(|_| OpenAiCompatibleError::InvalidHeader(AUTHORIZATION.to_string()))?;
                headers.insert(AUTHORIZATION, value);
            }
        }
    }

    for (name, value) in extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| OpenAiCompatibleError::InvalidHeader(name.clone()))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| OpenAiCompatibleError::InvalidHeader(name.clone()))?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

async fn run_prompt(
    inner: Arc<OpenAiCompatibleSessionInner>,
    prompt: PromptTurn,
) -> Result<String, OpenAiCompatibleError> {
    let _turn_guard = inner.turn_lock.lock().await;
    if !inner.alive.load(Ordering::SeqCst) {
        return Err(OpenAiCompatibleError::NotRunning);
    }

    let mut response_text = String::new();

    emit_event(
        &inner,
        AdapterEvent::OperationStarted {
            operation: "openai-compatible turn".to_string(),
        },
    )
    .await;

    append_message(
        &inner,
        json!({
            "role": "user",
            "content": prompt.content,
        }),
    )
    .await;

    for _ in 0..MAX_TOOL_ROUNDS {
        let model = inner
            .model
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "gpt-5.4-mini".to_string());
        let messages = inner.messages.lock().await.clone();
        let body = json!({
            "model": model,
            "messages": messages,
            "tools": inner.tool_bridge.tool_definitions(),
        });

        let response = inner
            .client
            .post(format!("{}/chat/completions", inner.base_url))
            .headers(inner.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|err| OpenAiCompatibleError::Http(err.to_string()))?;

        if !response.status().is_success() {
            return Err(OpenAiCompatibleError::Request(format!(
                "status {}",
                response.status()
            )));
        }

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|err| OpenAiCompatibleError::Http(err.to_string()))?;
        let assistant = parse_chat_completion_message(&response_json)?;
        append_message(&inner, assistant.history_message).await;

        if let Some(text) = assistant
            .content
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let text = format!("{text}\n");
            response_text.push_str(&text);
            emit_event(&inner, AdapterEvent::Output { text }).await;
        }

        if assistant.tool_calls.is_empty() {
            emit_event(
                &inner,
                AdapterEvent::OperationCompleted {
                    operation: "openai-compatible turn".to_string(),
                    success: true,
                },
            )
            .await;
            return Ok(response_text);
        }

        for tool_call in assistant.tool_calls {
            emit_event(
                &inner,
                AdapterEvent::ToolCallStarted {
                    tool_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    details: Some(serde_json::json!({ "input": tool_call.arguments.clone() })),
                },
            )
            .await;

            let (tool_result, success) = match inner
                .tool_bridge
                .invoke(&tool_call.name, tool_call.arguments)
                .await
            {
                Ok(result) => (result, true),
                Err(err) => (format!("ERROR: {err}"), false),
            };

            emit_event(
                &inner,
                AdapterEvent::ToolCallCompleted {
                    tool_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    status: if success {
                        "success".to_string()
                    } else {
                        "failed".to_string()
                    },
                    details: Some(serde_json::json!({ "output": tool_result.clone() })),
                },
            )
            .await;

            append_message(
                &inner,
                json!({
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "content": tool_result,
                }),
            )
            .await;
        }
    }

    Err(OpenAiCompatibleError::ToolLoopExceeded)
}

async fn append_message(inner: &OpenAiCompatibleSessionInner, message: Value) {
    let mut messages = inner.messages.lock().await;
    messages.push(message);
    trim_messages(&mut messages);
}

fn trim_messages(messages: &mut Vec<Value>) {
    if messages.len() <= MAX_HISTORY_MESSAGES {
        return;
    }

    let system_message = messages
        .first()
        .filter(|value| value.get("role").and_then(Value::as_str) == Some("system"))
        .cloned();
    let keep_tail = MAX_HISTORY_MESSAGES.saturating_sub(system_message.is_some() as usize);
    let mut trimmed = Vec::with_capacity(MAX_HISTORY_MESSAGES);
    if let Some(system_message) = system_message {
        trimmed.push(system_message);
    }
    let tail_start = messages.len().saturating_sub(keep_tail);
    trimmed.extend(messages.iter().skip(tail_start).cloned());
    *messages = trimmed;
}

fn system_message(spec: &SessionSpec, tool_prefix: &str) -> Value {
    // Delegate to the shared role-protocol builder so chat-completions clients
    // see the same worktree/task-lifecycle rules CLI agents receive as their
    // startup user message. Without this, models behind an OpenAI-compatible
    // endpoint only get a paragraph of vague guidance and routinely violate
    // worktree containment (cd outside, checkout main, etc.).
    //
    // `supervisor_name` and `project_policy` aren't on `SessionSpec` today;
    // plumb them through `OpenAiCompatibleConfig` if per-pool overrides land.
    let agent_name = spec.agent_id.as_str();
    let agent_type = "openai-compatible";
    let supervisor_name = "supervisor";
    let content = build_native_agent_system_prompt(
        &spec.role,
        agent_name,
        agent_type,
        &spec.worktree_path,
        tool_prefix,
        supervisor_name,
        None,
    );
    json!({
        "role": "system",
        "content": content,
    })
}

fn parse_chat_completion_message(value: &Value) -> Result<AssistantTurn, OpenAiCompatibleError> {
    let message = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| {
            OpenAiCompatibleError::Request("missing assistant message in chat completion".into())
        })?;

    let content = parse_message_content(message.get("content"));
    let mut parsed_tool_calls = Vec::new();
    let mut raw_tool_calls = Vec::new();

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("tool-call")
                .to_string();
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    OpenAiCompatibleError::Request("tool call missing function name".into())
                })?
                .to_string();
            let raw_arguments = function
                .get("arguments")
                .map(raw_arguments_string)
                .unwrap_or_else(|| "{}".to_string());
            let arguments = parse_tool_arguments(function.get("arguments"))?;

            parsed_tool_calls.push(AssistantToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments,
            });
            raw_tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": raw_arguments,
                }
            }));
        }
    }

    Ok(AssistantTurn {
        content: content.clone(),
        tool_calls: parsed_tool_calls,
        history_message: json!({
            "role": "assistant",
            "content": content.unwrap_or_default(),
            "tool_calls": raw_tool_calls,
        }),
    })
}

fn parse_message_content(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }

    content.as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        part.get("text")
                            .and_then(|text| text.get("value"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("")
    })
}

fn raw_arguments_string(value: &Value) -> String {
    if let Some(raw) = value.as_str() {
        raw.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
    }
}

fn parse_tool_arguments(value: Option<&Value>) -> Result<Value, OpenAiCompatibleError> {
    let Some(value) = value else {
        return Ok(Value::Object(Default::default()));
    };

    if let Some(raw) = value.as_str() {
        return serde_json::from_str(raw).map_err(|err| {
            OpenAiCompatibleError::Request(format!("invalid tool arguments JSON: {err}"))
        });
    }

    Ok(value.clone())
}

async fn emit_event(inner: &OpenAiCompatibleSessionInner, event: AdapterEvent) {
    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
}

async fn cache_prompt_result(
    inner: &Arc<OpenAiCompatibleSessionInner>,
    prompt_id: String,
    result: brehon_adapter_sdk::PromptResult,
) {
    let mut prompt_result_keys = inner.prompt_result_keys.lock().await;
    let mut prompt_results = inner.prompt_results.lock().await;

    if let Some(existing_position) = prompt_result_keys
        .iter()
        .position(|stored| stored == &prompt_id)
    {
        prompt_result_keys.remove(existing_position);
    }
    prompt_result_keys.push_back(prompt_id.clone());
    prompt_results.insert(prompt_id, result);

    while prompt_result_keys.len() > MAX_CACHED_PROMPT_RESULTS {
        if let Some(evicted_prompt_id) = prompt_result_keys.pop_front() {
            prompt_results.remove(&evicted_prompt_id);
        }
    }
}

async fn pop_prompt_result(
    inner: &Arc<OpenAiCompatibleSessionInner>,
    prompt_id: &str,
) -> Option<brehon_adapter_sdk::PromptResult> {
    let mut prompt_result_keys = inner.prompt_result_keys.lock().await;
    let mut prompt_results = inner.prompt_results.lock().await;
    if let Some(existing_position) = prompt_result_keys
        .iter()
        .position(|stored| stored == prompt_id)
    {
        prompt_result_keys.remove(existing_position);
    }
    prompt_results.remove(prompt_id)
}

async fn clear_prompt_results(inner: &Arc<OpenAiCompatibleSessionInner>) {
    inner.prompt_result_keys.lock().await.clear();
    inner.prompt_results.lock().await.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parse_message_content_handles_string_content() {
        let response = json!("hello");
        assert_eq!(
            parse_message_content(Some(&response)).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn parse_message_content_handles_content_parts() {
        let response = json!([
            { "text": "hello " },
            { "text": "world" }
        ]);
        assert_eq!(
            parse_message_content(Some(&response)).as_deref(),
            Some("hello world")
        );
    }

    #[tokio::test]
    async fn session_send_prompt_emits_output_events() {
        let (base_url, _server, _requests) = spawn_test_server(vec![
            r#"{"choices":[{"message":{"content":"hello from api"}}]}"#.to_string(),
        ])
        .await;
        let (tx, mut rx) = mpsc::channel(8);

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            Some(tx),
        )
        .await
        .unwrap();

        session
            .send_prompt(PromptTurn {
                prompt_id: PromptId::new("prompt-1"),
                content: "say hello".to_string(),
                kind: brehon_types::MessageKind::System,
                sent_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let mut saw_output = false;
        for _ in 0..3 {
            if let Some(AdapterEvent::Output { text, .. }) = rx.recv().await {
                assert!(text.contains("hello from api"));
                saw_output = true;
                break;
            }
        }
        assert!(saw_output, "expected an output event");
    }

    #[tokio::test]
    async fn session_wait_for_response_returns_result() {
        let (base_url, _server, _requests) = spawn_test_server(vec![
            r#"{"choices":[{"message":{"content":"hello from api"}}]}"#.to_string(),
        ])
        .await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-wait");
        session
            .send_prompt(PromptTurn {
                prompt_id: prompt_id.clone(),
                content: "say hello".to_string(),
                kind: brehon_types::MessageKind::System,
                sent_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let result = session.wait_for_response(&prompt_id, 5000).await.unwrap();
        assert_eq!(result.stop_reason, Some("stop".to_string()));
        assert_eq!(result.response, Some("hello from api\n".to_string()));
        assert!(session
            .inner
            .prompt_results
            .lock()
            .await
            .get("prompt-wait")
            .is_none());
    }

    #[tokio::test]
    async fn session_wait_for_response_after_completion() {
        let (base_url, _server, _requests) = spawn_test_server(vec![
            r#"{"choices":[{"message":{"content":"already done"}}]}"#.to_string(),
        ])
        .await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-late-wait");
        session
            .send_prompt(PromptTurn {
                prompt_id: prompt_id.clone(),
                content: "say hello".to_string(),
                kind: brehon_types::MessageKind::System,
                sent_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // Allow the spawned prompt task to finish before calling wait_for_response.
        // This exercises the race where the result is stored in prompt_results
        // before the waiter registers its oneshot.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let result = session.wait_for_response(&prompt_id, 5000).await.unwrap();
        assert_eq!(result.stop_reason, Some("stop".to_string()));
        assert_eq!(result.response, Some("already done\n".to_string()));
        assert!(session
            .inner
            .prompt_results
            .lock()
            .await
            .get("prompt-late-wait")
            .is_none());
    }

    #[tokio::test]
    async fn session_wait_for_response_unknown_prompt() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let result = session
            .wait_for_response(&PromptId::new("never-sent"), 100)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn session_wait_for_response_returns_cached_result_when_prompt_becomes_inactive() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-inactive-with-result");
        let prompt_key = prompt_id.as_str().to_string();
        session.inner.active_prompts.lock().await.insert(
            prompt_key.clone(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }),
        );

        // Hold active_prompts so the waiter can complete its first pop and then
        // block before the active check. We then remove active state and cache
        // a completion result to exercise the pre-registration inactive branch.
        let mut active_guard = session.inner.active_prompts.lock().await;
        let first_pop_guard = session.inner.prompt_result_keys.lock().await;
        let waiter_session = session.clone();
        let waiter_prompt = prompt_id.clone();
        let waiter = tokio::spawn(async move {
            waiter_session
                .wait_for_response(&waiter_prompt, 1_000)
                .await
        });
        tokio::task::yield_now().await;
        drop(first_pop_guard);

        let handle = active_guard
            .remove(&prompt_key)
            .expect("prompt should still be active");
        handle.abort();
        let mut cached_result = brehon_adapter_sdk::PromptResult::default();
        cached_result.stop_reason = Some("stop".to_string());
        cached_result.response = Some("late completion".to_string());
        cache_prompt_result(&session.inner, prompt_key.clone(), cached_result.clone()).await;
        drop(active_guard);

        let result = tokio::time::timeout(Duration::from_millis(300), waiter)
            .await
            .expect("waiter should finish quickly")
            .expect("waiter task should not panic")
            .expect("waiter should return cached completion result");
        assert_eq!(result, cached_result);
        assert!(!session
            .inner
            .prompt_results
            .lock()
            .await
            .contains_key(&prompt_key));
    }

    #[tokio::test]
    async fn session_wait_for_response_times_out_with_timeout_error() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-timeout");
        session.inner.active_prompts.lock().await.insert(
            prompt_id.as_str().to_string(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }),
        );

        let result = session.wait_for_response(&prompt_id, 20).await;
        assert!(matches!(
            result,
            Err(OpenAiCompatibleError::Timeout(ref value)) if value == "prompt-timeout"
        ));

        let handle = {
            session
                .inner
                .active_prompts
                .lock()
                .await
                .remove(prompt_id.as_str())
        };
        if let Some(handle) = handle {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn session_wait_for_response_rejects_concurrent_waiters() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-concurrent");
        session.inner.active_prompts.lock().await.insert(
            prompt_id.as_str().to_string(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }),
        );

        let (tx, _rx) = oneshot::channel();
        session
            .inner
            .prompt_completers
            .lock()
            .await
            .insert(prompt_id.as_str().to_string(), tx);

        let result = session.wait_for_response(&prompt_id, 100).await;
        assert!(matches!(result, Err(OpenAiCompatibleError::Request(_))));
    }

    #[tokio::test]
    async fn session_wait_for_response_detects_cancel_after_registration() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-cancel-race");
        let prompt_key = prompt_id.as_str().to_string();
        session.inner.active_prompts.lock().await.insert(
            prompt_key.clone(),
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }),
        );

        // Block the first pop_prompt_result call so we can queue the waiter
        // deterministically at the start of wait_for_response.
        let first_pop_guard = session.inner.prompt_result_keys.lock().await;
        let waiter_session = session.clone();
        let waiter_prompt = prompt_id.clone();
        let waiter = tokio::spawn(async move {
            waiter_session
                .wait_for_response(&waiter_prompt, 1_000)
                .await
        });
        tokio::task::yield_now().await;
        drop(first_pop_guard);

        // Re-acquire prompt_result_keys before the second pop_prompt_result
        // call to freeze the waiter after completer registration.
        let second_pop_guard = session.inner.prompt_result_keys.lock().await;

        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if session
                    .inner
                    .prompt_completers
                    .lock()
                    .await
                    .contains_key(&prompt_key)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter should register completer");

        if let Some(handle) = session
            .inner
            .active_prompts
            .lock()
            .await
            .remove(&prompt_key)
        {
            handle.abort();
        }
        drop(second_pop_guard);

        let result = tokio::time::timeout(Duration::from_millis(300), waiter)
            .await
            .expect("waiter should finish after cancellation")
            .expect("waiter task should not panic");
        assert!(matches!(result, Err(OpenAiCompatibleError::NotRunning)));
        assert!(!session
            .inner
            .prompt_completers
            .lock()
            .await
            .contains_key(&prompt_key));
    }

    #[tokio::test]
    async fn session_cancel_prompt_returns_ok_when_cleanup_occurs_without_active_handle() {
        let (base_url, _server, _requests) = spawn_test_server(vec![]).await;

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                ".".to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            None,
        )
        .await
        .unwrap();

        let prompt_id = PromptId::new("prompt-cleanup-only");
        let prompt_key = prompt_id.as_str().to_string();
        let (tx, _rx) = oneshot::channel();
        session
            .inner
            .prompt_completers
            .lock()
            .await
            .insert(prompt_key.clone(), tx);
        session
            .inner
            .prompt_result_keys
            .lock()
            .await
            .push_back(prompt_key.clone());
        let mut cached_result = brehon_adapter_sdk::PromptResult::default();
        cached_result.stop_reason = Some("stop".to_string());
        cached_result.response = Some("cached".to_string());
        session
            .inner
            .prompt_results
            .lock()
            .await
            .insert(prompt_key.clone(), cached_result);

        let result = session.cancel_prompt(&prompt_id).await;
        assert!(result.is_ok());
        assert!(!session
            .inner
            .prompt_completers
            .lock()
            .await
            .contains_key(&prompt_key));
        assert!(!session
            .inner
            .prompt_result_keys
            .lock()
            .await
            .iter()
            .any(|stored| stored == &prompt_key));
        assert!(!session
            .inner
            .prompt_results
            .lock()
            .await
            .contains_key(&prompt_key));
    }

    #[tokio::test]
    async fn session_executes_read_file_tool_calls() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("notes.txt");
        std::fs::write(&file_path, "line one\nline two\n").unwrap();

        let (base_url, _server, requests) = spawn_test_server(vec![
            r#"{"choices":[{"message":{"content":"","tool_calls":[{"id":"call-read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"notes.txt\"}"}}]}}]}"#.to_string(),
            r#"{"choices":[{"message":{"content":"finished"}}]}"#.to_string(),
        ])
        .await;
        let (tx, mut rx) = mpsc::channel(16);

        let session = OpenAiCompatibleSession::spawn(
            SessionSpec::new(
                brehon_types::AgentId::new("openai-worker"),
                "worker".to_string(),
                temp.path().to_string_lossy().to_string(),
            ),
            Some(base_url),
            None,
            vec![],
            Some("gpt-test".to_string()),
            Some("mcp_brehon_".to_string()),
            None,
            Some(tx),
        )
        .await
        .unwrap();

        session
            .send_prompt(PromptTurn {
                prompt_id: PromptId::new("prompt-2"),
                content: "inspect notes".to_string(),
                kind: brehon_types::MessageKind::System,
                sent_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let mut saw_tool_start = false;
        let mut saw_tool_complete = false;
        let mut saw_output = false;
        for _ in 0..8 {
            match rx.recv().await {
                Some(AdapterEvent::ToolCallStarted { tool_name, .. })
                    if tool_name == "read_file" =>
                {
                    saw_tool_start = true;
                }
                Some(AdapterEvent::ToolCallCompleted {
                    tool_name, status, ..
                }) if tool_name == "read_file" && status == "success" => {
                    saw_tool_complete = true;
                }
                Some(AdapterEvent::Output { text, .. }) if text.contains("finished") => {
                    saw_output = true;
                }
                _ => {}
            }
            if saw_tool_start && saw_tool_complete && saw_output {
                break;
            }
        }

        assert!(saw_tool_start);
        assert!(saw_tool_complete);
        assert!(saw_output);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("line one"));
        assert!(requests[1].contains("\"tool_call_id\":\"call-read\""));
    }

    async fn spawn_test_server(
        responses: Vec<String>,
    ) -> (String, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);
        let server_responses = Arc::clone(&responses);

        let handle = tokio::spawn(async move {
            loop {
                let response_body = {
                    let mut queued = server_responses.lock().await;
                    queued.pop_front()
                };
                let Some(body) = response_body else {
                    break;
                };

                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 32 * 1024];
                let read = socket.read(&mut buf).await.unwrap();
                server_requests
                    .lock()
                    .await
                    .push(String::from_utf8_lossy(&buf[..read]).to_string());

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });

        (format!("http://{addr}/v1"), handle, requests)
    }
}
