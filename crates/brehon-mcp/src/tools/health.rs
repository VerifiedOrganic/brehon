//! Health check tool for MCP.
//!
//! Returns server uptime (seconds since process start) and version.

use async_trait::async_trait;
use serde_json::Value;
use std::time::Instant;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{text_result, Tool};

static START_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn server_start_time() -> &'static Instant {
    START_TIME.get_or_init(Instant::now)
}

/// Initialize the server start time. Call once at startup.
pub fn init_start_time() {
    let _ = server_start_time();
}

/// MCP tool that returns server health status, uptime, and version.
pub struct HealthCheckTool;

impl Default for HealthCheckTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthCheckTool {
    /// Create a new health check tool instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for HealthCheckTool {
    fn name(&self) -> &str {
        "health"
    }

    fn description(&self) -> &str {
        "Health check endpoint. Returns server uptime and version."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, McpError> {
        let uptime_secs = server_start_time().elapsed().as_secs();

        let response = serde_json::json!({
            "status": "ok",
            "uptime_seconds": uptime_secs,
            "version": env!("CARGO_PKG_VERSION")
        });

        let result_json = serde_json::to_string_pretty(&response)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        Ok(text_result(result_json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ContentBlock;
    use crate::tools::Tool;

    #[tokio::test]
    async fn test_health_check_returns_json_response() {
        init_start_time();
        let tool = HealthCheckTool::new();

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.is_error.is_none());
        assert_eq!(result.content.len(), 1);

        if let ContentBlock::Text { text } = &result.content[0] {
            let parsed: serde_json::Value =
                serde_json::from_str(text).expect("Response should be valid JSON");
            assert!(parsed.is_object(), "Response should be a JSON object");
        } else {
            panic!("Expected text content block");
        }
    }

    #[tokio::test]
    async fn test_health_check_contains_status_ok() {
        init_start_time();
        let tool = HealthCheckTool::new();

        let result = tool.execute(serde_json::json!({})).await.unwrap();

        if let ContentBlock::Text { text } = &result.content[0] {
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(parsed["status"], "ok");
        } else {
            panic!("Expected text content block");
        }
    }

    #[tokio::test]
    async fn test_health_check_contains_uptime() {
        init_start_time();
        let tool = HealthCheckTool::new();

        let result = tool.execute(serde_json::json!({})).await.unwrap();

        if let ContentBlock::Text { text } = &result.content[0] {
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            assert!(
                parsed["uptime_seconds"].is_u64(),
                "uptime_seconds should be a non-negative integer"
            );
        } else {
            panic!("Expected text content block");
        }
    }

    #[tokio::test]
    async fn test_health_check_contains_version() {
        init_start_time();
        let tool = HealthCheckTool::new();

        let result = tool.execute(serde_json::json!({})).await.unwrap();

        if let ContentBlock::Text { text } = &result.content[0] {
            let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
            let version = parsed["version"]
                .as_str()
                .expect("version should be a string");
            assert!(!version.is_empty(), "version should not be empty");
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
        } else {
            panic!("Expected text content block");
        }
    }

    #[test]
    fn test_health_check_tool_metadata() {
        let tool = HealthCheckTool::new();
        assert_eq!(tool.name(), "health");
        assert!(!tool.description().is_empty());

        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
    }
}
