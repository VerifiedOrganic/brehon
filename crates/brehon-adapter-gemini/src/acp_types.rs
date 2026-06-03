//! ACP-specific protocol types.
//!
//! These types map to the Agent Client Protocol (ACP) JSON-RPC message
//! structures used by OpenCode and similar stdio ACP agents.

use serde::{Deserialize, Serialize};

use brehon_types::{AgentCapabilities, ToolCallStreaming};

use crate::protocol::{JsonRpcNotification, JsonRpcRequest};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    #[serde(rename = "clientCapabilities")]
    pub client_capabilities: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitializeResult {
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: Option<u32>,
    #[serde(rename = "agentCapabilities", alias = "capabilities")]
    pub capabilities: AcpCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default, rename = "embeddedContext")]
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AcpCapabilities {
    #[serde(default)]
    pub content_block_types: Vec<String>,
    #[serde(default)]
    pub session_config_options: Vec<String>,
    #[serde(default)]
    pub permission_support: bool,
    #[serde(default)]
    pub terminal_support: bool,
    #[serde(default)]
    pub tool_call_streaming: String,
    #[serde(default, rename = "promptCapabilities")]
    pub prompt_capabilities: Option<PromptCapabilities>,
}

impl From<AcpCapabilities> for AgentCapabilities {
    fn from(cap: AcpCapabilities) -> Self {
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
            vec!["mode".to_string(), "model".to_string()]
        } else {
            cap.session_config_options
        };

        Self {
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewSessionParams {
    pub cwd: String,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewSessionResult {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tokensUsed")]
    pub tokens_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stopReason")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalAttachParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalAttachResult {
    #[serde(rename = "terminalId")]
    pub terminal_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalInputParams {
    #[serde(rename = "terminalId")]
    pub terminal_id: String,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetModeParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "modeId")]
    pub mode_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetModelParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShutdownParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub fn create_initialize_request(_cwd: &str, _metadata: Option<SessionMetadata>) -> JsonRpcRequest {
    JsonRpcRequest::new(
        "initialize",
        Some(
            serde_json::to_value(InitializeParams {
                protocol_version: 1,
                client_capabilities: serde_json::json!({}),
            })
            .unwrap(),
        ),
    )
}

pub fn create_new_session_request(
    cwd: &str,
    mcp_servers: Vec<serde_json::Value>,
) -> JsonRpcRequest {
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

pub fn create_prompt_request(prompt_id: &str, session_id: &str, content: &str) -> JsonRpcRequest {
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

pub fn create_cancel_notification(session_id: &str) -> JsonRpcNotification {
    JsonRpcNotification::new(
        "session/cancel",
        Some(
            serde_json::to_value(CancelParams {
                session_id: session_id.to_string(),
            })
            .unwrap(),
        ),
    )
}

pub fn create_terminal_attach_request(session_id: &str, cols: u16, rows: u16) -> JsonRpcRequest {
    JsonRpcRequest::new(
        "terminal_attach",
        Some(
            serde_json::to_value(TerminalAttachParams {
                session_id: session_id.to_string(),
                cols: Some(cols),
                rows: Some(rows),
            })
            .unwrap(),
        ),
    )
}

pub fn create_terminal_input_request(terminal_id: &str, input: &[u8]) -> JsonRpcRequest {
    JsonRpcRequest::new(
        "terminal_input",
        Some(
            serde_json::to_value(TerminalInputParams {
                terminal_id: terminal_id.to_string(),
                input: base64_encode(input),
            })
            .unwrap(),
        ),
    )
}

pub fn create_set_config_request(session_id: &str, option: &str, value: &str) -> JsonRpcRequest {
    match option {
        "mode" => JsonRpcRequest::new(
            "session/set_mode",
            Some(
                serde_json::to_value(SetModeParams {
                    session_id: session_id.to_string(),
                    mode_id: value.to_string(),
                })
                .unwrap(),
            ),
        ),
        "model" => JsonRpcRequest::new(
            "session/set_model",
            Some(
                serde_json::to_value(SetModelParams {
                    session_id: session_id.to_string(),
                    model_id: value.to_string(),
                })
                .unwrap(),
            ),
        ),
        _ => JsonRpcRequest::new(
            "session/set_mode",
            Some(
                serde_json::to_value(SetModeParams {
                    session_id: session_id.to_string(),
                    mode_id: value.to_string(),
                })
                .unwrap(),
            ),
        ),
    }
}

pub fn create_shutdown_request(reason: Option<&str>) -> JsonRpcRequest {
    JsonRpcRequest::new(
        "shutdown",
        Some(
            serde_json::to_value(ShutdownParams {
                reason: reason.map(|r| r.to_string()),
            })
            .unwrap(),
        ),
    )
}

pub fn parse_new_session_result(
    response: &super::protocol::JsonRpcResponse,
) -> Result<NewSessionResult, String> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| format!("Failed to parse session/new result: {}", e)),
        None => Err("No result in session/new response".to_string()),
    }
}

pub fn parse_prompt_result(
    response: &super::protocol::JsonRpcResponse,
) -> Result<PromptResult, String> {
    match &response.result {
        Some(result) => {
            let mut prompt_result: PromptResult = serde_json::from_value(result.clone())
                .map_err(|e| format!("Failed to parse prompt result: {}", e))?;
            if prompt_result.tokens_used.is_none() {
                prompt_result.tokens_used = prompt_result_tokens_from_meta(result);
            }
            Ok(prompt_result)
        }
        None => Ok(PromptResult::default()),
    }
}

fn prompt_result_tokens_from_meta(result: &serde_json::Value) -> Option<u64> {
    let meta = result.get("_meta")?;
    let quota = meta.get("quota").unwrap_or(meta);
    let token_count = quota
        .get("token_count")
        .or_else(|| quota.get("tokenCount"))
        .unwrap_or(quota);

    token_field(
        token_count,
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
            token_count,
            &[
                "input",
                "inputTokens",
                "input_tokens",
                "promptTokens",
                "prompt_tokens",
                "output",
                "outputTokens",
                "output_tokens",
                "completionTokens",
                "completion_tokens",
                "reasoning",
                "reasoningTokens",
                "reasoning_tokens",
                "reasoningOutputTokens",
                "reasoning_output_tokens",
                "cacheCreationInputTokens",
                "cache_creation_input_tokens",
                "cacheReadInputTokens",
                "cache_read_input_tokens",
                "cachedInputTokens",
                "cached_input_tokens",
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

fn base64_encode(input: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_request() {
        let request = create_initialize_request("/tmp/work", None);
        assert_eq!(request.method, "initialize");
        let params = request.params.unwrap();
        assert_eq!(params["protocolVersion"], 1);
    }

    #[test]
    fn test_new_session_request() {
        let request = create_new_session_request(
            "/tmp/work",
            vec![serde_json::json!({
                "name": "brehon",
                "command": "brehon",
                "args": ["serve"],
                "env": [],
            })],
        );
        assert_eq!(request.method, "session/new");
        let params = request.params.unwrap();
        assert_eq!(params["cwd"], "/tmp/work");
        assert_eq!(params["mcpServers"][0]["name"], "brehon");
    }

    #[test]
    fn test_prompt_request() {
        let request = create_prompt_request("p-123", "s-1", "Write hello world");
        assert_eq!(request.method, "session/prompt");
        let params = request.params.unwrap();
        assert_eq!(params["sessionId"], "s-1");
        assert_eq!(params["prompt"][0]["text"], "Write hello world");
    }

    #[test]
    fn test_parse_prompt_result_extracts_gemini_meta_tokens() {
        let response = super::super::protocol::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: "p-123".to_string(),
            result: Some(serde_json::json!({
                "stopReason": "end_turn",
                "_meta": {
                    "quota": {
                        "token_count": {
                            "input_tokens": 9527,
                            "output_tokens": 1
                        }
                    }
                }
            })),
            error: None,
        };

        let result = parse_prompt_result(&response).expect("parse prompt result");

        assert_eq!(result.tokens_used, Some(9528));
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn test_capabilities_conversion() {
        let acp_caps = AcpCapabilities {
            content_block_types: vec![],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: String::new(),
            prompt_capabilities: Some(PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: true,
            }),
        };
        let caps: AgentCapabilities = acp_caps.into();
        assert_eq!(caps.content_block_types, vec!["text", "image", "resource"]);
        assert!(!caps.permission_support);
        assert!(!caps.terminal_support);
        assert_eq!(caps.tool_call_streaming, ToolCallStreaming::Basic);
    }
}
