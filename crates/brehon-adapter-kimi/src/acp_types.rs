//! ACP-specific protocol types.
//!
//! These types map to the Agent Client Protocol (ACP) JSON-RPC message
//! structures used by Kimi and similar stdio ACP agents.

use serde::{Deserialize, Serialize};

use brehon_types::{AgentCapabilities, ToolCallStreaming};

use brehon_adapter_sdk::protocol::{JsonRpcNotification, JsonRpcRequest};

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
    #[serde(default, rename = "toolCallStreaming")]
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

pub fn create_initialize_request(_cwd: &str, metadata: Option<SessionMetadata>) -> JsonRpcRequest {
    let mut params = serde_json::to_value(InitializeParams {
        protocol_version: 1,
        client_capabilities: serde_json::json!({}),
    })
    .unwrap();

    if let Some(metadata) = metadata {
        if let serde_json::Value::Object(ref mut map) = params {
            map.insert(
                "metadata".to_string(),
                serde_json::to_value(metadata).unwrap(),
            );
        }
    }

    JsonRpcRequest::new("initialize", Some(params))
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

pub fn parse_new_session_result(
    response: &brehon_adapter_sdk::protocol::JsonRpcResponse,
) -> Result<NewSessionResult, String> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| format!("Failed to parse session/new result: {}", e)),
        None => Err("No result in session/new response".to_string()),
    }
}

pub fn parse_prompt_result(
    response: &brehon_adapter_sdk::protocol::JsonRpcResponse,
) -> Result<PromptResult, String> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| format!("Failed to parse prompt result: {}", e)),
        None => Ok(PromptResult::default()),
    }
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
        let request = create_new_session_request("/tmp/work", vec![]);
        assert_eq!(request.method, "session/new");
        let params = request.params.unwrap();
        assert_eq!(params["cwd"], "/tmp/work");
    }

    #[test]
    fn test_prompt_request() {
        let request = create_prompt_request("p-1", "s-1", "hello");
        assert_eq!(request.method, "session/prompt");
        let params = request.params.unwrap();
        assert_eq!(params["sessionId"], "s-1");
        assert_eq!(params["prompt"][0]["text"], "hello");
    }
}
