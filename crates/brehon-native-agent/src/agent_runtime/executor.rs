// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's `zeph-tools/src/executor.rs` tool runtime contract.

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::agent_runtime::message::ToolUseRequest;
use crate::runtime::CancellationToken;
use crate::server::RpcHandle;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) tool_id: String,
    pub(crate) params: Map<String, Value>,
    pub(crate) caller_id: Option<String>,
}

impl ToolCall {
    pub(crate) fn from_request(request: ToolUseRequest, caller_id: Option<String>) -> Self {
        let params = match request.arguments {
            Value::Object(map) => map,
            value => {
                let mut map = Map::new();
                map.insert("value".to_string(), value);
                map
            }
        };
        Self {
            id: request.id,
            tool_id: request.name,
            params,
            caller_id,
        }
    }

    pub(crate) fn params_value(&self) -> Value {
        Value::Object(self.params.clone())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolOutput {
    pub(crate) tool_name: String,
    pub(crate) summary: String,
    pub(crate) raw_response: Option<Value>,
    pub(crate) streamed: bool,
    pub(crate) terminal_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolError {
    #[error("tool invocation cancelled")]
    Cancelled,
    #[error("invalid tool parameters: {0}")]
    InvalidParams(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("execution failed: {0}")]
    Execution(String),
}

impl ToolError {
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Cancelled | Self::InvalidParams(_) | Self::PermissionDenied(_) => false,
            Self::Execution(message) => {
                let lower = message.to_ascii_lowercase();
                lower.contains("timed out")
                    || lower.contains("timeout")
                    || lower.contains("temporarily")
                    || lower.contains("connection")
                    || lower.contains("channel closed")
                    || lower.contains("closed")
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolExecutionContext<'a> {
    pub(crate) rpc: &'a RpcHandle,
    pub(crate) session_id: &'a str,
    pub(crate) cancel: &'a CancellationToken,
}

#[async_trait]
pub(crate) trait ToolExecutor: Send + Sync {
    fn tool_definitions(&self) -> Vec<Value>;

    async fn execute_tool_call(
        &self,
        ctx: ToolExecutionContext<'_>,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError>;

    fn is_tool_retryable(&self, _tool_id: &str) -> bool {
        false
    }
}

pub(crate) const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

pub(crate) fn truncate_tool_output(output: &str) -> String {
    truncate_tool_output_at(output, MAX_TOOL_OUTPUT_CHARS)
}

pub(crate) fn truncate_tool_output_at(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let half = max_chars / 2;
    let head_end = floor_char_boundary(output, half);
    let tail_start = ceil_char_boundary(output, output.len() - half);
    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let truncated = output.len() - head_end - (output.len() - tail_start);

    format!(
        "{head}\n\n... [truncated {truncated} chars, showing first and last ~{half} chars] ...\n\n{tail}"
    )
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_with_head_and_tail() {
        let input = "a".repeat(200);
        let output = truncate_tool_output_at(&input, 80);

        assert!(output.contains("truncated"));
        assert!(output.len() < input.len());
    }

    #[test]
    fn converts_non_object_arguments_to_value_param() {
        let call = ToolCall::from_request(
            ToolUseRequest::new("call-1", "echo", Value::String("hi".to_string())),
            Some("session-1".to_string()),
        );

        assert_eq!(
            call.params.get("value"),
            Some(&Value::String("hi".to_string()))
        );
        assert_eq!(call.caller_id.as_deref(), Some("session-1"));
    }
}
