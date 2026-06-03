//! Mock MCP server for chaos and soak testing.
//!
//! This is a simplified stand-in for `brehon_mcp::server::McpServer` that
//! captures the key failure modes from STABILITY_REVIEW §2.8–2.10:
//! - Stubbed/unbacked tool responses
//! - Panic isolation boundaries
//! - Request-size bounds
//! - In-flight work tracking during drain
//!
//! Using this mock lets the `brehon-test-harness` crate test MCP paths
//! without pulling in the full `brehon-mcp` → `brehon-review` → `brehon-mux`
//! dependency chain (which transitively depends on the Zig-built
//! `ghostty_vt_sys` crate).

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::FutureExt;
use parking_lot::RwLock;
use serde_json::Value;

use brehon_types::drain;

/// Error type returned by the mock MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum MockMcpError {
    ToolNotFound(String),
    InvalidRequest(String),
    Internal(String),
    OversizedPayload { size: usize, max: usize },
}

impl std::fmt::Display for MockMcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MockMcpError::ToolNotFound(t) => write!(f, "Tool not found: {t}"),
            MockMcpError::InvalidRequest(t) => write!(f, "Invalid request: {t}"),
            MockMcpError::Internal(t) => write!(f, "Internal error: {t}"),
            MockMcpError::OversizedPayload { size, max } => {
                write!(f, "Payload {size} bytes exceeds max {max}")
            }
        }
    }
}

impl std::error::Error for MockMcpError {}

/// Result of a mock tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct MockToolResult {
    pub text: String,
    pub is_error: bool,
}

/// Trait for mock MCP tools.
#[async_trait::async_trait]
pub trait MockTool: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&self, args: Value) -> Result<MockToolResult, MockMcpError>;
}

/// A mock MCP server with panic boundaries, size limits, and drain tracking.
pub struct MockMcpServer {
    tools: RwLock<HashMap<String, Arc<dyn MockTool>>>,
    call_count: AtomicUsize,
    max_payload_bytes: usize,
}

impl std::fmt::Debug for MockMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockMcpServer")
            .field("tools", &self.tools.read().keys().collect::<Vec<_>>())
            .field("call_count", &self.call_count.load(Ordering::SeqCst))
            .field("max_payload_bytes", &self.max_payload_bytes)
            .finish()
    }
}

impl MockMcpServer {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            call_count: AtomicUsize::new(0),
            max_payload_bytes: 256 * 1024,
        }
    }

    pub fn with_max_payload_bytes(mut self, max: usize) -> Self {
        self.max_payload_bytes = max;
        self
    }

    pub fn register_tool(&self, tool: Arc<dyn MockTool>) {
        self.tools.write().insert(tool.name().to_string(), tool);
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Call a tool with panic isolation, size limits, and drain tracking.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: Value,
    ) -> Result<MockToolResult, MockMcpError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);

        // Track in-flight work (mirrors real McpServer behavior)
        let _guard = drain::in_flight_guard(&format!("mock_mcp:{tool_name}"));

        if drain::is_draining() {
            return Err(MockMcpError::InvalidRequest(format!(
                "Shutdown in progress — tool {tool_name} rejected during drain"
            )));
        }

        let tool = self
            .tools
            .read()
            .get(tool_name)
            .cloned()
            .ok_or_else(|| MockMcpError::ToolNotFound(tool_name.to_string()))?;

        // Size check
        let payload_size = serde_json::to_string(&args).unwrap_or_default().len();
        if payload_size > self.max_payload_bytes {
            return Err(MockMcpError::OversizedPayload {
                size: payload_size,
                max: self.max_payload_bytes,
            });
        }

        // Panic boundary
        match AssertUnwindSafe(tool.execute(args)).catch_unwind().await {
            Ok(result) => result,
            Err(_payload) => Err(MockMcpError::Internal(format!("Tool {tool_name} panicked"))),
        }
    }
}

impl Default for MockMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple echo tool for testing.
#[derive(Debug, Clone)]
pub struct MockEchoTool {
    pub name: String,
}

#[async_trait::async_trait]
impl MockTool for MockEchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    async fn execute(&self, args: Value) -> Result<MockToolResult, MockMcpError> {
        let text = args
            .get("payload")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(MockToolResult {
            text,
            is_error: false,
        })
    }
}

/// A tool that panics on configurable intervals.
#[derive(Debug, Clone)]
pub struct MockPanicTool {
    pub name: String,
    pub call_count: Arc<AtomicUsize>,
    /// 1-indexed call number that should panic.
    pub panic_on_call: Option<usize>,
}

#[async_trait::async_trait]
impl MockTool for MockPanicTool {
    fn name(&self) -> &str {
        &self.name
    }

    async fn execute(&self, _args: Value) -> Result<MockToolResult, MockMcpError> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if self.panic_on_call == Some(count + 1) {
            panic!("intentional panic in {}", self.name);
        }
        Ok(MockToolResult {
            text: "ok".to_string(),
            is_error: false,
        })
    }
}

/// A tool that simulates variable latency.
#[derive(Debug, Clone)]
pub struct MockSlowTool {
    pub name: String,
    pub delay_ms: u64,
}

#[async_trait::async_trait]
impl MockTool for MockSlowTool {
    fn name(&self) -> &str {
        &self.name
    }

    async fn execute(&self, _args: Value) -> Result<MockToolResult, MockMcpError> {
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        Ok(MockToolResult {
            text: "slow-ok".to_string(),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_mcp_echo_works() {
        let server = MockMcpServer::new();
        server.register_tool(Arc::new(MockEchoTool {
            name: "echo".to_string(),
        }));

        let result = server
            .call_tool("echo", serde_json::json!({"payload": "hello"}))
            .await
            .unwrap();
        assert_eq!(result.text, "hello");
    }

    #[tokio::test]
    async fn mock_mcp_catches_panic() {
        drain::reset_draining_for_test();
        let server = MockMcpServer::new();
        server.register_tool(Arc::new(MockPanicTool {
            name: "panic".to_string(),
            call_count: Arc::new(AtomicUsize::new(0)),
            panic_on_call: Some(2),
        }));

        // First call succeeds (panic_on_call = 2)
        let r1 = server.call_tool("panic", serde_json::json!({})).await;
        assert!(r1.is_ok());

        // Second call panics but is caught
        let r2 = server.call_tool("panic", serde_json::json!({})).await;
        assert!(matches!(r2, Err(MockMcpError::Internal(_))));
    }

    #[tokio::test]
    async fn mock_mcp_size_limit() {
        drain::reset_draining_for_test();
        let server = MockMcpServer::new().with_max_payload_bytes(64);
        server.register_tool(Arc::new(MockEchoTool {
            name: "echo".to_string(),
        }));

        let result = server
            .call_tool("echo", serde_json::json!({"payload": "x".repeat(256)}))
            .await;
        assert!(
            matches!(result, Err(MockMcpError::OversizedPayload { max: 64, .. })),
            "Expected oversized payload error, got {:?}",
            result
        );
    }
}
