use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, ToolCallStreaming,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use super::direct_tools::{CodingToolBridge, CompositeToolBridge, DirectToolBridge};
use super::stability_runtime::{clear_session_snapshot, persist_session_snapshot};
use super::updates::SessionEvent;

const REQUEST_TIMEOUT_SECS: u64 = 120;
const MAX_TOOL_ROUNDS: usize = 24;
const MAX_HISTORY_MESSAGES: usize = 60;

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
    event_tx: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    alive: AtomicBool,
    capabilities: AgentCapabilities,
    turn_lock: Mutex<()>,
}

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
        event_tx: Option<mpsc::Sender<SessionEvent>>,
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
            if let Err(err) = result {
                emit_event(
                    &inner,
                    SessionEvent::Output {
                        session_id: inner.session_id.clone(),
                        text: format!("OpenAI-compatible prompt failed: {err}\n"),
                    },
                )
                .await;
                emit_event(
                    &inner,
                    SessionEvent::OperationCompleted {
                        session_id: inner.session_id.clone(),
                        operation: "openai-compatible turn".to_string(),
                        success: false,
                    },
                )
                .await;
            }
            inner.active_prompts.lock().await.remove(&removal_key);
            persist_session_snapshot(
                inner.session_id.as_str(),
                brehon_types::StabilityCounters {
                    pending_requests: inner.active_prompts.lock().await.len(),
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

    pub async fn cancel_prompt(&self, prompt_id: &PromptId) -> Result<(), OpenAiCompatibleError> {
        let key = prompt_id.as_str().to_string();
        if let Some(handle) = self.inner.active_prompts.lock().await.remove(&key) {
            handle.abort();
            self.persist_runtime_stability();
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
) -> Result<(), OpenAiCompatibleError> {
    let _turn_guard = inner.turn_lock.lock().await;
    if !inner.alive.load(Ordering::SeqCst) {
        return Err(OpenAiCompatibleError::NotRunning);
    }

    emit_event(
        &inner,
        SessionEvent::OperationStarted {
            session_id: inner.session_id.clone(),
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
            emit_event(
                &inner,
                SessionEvent::Output {
                    session_id: inner.session_id.clone(),
                    text: format!("{text}\n"),
                },
            )
            .await;
        }

        if assistant.tool_calls.is_empty() {
            emit_event(
                &inner,
                SessionEvent::OperationCompleted {
                    session_id: inner.session_id.clone(),
                    operation: "openai-compatible turn".to_string(),
                    success: true,
                },
            )
            .await;
            return Ok(());
        }

        for tool_call in assistant.tool_calls {
            emit_event(
                &inner,
                SessionEvent::ToolCallStarted {
                    session_id: inner.session_id.clone(),
                    tool_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    details: Some(json!({ "input": tool_call.arguments.clone() })),
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
                SessionEvent::ToolCallCompleted {
                    session_id: inner.session_id.clone(),
                    tool_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    status: if success {
                        "success".to_string()
                    } else {
                        "failed".to_string()
                    },
                    details: Some(json!({ "output": tool_result.clone() })),
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
    json!({
        "role": "system",
        "content": format!(
            "You are an Brehon {} agent operating inside the worktree '{}'. \
    Use the provided function tools for repo work and Brehon coordination. \
    Use {}* tools for session/task/review/factory state. \
    Use read_file, search_text, list_files, write_file, replace_in_file, and bash for coding work. \
    Stay inside the current worktree. Do not invent tool results or claim commands ran if they did not.",
            spec.role,
            spec.worktree_path,
            tool_prefix,
        )
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

async fn emit_event(inner: &OpenAiCompatibleSessionInner, event: SessionEvent) {
    // Hoist the clone so the `event_tx` MutexGuard is not held across
    // `tx.send(event).await` — otherwise a full event channel serializes all
    // event emission through this lock.
    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
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
            if let Some(SessionEvent::Output { text, .. }) = rx.recv().await {
                assert!(text.contains("hello from api"));
                saw_output = true;
                break;
            }
        }
        assert!(saw_output, "expected an output event");
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
                Some(SessionEvent::ToolCallStarted { tool_name, .. }) => {
                    if tool_name == "read_file" {
                        saw_tool_start = true;
                    }
                }
                Some(SessionEvent::ToolCallCompleted {
                    tool_name, status, ..
                }) => {
                    if tool_name == "read_file" && status == "success" {
                        saw_tool_complete = true;
                    }
                }
                Some(SessionEvent::Output { text, .. }) => {
                    if text.contains("finished") {
                        saw_output = true;
                    }
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
