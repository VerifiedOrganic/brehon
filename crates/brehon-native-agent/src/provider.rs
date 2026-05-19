use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::agent_runtime::message::{AgentMessage, AgentRole, AssistantTurn, ToolUseRequest};
use crate::runtime::CancellationToken;

const REQUEST_CONNECT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    #[error("missing base_url for OpenAI-compatible provider")]
    MissingBaseUrl,
    #[error("missing API key in environment variable {0}")]
    MissingApiKey(String),
    #[error("invalid HTTP header {0}")]
    InvalidHeader(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("request failed: {0}")]
    Request(String),
    #[error("cancelled")]
    Cancelled,
}

impl ProviderError {
    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Self::Http(_) => true,
            Self::Request(message) => {
                if let Some(status) = request_status_code(message) {
                    return matches!(status, 408 | 409 | 425 | 429 | 500..=599);
                }
                message.starts_with("invalid streaming chat completion chunk:")
                    || message.starts_with("streaming chat completion error:")
                    || message.starts_with("streamed tool call ")
                    || message.starts_with("invalid tool arguments JSON:")
            }
            Self::Cancelled
            | Self::MissingBaseUrl
            | Self::MissingApiKey(_)
            | Self::InvalidHeader(_) => false,
        }
    }

    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::Http(_) => "transport",
            Self::Request(message) => {
                if let Some(status) = request_status_code(message) {
                    if matches!(status, 408 | 409 | 425 | 429 | 500..=599) {
                        "retryable_http_status"
                    } else {
                        "non_retryable_http_status"
                    }
                } else if self.is_retryable() {
                    "provider_output"
                } else {
                    "request"
                }
            }
            Self::Cancelled => "cancelled",
            Self::MissingBaseUrl | Self::MissingApiKey(_) | Self::InvalidHeader(_) => {
                "configuration"
            }
        }
    }
}

pub(crate) struct ProviderRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) reasoning_effort: Option<&'a str>,
    pub(crate) reasoning_effort_param: Option<&'a str>,
    pub(crate) messages: &'a [AgentMessage],
    pub(crate) tools: &'a [Value],
    pub(crate) extra_body: Option<&'a Value>,
    pub(crate) assistant_message_passthrough_fields: &'a [String],
    pub(crate) activity: Option<watch::Sender<Instant>>,
}

#[async_trait]
pub(crate) trait ChatProvider: Send + Sync {
    async fn complete(
        &self,
        request: ProviderRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn, ProviderError>;
}

pub struct FakeProvider;

#[async_trait]
impl ChatProvider for FakeProvider {
    async fn complete(
        &self,
        request: ProviderRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<AssistantTurn, ProviderError> {
        if let Some(tool_result) = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role() == AgentRole::Tool)
            .map(AgentMessage::text_content)
        {
            let content = format!("native-agent fake tool result: {tool_result}");
            return Ok(AssistantTurn {
                content: Some(content.clone()),
                tool_calls: Vec::new(),
                history_message: AgentMessage::assistant(Some(content), Vec::new()),
                tokens_used: Some(0),
                stop_reason: Some("stop".to_string()),
            });
        }

        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role() == AgentRole::User)
            .map(AgentMessage::text_content)
            .unwrap_or_default();
        if prompt.contains("fake-report-role") {
            let system = request
                .messages
                .iter()
                .find(|message| message.role() == AgentRole::System)
                .map(AgentMessage::text_content)
                .unwrap_or_default();
            let role = if system.contains("Brehon reviewer startup") {
                "reviewer"
            } else if system.contains("Brehon worker startup") {
                "worker"
            } else if system.contains("Factory supervisor startup") {
                "supervisor"
            } else {
                "unknown"
            };
            let content = format!("native-agent fake role: {role}");
            return Ok(AssistantTurn {
                content: Some(content.clone()),
                tool_calls: Vec::new(),
                history_message: AgentMessage::assistant(Some(content), Vec::new()),
                tokens_used: Some(0),
                stop_reason: Some("stop".to_string()),
            });
        }
        if prompt.contains("fake-write-file") {
            let arguments = json!({
                "path": "native-agent-permission.txt",
                "content": "approved by native agent\n",
            });
            let tool_call = ToolUseRequest::new("fake-write-file-1", "write_file", arguments);
            return Ok(AssistantTurn {
                content: None,
                tool_calls: vec![tool_call.clone()],
                history_message: AgentMessage::assistant(None, vec![tool_call]),
                tokens_used: Some(0),
                stop_reason: Some("tool_calls".to_string()),
            });
        }

        let content = format!("native-agent fake response: {prompt}");
        Ok(AssistantTurn {
            content: Some(content.clone()),
            tool_calls: Vec::new(),
            history_message: AgentMessage::assistant(Some(content), Vec::new()),
            tokens_used: Some(0),
            stop_reason: Some("stop".to_string()),
        })
    }
}

pub struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    base_url: String,
    headers: HeaderMap,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        base_url: Option<String>,
        api_key_env: Option<String>,
        headers: &[(String, String)],
    ) -> Result<Self, ProviderError> {
        let base_url = base_url
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .ok_or(ProviderError::MissingBaseUrl)?;
        let headers = build_headers(api_key_env.as_deref(), headers)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(REQUEST_CONNECT_TIMEOUT_SECS))
            .build()
            .map_err(|err| ProviderError::Http(err.to_string()))?;
        Ok(Self {
            client,
            base_url,
            headers,
        })
    }
}

#[async_trait]
impl ChatProvider for OpenAiCompatibleProvider {
    async fn complete(
        &self,
        request: ProviderRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn, ProviderError> {
        let body = build_chat_completion_body(&request)?;

        let assistant_message_passthrough_fields = request.assistant_message_passthrough_fields;
        let activity = request.activity;
        let request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .headers(self.headers.clone())
            .json(&body)
            .send();

        let response = tokio::select! {
            response = request => response.map_err(|err| ProviderError::Http(err.to_string()))?,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|err| format!("<failed to read response body: {err}>"));
            return Err(ProviderError::Request(format!("status {status}: {body}")));
        }
        mark_activity(activity.as_ref());

        parse_streaming_chat_completion_response(
            response,
            assistant_message_passthrough_fields,
            cancel,
            activity.as_ref(),
        )
        .await
    }
}

fn build_chat_completion_body(request: &ProviderRequest<'_>) -> Result<Value, ProviderError> {
    let messages = request
        .messages
        .iter()
        .map(AgentMessage::to_openai_json)
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.to_vec());
    }
    if let Some(extra_body) = request.extra_body {
        let extra_body = extra_body.as_object().ok_or_else(|| {
            ProviderError::Request("--extra-body-json must decode to a JSON object".into())
        })?;
        for (key, value) in extra_body {
            body[key] = value.clone();
        }
    }
    if let (Some(reasoning_effort), Some(path)) = (
        request
            .reasoning_effort
            .filter(|value| !value.trim().is_empty()),
        request
            .reasoning_effort_param
            .filter(|value| !value.trim().is_empty()),
    ) {
        set_body_path(&mut body, path, json!(reasoning_effort))?;
    }
    if body.get("stream_options").is_none() {
        body["stream_options"] = json!({"include_usage": true});
    }
    body["stream"] = Value::Bool(true);
    Ok(body)
}

fn build_headers(
    api_key_env: Option<&str>,
    extra_headers: &[(String, String)],
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    if let Some(api_key_env) = api_key_env.filter(|value| !value.trim().is_empty()) {
        let api_key = std::env::var(api_key_env)
            .map_err(|_| ProviderError::MissingApiKey(api_key_env.to_string()))?;
        let value = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| ProviderError::InvalidHeader(AUTHORIZATION.to_string()))?;
        headers.insert(AUTHORIZATION, value);
    }

    for (name, value) in extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ProviderError::InvalidHeader(name.clone()))?;
        let header_value =
            HeaderValue::from_str(value).map_err(|_| ProviderError::InvalidHeader(name.clone()))?;
        headers.insert(header_name, header_value);
    }

    Ok(headers)
}

#[cfg(test)]
fn parse_chat_completion_message(
    value: &Value,
    assistant_message_passthrough_fields: &[String],
) -> Result<AssistantTurn, ProviderError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::Request("missing choices in chat completion".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::Request("missing assistant message".into()))?;

    let content = parse_message_content(message.get("content"));
    let mut parsed_tool_calls = Vec::new();

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
                .ok_or_else(|| ProviderError::Request("tool call missing function name".into()))?
                .to_string();
            let arguments = parse_tool_arguments(function.get("arguments"))?;

            parsed_tool_calls.push(ToolUseRequest::new(id.clone(), name.clone(), arguments));
        }
    } else if let Some(function_call) = message.get("function_call").and_then(Value::as_object) {
        let name = function_call
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::Request("function call missing name".into()))?
            .to_string();
        let arguments = parse_tool_arguments(function_call.get("arguments"))?;
        parsed_tool_calls.push(ToolUseRequest::new("function-call-0", name, arguments));
    }

    let assistant_extension_fields =
        parse_assistant_extension_fields(message, assistant_message_passthrough_fields);
    let history_message = AgentMessage::assistant_with_extension_fields(
        content.clone(),
        parsed_tool_calls.clone(),
        assistant_extension_fields,
    );

    let tokens_used = value.get("usage").and_then(parse_usage_tokens);
    let stop_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(AssistantTurn {
        content,
        tool_calls: parsed_tool_calls,
        history_message,
        tokens_used,
        stop_reason,
    })
}

#[derive(Default)]
struct StreamingAssistantTurn {
    content: String,
    tool_calls: BTreeMap<usize, StreamingToolCall>,
    assistant_extension_fields: Map<String, Value>,
    tokens_used: Option<u64>,
    stop_reason: Option<String>,
}

#[derive(Default)]
struct StreamingToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

async fn parse_streaming_chat_completion_response(
    response: reqwest::Response,
    assistant_message_passthrough_fields: &[String],
    cancel: &CancellationToken,
    activity: Option<&watch::Sender<Instant>>,
) -> Result<AssistantTurn, ProviderError> {
    let mut accumulator = StreamingAssistantTurn::default();
    let mut buffer = String::new();
    let mut chunks = response.bytes_stream();

    loop {
        let chunk = tokio::select! {
            chunk = chunks.next() => chunk,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let bytes = chunk.map_err(|err| ProviderError::Http(err.to_string()))?;
        mark_activity(activity);
        let text = String::from_utf8_lossy(&bytes);
        buffer.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));

        while let Some(event) = take_sse_event(&mut buffer) {
            if let Some(data) = parse_sse_data(&event) {
                if data.trim() == "[DONE]" {
                    return finish_streaming_turn(accumulator);
                }
                apply_streaming_chat_completion_chunk(
                    &mut accumulator,
                    &data,
                    assistant_message_passthrough_fields,
                )?;
            }
        }
    }

    if let Some(data) = parse_sse_data(&buffer) {
        if data.trim() != "[DONE]" {
            apply_streaming_chat_completion_chunk(
                &mut accumulator,
                &data,
                assistant_message_passthrough_fields,
            )?;
        }
    }

    finish_streaming_turn(accumulator)
}

fn take_sse_event(buffer: &mut String) -> Option<String> {
    let index = buffer.find("\n\n")?;
    let event = buffer[..index].to_string();
    buffer.drain(..index + 2);
    Some(event)
}

fn parse_sse_data(event: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in event.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

fn apply_streaming_chat_completion_chunk(
    accumulator: &mut StreamingAssistantTurn,
    data: &str,
    assistant_message_passthrough_fields: &[String],
) -> Result<(), ProviderError> {
    let value: Value = serde_json::from_str(data).map_err(|err| {
        ProviderError::Request(format!(
            "invalid streaming chat completion chunk: {err}: {}",
            data.chars().take(240).collect::<String>()
        ))
    })?;

    if let Some(error) = value.get("error").filter(|value| !value.is_null()) {
        return Err(ProviderError::Request(format!(
            "streaming chat completion error: {}",
            compact_json(error)
        )));
    }

    if let Some(tokens) = value.get("usage").and_then(parse_usage_tokens) {
        accumulator.tokens_used = Some(tokens);
    }

    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(());
    };

    for choice in choices {
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            accumulator.stop_reason = Some(reason.to_string());
        }

        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(content) = parse_message_content(delta.get("content")) {
            accumulator.content.push_str(&content);
        }

        for field in assistant_message_passthrough_fields {
            let field = field.trim();
            if field.is_empty() || matches!(field, "role" | "content" | "tool_calls") {
                continue;
            }
            if let Some(value) = delta.get(field).filter(|value| !value.is_null()) {
                append_assistant_extension_field(
                    &mut accumulator.assistant_extension_fields,
                    field,
                    value,
                );
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (fallback_index, call) in tool_calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(fallback_index);
                append_streaming_tool_call(&mut accumulator.tool_calls, index, call);
            }
        }

        if let Some(function_call) = delta.get("function_call").filter(|value| value.is_object()) {
            append_streaming_tool_call(&mut accumulator.tool_calls, 0, function_call);
        }
    }

    Ok(())
}

fn append_streaming_tool_call(
    tool_calls: &mut BTreeMap<usize, StreamingToolCall>,
    index: usize,
    call: &Value,
) {
    let entry = tool_calls.entry(index).or_default();
    if let Some(id) = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        entry.id = Some(id.to_string());
    }

    let function = call.get("function").unwrap_or(call);
    if let Some(name) = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        entry.name.push_str(name);
    }
    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
        entry.arguments.push_str(arguments);
    }
}

fn append_assistant_extension_field(fields: &mut Map<String, Value>, field: &str, value: &Value) {
    if let Some(text) = value.as_str() {
        if text.is_empty() {
            return;
        }
        match fields.get_mut(field) {
            Some(Value::String(existing)) => existing.push_str(text),
            _ => {
                fields.insert(field.to_string(), Value::String(text.to_string()));
            }
        }
        return;
    }

    fields.insert(field.to_string(), value.clone());
}

fn finish_streaming_turn(
    accumulator: StreamingAssistantTurn,
) -> Result<AssistantTurn, ProviderError> {
    let content = if accumulator.content.is_empty() {
        None
    } else {
        Some(accumulator.content)
    };
    let mut parsed_tool_calls = Vec::new();
    for (index, call) in accumulator.tool_calls {
        let name = call.name.trim();
        if name.is_empty() {
            return Err(ProviderError::Request(format!(
                "streamed tool call {index} missing function name"
            )));
        }
        let id = call.id.unwrap_or_else(|| format!("tool-call-{index}"));
        let arguments = if call.arguments.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            parse_tool_arguments(Some(&Value::String(call.arguments)))?
        };
        parsed_tool_calls.push(ToolUseRequest::new(id, name.to_string(), arguments));
    }

    let history_message = AgentMessage::assistant_with_extension_fields(
        content.clone(),
        parsed_tool_calls.clone(),
        accumulator.assistant_extension_fields,
    );

    Ok(AssistantTurn {
        content,
        tool_calls: parsed_tool_calls,
        history_message,
        tokens_used: accumulator.tokens_used,
        stop_reason: accumulator.stop_reason,
    })
}

fn mark_activity(activity: Option<&watch::Sender<Instant>>) {
    if let Some(activity) = activity {
        let _ = activity.send(Instant::now());
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn parse_usage_tokens(usage: &Value) -> Option<u64> {
    usage_token_field(
        usage,
        &[
            "total_tokens",
            "totalTokens",
            "tokens_used",
            "tokensUsed",
            "total",
        ],
    )
    .or_else(|| {
        let input = usage_token_field(
            usage,
            &[
                "prompt_tokens",
                "promptTokens",
                "input_tokens",
                "inputTokens",
            ],
        );
        let output = usage_token_field(
            usage,
            &[
                "completion_tokens",
                "completionTokens",
                "output_tokens",
                "outputTokens",
            ],
        );
        match (input, output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            (Some(input), None) => Some(input),
            (None, Some(output)) => Some(output),
            (None, None) => None,
        }
    })
}

fn usage_token_field(usage: &Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| usage.get(*name).and_then(Value::as_u64))
}

#[cfg(test)]
fn parse_assistant_extension_fields(message: &Value, field_names: &[String]) -> Map<String, Value> {
    field_names
        .iter()
        .filter_map(|field| {
            let field = field.trim();
            if field.is_empty() || matches!(field, "role" | "content" | "tool_calls") {
                return None;
            }
            message
                .get(field)
                .filter(|value| !value.is_null())
                .map(|value| (field.to_string(), value.clone()))
        })
        .collect()
}

fn set_body_path(body: &mut Value, path: &str, value: Value) -> Result<(), ProviderError> {
    let segments = path
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Err(ProviderError::Request(
            "reasoning effort request path is empty".into(),
        ));
    }

    let mut current = body;
    for segment in &segments[..segments.len() - 1] {
        if !current.is_object() {
            return Err(ProviderError::Request(format!(
                "cannot set nested request field '{path}' through non-object segment '{segment}'"
            )));
        }
        if current.get(*segment).is_none() {
            current[*segment] = json!({});
        }
        current = current.get_mut(*segment).ok_or_else(|| {
            ProviderError::Request(format!("failed to create request field '{segment}'"))
        })?;
    }
    let leaf = segments
        .last()
        .expect("segments is non-empty after validation");
    if !current.is_object() {
        return Err(ProviderError::Request(format!(
            "cannot set request field '{path}' on non-object parent"
        )));
    }
    current[*leaf] = value;
    Ok(())
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

fn parse_tool_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    let Some(value) = value else {
        return Ok(Value::Object(Default::default()));
    };
    if let Some(raw) = value.as_str() {
        return serde_json::from_str(raw)
            .map_err(|err| ProviderError::Request(format!("invalid tool arguments JSON: {err}")));
    }
    Ok(value.clone())
}

pub(crate) fn provider_error_to_string(err: ProviderError) -> String {
    match err {
        ProviderError::Cancelled => "cancelled".to_string(),
        other => other.to_string(),
    }
}

fn request_status_code(message: &str) -> Option<u16> {
    let rest = message.strip_prefix("status ")?;
    let code = rest.split_whitespace().next()?;
    code.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_omits_empty_tool_calls_from_history() {
        let turn = parse_chat_completion_message(
            &json!({
                "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}],
                "usage": {"total_tokens": 12}
            }),
            &[],
        )
        .unwrap();

        assert_eq!(turn.content.as_deref(), Some("ok"));
        assert!(turn.history_message.tool_calls().is_empty());
        assert_eq!(turn.tokens_used, Some(12));
    }

    #[test]
    fn parse_message_sums_usage_when_total_tokens_absent() {
        let turn = parse_chat_completion_message(
            &json!({
                "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 2,
                    "input_tokens_details": {"cached_tokens": 4},
                    "completion_tokens_details": {"reasoning_tokens": 1}
                }
            }),
            &[],
        )
        .unwrap();

        assert_eq!(turn.tokens_used, Some(12));
    }

    #[test]
    fn parse_usage_tokens_accepts_camel_case_input_output_fields() {
        assert_eq!(
            parse_usage_tokens(&json!({
                "inputTokens": 7,
                "outputTokens": 3
            })),
            Some(10)
        );
    }

    #[test]
    fn chat_completion_body_requests_stream_usage_by_default() {
        let messages = vec![AgentMessage::user("hello")];
        let tools = Vec::new();
        let fields = Vec::new();
        let request = ProviderRequest {
            model: "test-model",
            reasoning_effort: None,
            reasoning_effort_param: None,
            messages: &messages,
            tools: &tools,
            extra_body: None,
            assistant_message_passthrough_fields: &fields,
            activity: None,
        };

        let body = build_chat_completion_body(&request).unwrap();

        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn chat_completion_body_preserves_explicit_stream_options() {
        let messages = vec![AgentMessage::user("hello")];
        let tools = Vec::new();
        let fields = Vec::new();
        let extra_body = json!({
            "stream_options": {
                "include_usage": false,
                "provider_flag": "custom"
            }
        });
        let request = ProviderRequest {
            model: "test-model",
            reasoning_effort: None,
            reasoning_effort_param: None,
            messages: &messages,
            tools: &tools,
            extra_body: Some(&extra_body),
            assistant_message_passthrough_fields: &fields,
            activity: None,
        };

        let body = build_chat_completion_body(&request).unwrap();

        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], false);
        assert_eq!(body["stream_options"]["provider_flag"], "custom");
    }

    #[test]
    fn parse_message_preserves_reasoning_content_for_tool_call_subturns() {
        let fields = vec!["reasoning_content".to_string()];
        let turn = parse_chat_completion_message(
            &json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "content": null,
                        "reasoning_content": "I need to inspect the task first.",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "mcp_brehon_task",
                                "arguments": "{\"action\":\"ready\"}"
                            }
                        }]
                    }
                }]
            }),
            &fields,
        )
        .unwrap();

        assert_eq!(
            turn.history_message.to_openai_json()["reasoning_content"],
            "I need to inspect the task first."
        );
        assert_eq!(turn.tool_calls[0].id, "call-1");
    }

    #[test]
    fn streaming_chunks_preserve_reasoning_and_tool_call_deltas() {
        let fields = vec!["reasoning_content".to_string()];
        let mut accumulator = StreamingAssistantTurn::default();
        apply_streaming_chat_completion_chunk(
            &mut accumulator,
            r#"{"choices":[{"delta":{"reasoning_content":"I need "}}]}"#,
            &fields,
        )
        .unwrap();
        apply_streaming_chat_completion_chunk(
            &mut accumulator,
            r#"{"choices":[{"delta":{"reasoning_content":"state.","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"mcp_brehon_task","arguments":"{\"action\""}}]}}]}"#,
            &fields,
        )
        .unwrap();
        apply_streaming_chat_completion_chunk(
            &mut accumulator,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ready\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"total_tokens":42}}"#,
            &fields,
        )
        .unwrap();

        let turn = finish_streaming_turn(accumulator).unwrap();
        assert_eq!(
            turn.history_message.to_openai_json()["reasoning_content"],
            "I need state."
        );
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call-1");
        assert_eq!(turn.tool_calls[0].name, "mcp_brehon_task");
        assert_eq!(turn.tool_calls[0].arguments["action"], "ready");
        assert_eq!(turn.tokens_used, Some(42));
        assert_eq!(turn.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn streaming_chunks_assemble_plain_content() {
        let mut accumulator = StreamingAssistantTurn::default();
        apply_streaming_chat_completion_chunk(
            &mut accumulator,
            r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
            &[],
        )
        .unwrap();
        apply_streaming_chat_completion_chunk(
            &mut accumulator,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            &[],
        )
        .unwrap();

        let turn = finish_streaming_turn(accumulator).unwrap();
        assert_eq!(turn.content.as_deref(), Some("hello"));
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parse_message_does_not_preserve_unconfigured_assistant_extensions() {
        let turn = parse_chat_completion_message(
            &json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "content": null,
                        "reasoning_content": "provider-specific metadata",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "mcp_brehon_task",
                                "arguments": "{}"
                            }
                        }]
                    }
                }]
            }),
            &[],
        )
        .unwrap();

        assert!(turn.history_message.to_openai_json()["reasoning_content"].is_null());
    }

    #[test]
    fn set_body_path_creates_nested_objects() {
        let mut body = json!({"model": "test"});
        set_body_path(&mut body, "thinking.reasoning_effort", json!("high")).unwrap();
        assert_eq!(body["thinking"]["reasoning_effort"], "high");
    }

    #[test]
    fn parse_message_accepts_legacy_function_call_shape() {
        let turn = parse_chat_completion_message(
            &json!({
                "choices": [{
                    "finish_reason": "function_call",
                    "message": {
                        "content": null,
                        "function_call": {
                            "name": "mcp_brehon_verification",
                            "arguments": "{\"action\":\"review_status\",\"task_id\":\"T-1\"}"
                        }
                    }
                }]
            }),
            &[],
        )
        .unwrap();

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "function-call-0");
        assert_eq!(turn.tool_calls[0].name, "mcp_brehon_verification");
        assert_eq!(turn.tool_calls[0].arguments["action"], "review_status");
    }

    #[test]
    fn provider_error_retry_classification_matches_unattended_recovery_policy() {
        assert!(ProviderError::Http("connection reset".into()).is_retryable());
        assert!(
            ProviderError::Request("status 429 Too Many Requests: slow down".into()).is_retryable()
        );
        assert!(
            ProviderError::Request("status 500 Internal Server Error: try later".into())
                .is_retryable()
        );
        assert!(ProviderError::Request(
            "streaming chat completion error: {\"message\":\"upstream reset\"}".into()
        )
        .is_retryable());

        assert!(!ProviderError::Request("status 400 Bad Request: bad body".into()).is_retryable());
        assert!(!ProviderError::MissingApiKey("API_KEY".into()).is_retryable());
    }
}
