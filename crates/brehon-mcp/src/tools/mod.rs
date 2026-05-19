//! MCP tools for Brehon.
//!
//! This module defines the tools exposed by the MCP server.

pub mod advisor;
pub mod agent;
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
pub mod verification;

use async_trait::async_trait;
use serde_json::Value;
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

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
