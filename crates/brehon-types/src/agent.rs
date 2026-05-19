//! Agent-related types for session management and capability negotiation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::Hash;

/// Unique identifier for an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AgentId(pub String);

impl AgentId {
    /// Create a new `AgentId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Kind of agent adapter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AdapterKind {
    /// ACP-compatible agent.
    Acp,
    /// OpenAI Codex CLI app-server websocket.
    Codex,
    /// OpenAI-compatible direct HTTP API.
    OpenAiCompatible,
    /// Brehon-native ACP runtime.
    NativeAgent,
    /// Native PTY hooks integration (Claude).
    PtyHooks,
    /// Mock agent for testing.
    Mock,
    /// Kimi Code CLI adapter.
    Kimi,
    /// JetBrains Junie CLI adapter.
    Junie,
    /// GitHub Copilot CLI adapter.
    Copilot,
}

/// Configuration for spawning an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    /// Which adapter to use.
    pub adapter: AdapterKind,
    /// Command to invoke.
    pub command: String,
    /// Arguments for the command.
    pub args: Vec<String>,
}

/// Unique identifier for a session (a running agent instance).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    /// Create a new `SessionId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Specification for spawning an agent session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSpec {
    /// Which agent to use.
    pub agent_id: AgentId,
    /// Role assignment for this session.
    pub role: String,
    /// Working directory for the session.
    pub worktree_path: String,
    /// Optional merge target branch for subtask worktrees.
    /// When set, the worker branch should be created from this branch
    /// instead of the default branch (main/master).
    #[serde(default)]
    pub merge_target: Option<String>,
}

impl SessionSpec {
    /// Create a new SessionSpec with required fields.
    pub fn new(agent_id: AgentId, role: String, worktree_path: String) -> Self {
        Self {
            agent_id,
            role,
            worktree_path,
            merge_target: None,
        }
    }
}

/// Capabilities that an agent session supports.
///
/// Negotiated during session setup. Different agents expose different
/// subsets of ACP, so all downstream code must check these capabilities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// Content block types the agent can receive.
    pub content_block_types: Vec<String>,
    /// Session config options the agent supports.
    pub session_config_options: Vec<String>,
    /// Whether the agent supports permission callbacks.
    pub permission_support: bool,
    /// Whether the agent supports interactive terminals.
    pub terminal_support: bool,
    /// Fidelity of tool-call status streaming.
    pub tool_call_streaming: ToolCallStreaming,
}

/// Tool-call streaming fidelity level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ToolCallStreaming {
    /// No streaming.
    None,
    /// Basic status updates.
    Basic,
    /// Full streaming with details.
    Full,
}

/// Health status of an agent session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HealthStatus {
    /// Session is healthy and responsive.
    Healthy,
    /// Session is unhealthy or unresponsive.
    Unhealthy,
    /// Health status unknown.
    Unknown,
}

/// Information about an active session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session identifier.
    pub session_id: SessionId,
    /// Agent running this session.
    pub agent_id: AgentId,
    /// Role this session is filling.
    pub role: String,
    /// Current health status.
    pub health: HealthStatus,
    /// When this session was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Capabilities negotiated for this session.
    pub capabilities: AgentCapabilities,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    #[test]
    fn agent_id_display_and_hash() {
        let id1 = AgentId::new("agent-1");
        let id2 = AgentId::new("agent-1");
        let id3 = AgentId::new("agent-2");

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
        assert_eq!(format!("{}", id1), "agent-1");

        let mut hasher = DefaultHasher::new();
        id1.hash(&mut hasher);
        let hash1 = hasher.finish();

        let mut hasher = DefaultHasher::new();
        id2.hash(&mut hasher);
        let hash2 = hasher.finish();

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn session_id_serialization() {
        let id = SessionId::new("session-123");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""session-123""#);
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn adapter_kind_roundtrip() {
        let kind = AdapterKind::Acp;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, r#""Acp""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::Acp);

        let direct = AdapterKind::OpenAiCompatible;
        let json = serde_json::to_string(&direct).unwrap();
        assert_eq!(json, r#""OpenAiCompatible""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::OpenAiCompatible);

        let native = AdapterKind::NativeAgent;
        let json = serde_json::to_string(&native).unwrap();
        assert_eq!(json, r#""NativeAgent""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::NativeAgent);

        let kimi = AdapterKind::Kimi;
        let json = serde_json::to_string(&kimi).unwrap();
        assert_eq!(json, r#""Kimi""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::Kimi);

        let junie = AdapterKind::Junie;
        let json = serde_json::to_string(&junie).unwrap();
        assert_eq!(json, r#""Junie""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::Junie);

        let copilot = AdapterKind::Copilot;
        let json = serde_json::to_string(&copilot).unwrap();
        assert_eq!(json, r#""Copilot""#);
        let parsed: AdapterKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, AdapterKind::Copilot);
    }

    #[test]
    fn agent_capabilities_serialization() {
        let caps = AgentCapabilities {
            content_block_types: vec!["text".into(), "image".into()],
            session_config_options: vec!["model".into()],
            permission_support: true,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::Basic,
        };
        let json = serde_json::to_string(&caps).unwrap();
        let parsed: AgentCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, parsed);
    }
}
