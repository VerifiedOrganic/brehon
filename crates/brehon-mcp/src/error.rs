//! Error types for the MCP server.

use thiserror::Error;

/// Error types returned by MCP server operations and tool execution.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("JSON-RPC error: {0}")]
    JsonRpc(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Tool execution error: {0}")]
    ToolExecution(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("MCP protocol error: {0}")]
    Protocol(String),

    #[error("Internal MCP error: {0}")]
    Internal(String),
}

impl From<serde_json::Error> for McpError {
    fn from(err: serde_json::Error) -> Self {
        McpError::Serialization(err.to_string())
    }
}
