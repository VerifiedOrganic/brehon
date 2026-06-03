//! MCP tools for Brehon.
//!
//! This module defines the tools exposed by the MCP server.

pub mod advisor;
pub mod agent;
pub(crate) mod assignment_observability;
pub(crate) mod context_efficiency;
pub mod factory;
pub mod freshness;
pub(crate) mod git_cherry_pick;
pub mod health;
pub mod memory;
pub(crate) mod proof_summary;
pub mod research;
pub(crate) mod routing;
pub mod rules;
pub mod skills;
pub mod stability;
pub mod task_actions;
pub mod tasks;
#[cfg(test)]
pub(crate) mod test_support;
pub mod verification;

use async_trait::async_trait;
use serde_json::Value;

#[cfg(test)]
pub(crate) use brehon_test_harness::{ScopedEnv, TEST_ENV_LOCK};

use crate::error::McpError;
use crate::server::{ContentBlock, ToolResult};

/// Trait implemented by every MCP tool. Provides metadata for listing and
/// an async `execute` method for handling tool calls.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The unique tool name exposed to MCP clients.
    fn name(&self) -> &str;
    /// A human-readable description of what the tool does.
    fn description(&self) -> &str;
    /// JSON Schema describing the tool's accepted input parameters.
    fn input_schema(&self) -> Value;
    /// Optional per-tool argument byte limit, tighter than the server default.
    fn max_argument_bytes(&self) -> Option<usize> {
        None
    }
    /// Execute the tool with the given JSON arguments and return a result.
    async fn execute(&self, args: Value) -> Result<ToolResult, McpError>;
}

fn text_result(text: impl Into<String>) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text { text: text.into() }],
        is_error: None,
    }
}

fn error_result(text: impl Into<String>) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text { text: text.into() }],
        is_error: Some(true),
    }
}

pub(crate) fn structured_error_result(
    error_code: &str,
    message: impl Into<String>,
    retryable: bool,
    current_state: Value,
    allowed_next_actions: Vec<Value>,
    next_action: Value,
) -> ToolResult {
    let payload = serde_json::json!({
        "error_code": error_code,
        "message": message.into(),
        "retryable": retryable,
        "current_state": current_state,
        "allowed_next_actions": allowed_next_actions,
        "next_action": next_action,
    });
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| {
        format!(
            "{{\"error_code\":\"{}\",\"message\":\"failed to serialize structured error\",\"retryable\":false,\"current_state\":null,\"allowed_next_actions\":[],\"next_action\":{{\"kind\":\"none\"}}}}",
            error_code
        )
    });
    error_result(text)
}
