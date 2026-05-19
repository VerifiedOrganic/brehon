//! Session attachment helpers for ACP integration.
//!
//! This module provides helpers for injecting MCP attachment metadata into
//! ACP session setup, allowing agents to discover and use MCP tools.

use serde::{Deserialize, Serialize};

/// Metadata key used in ACP session setup to carry MCP attachment info.
pub const SESSION_ATTACH_KEY: &str = "x-brehon-mcp-attachment";

/// Describes an MCP server and its tools for injection into ACP session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachment {
    pub mcp_server_name: String,
    pub mcp_server_version: String,
    pub protocol_version: String,
    pub tools: Vec<ToolAttachment>,
}

/// Minimal tool description included in a session attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAttachment {
    pub name: String,
    pub description: String,
}

impl SessionAttachment {
    /// Create a new attachment for the given server name and version.
    pub fn new(server_name: &str, server_version: &str) -> Self {
        Self {
            mcp_server_name: server_name.to_string(),
            mcp_server_version: server_version.to_string(),
            protocol_version: "2024-11-05".to_string(),
            tools: Vec::new(),
        }
    }

    /// Replace the tool list with the provided attachments.
    pub fn with_tools(mut self, tools: Vec<ToolAttachment>) -> Self {
        self.tools = tools;
        self
    }

    /// Append a tool with the given name and description to this attachment.
    pub fn add_tool(&mut self, name: &str, description: &str) {
        self.tools.push(ToolAttachment {
            name: name.to_string(),
            description: description.to_string(),
        });
    }

    /// Serialize this attachment to a JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Deserialize a session attachment from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Convert this attachment into a JSON metadata object keyed by [`SESSION_ATTACH_KEY`].
    pub fn to_session_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            SESSION_ATTACH_KEY: self
        })
    }
}

/// Runtime context for an active agent session, including identity and capabilities.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub agent_id: String,
    pub task_id: Option<String>,
    pub role: String,
    pub capabilities: AgentCapabilities,
}

/// Capabilities declared by an agent during session setup.
#[derive(Debug, Clone, Default)]
pub struct AgentCapabilities {
    pub supports_terminal: bool,
    pub supports_tools: bool,
    pub max_context_tokens: Option<u64>,
}

impl AgentCapabilities {
    /// Create default capabilities (all disabled).
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable terminal access capability.
    pub fn with_terminal(mut self) -> Self {
        self.supports_terminal = true;
        self
    }

    /// Enable tool-use capability.
    pub fn with_tools(mut self) -> Self {
        self.supports_tools = true;
        self
    }

    /// Set the maximum context token budget for this agent.
    pub fn with_max_tokens(mut self, tokens: u64) -> Self {
        self.max_context_tokens = Some(tokens);
        self
    }
}

impl SessionContext {
    /// Create a new session context with the given identifiers and role.
    pub fn new(session_id: &str, agent_id: &str, role: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            task_id: None,
            role: role.to_string(),
            capabilities: AgentCapabilities::new(),
        }
    }

    /// Associate this session with a specific task.
    pub fn with_task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.to_string());
        self
    }

    /// Set the agent capabilities for this session.
    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Serialize this context into a JSON value suitable for MCP tool arguments.
    pub fn to_mcp_context_args(&self) -> serde_json::Value {
        serde_json::json!({
            "session_id": self.session_id,
            "agent_id": self.agent_id,
            "task_id": self.task_id,
            "role": self.role,
            "capabilities": {
                "supports_terminal": self.capabilities.supports_terminal,
                "supports_tools": self.capabilities.supports_tools,
                "max_context_tokens": self.capabilities.max_context_tokens,
            }
        })
    }
}

/// Build the default session attachment listing all standard Brehon MCP tools.
pub fn create_default_attachment() -> SessionAttachment {
    let mut attachment = SessionAttachment::new("brehon-mcp", env!("CARGO_PKG_VERSION"));

    attachment.add_tool(
        "search_memories",
        "Search memories by keyword query. Returns ranked results.",
    );
    attachment.add_tool(
        "create_memory",
        "Create a new memory entry with content and optional tags.",
    );
    attachment.add_tool(
        "get_memories",
        "Fetch full memory bodies by ID after a search/list result.",
    );
    attachment.add_tool(
        "list_memories",
        "List memory summaries, optionally filtered by tag or time range.",
    );
    attachment.add_tool("delete_memory", "Delete a memory by its ID.");

    attachment.add_tool(
        "search_rules",
        "Search project coding rules and conventions.",
    );
    attachment.add_tool("create_rule", "Create a new coding rule or convention.");

    attachment.add_tool(
        "search_skills",
        "Search reusable patterns, templates, and skills.",
    );

    attachment.add_tool(
        "get_task_context",
        "Get current task description, status, and recent event history.",
    );
    attachment.add_tool(
        "list_tasks",
        "List tasks with optional filtering by status.",
    );
    attachment.add_tool(
        "get_task",
        "Get detailed information about a specific task.",
    );

    attachment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_attachment_serialization() {
        let mut attachment = SessionAttachment::new("test-server", "1.0.0");
        attachment.add_tool("test_tool", "A test tool");

        let json = attachment.to_json().unwrap();
        let parsed = SessionAttachment::from_json(&json).unwrap();

        assert_eq!(parsed.mcp_server_name, "test-server");
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].name, "test_tool");
    }

    #[test]
    fn test_session_attachment_metadata() {
        let attachment = SessionAttachment::new("test-server", "1.0.0");
        let metadata = attachment.to_session_metadata();

        assert!(metadata.is_object());
        assert!(metadata.get(SESSION_ATTACH_KEY).is_some());
    }

    #[test]
    fn test_session_context() {
        let context =
            SessionContext::new("session-123", "agent-456", "worker").with_task("task-789");

        let args = context.to_mcp_context_args();

        assert_eq!(args["session_id"], "session-123");
        assert_eq!(args["agent_id"], "agent-456");
        assert_eq!(args["task_id"], "task-789");
        assert_eq!(args["role"], "worker");
    }

    #[test]
    fn test_agent_capabilities() {
        let capabilities = AgentCapabilities::new()
            .with_terminal()
            .with_tools()
            .with_max_tokens(100000);

        assert!(capabilities.supports_terminal);
        assert!(capabilities.supports_tools);
        assert_eq!(capabilities.max_context_tokens, Some(100000));
    }

    #[test]
    fn test_default_attachment() {
        let attachment = create_default_attachment();

        assert_eq!(attachment.mcp_server_name, "brehon-mcp");
        assert!(!attachment.tools.is_empty());

        let tool_names: Vec<&str> = attachment.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"search_memories"));
        assert!(tool_names.contains(&"get_task_context"));
    }
}
