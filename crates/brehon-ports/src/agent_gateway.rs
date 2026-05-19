//! AgentGateway trait for agent session management.

use async_trait::async_trait;

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId,
};

use crate::PortError;

/// Trait for communicating with AI agent sessions.
///
/// This trait abstracts the communication protocol (ACP over stdio) so that
/// different agent adapters can be plugged in without changing core logic.
///
/// # Degraded Behavior for Unsupported Capabilities
///
/// Agent capabilities are negotiated during session setup. When an operation
/// is requested that the agent doesn't support, implementations must follow
/// these guidelines:
///
/// ## Terminal Operations (`attach_terminal`, `send_terminal_input`)
///
/// If `capabilities.terminal_support == false`:
/// - `attach_terminal` returns `Ok(None)` - no terminal attached
/// - The calling code should use transcript fallback mode instead
/// - No error is raised; this is expected behavior for non-terminal agents
///
/// ## Session Config Options
///
/// If a requested config option is not in `capabilities.session_config_options`:
/// - If the option is marked as required in the config, return an error
/// - If the option is optional, silently skip it
///
/// ## Permission Callbacks
///
/// If `capabilities.permission_support == false`:
/// - Permission requests from the agent should not be forwarded
/// - All permissions must use the configured default (allow/deny)
///
/// If `capabilities.permission_support == true`:
/// - Permission requests may arrive out-of-band through the session event stream
/// - Callers should resolve each pending request via [`AgentGateway::resolve_permission`]
///   before the session's configured timeout policy applies
///
/// ## Tool Call Streaming
///
/// If `capabilities.tool_call_streaming == ToolCallStreaming::None`:
/// - No streaming tool status is available
/// - Tool calls appear as complete results only
///
/// Implementations should log warnings when capability negotiation reduces
/// functionality but not raise errors unless required functionality is missing.
#[async_trait]
pub trait AgentGateway: Send + Sync {
    /// Spawn a new agent session.
    ///
    /// Creates a new session according to the provided specification.
    /// Returns the session ID for subsequent operations.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The agent command cannot be spawned
    /// - Session initialization fails
    /// - Capability negotiation fails
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId, PortError>;

    /// Set a configuration option on a session.
    ///
    /// # Degraded Behavior
    ///
    /// If the option is not supported, the behavior depends on whether
    /// the option is required. Unsupported required options should return
    /// an error; unsupported optional options should be silently ignored.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - The option is required but not supported
    /// - Setting the option fails
    async fn set_config(
        &self,
        session: &SessionId,
        option: &str,
        value: &str,
    ) -> Result<(), PortError>;

    /// Send a prompt to the agent session.
    ///
    /// Returns a handle for tracking the prompt response.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - The prompt cannot be sent
    async fn send_prompt(
        &self,
        session: &SessionId,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, PortError>;

    /// Cancel an in-flight prompt.
    ///
    /// Attempts to stop a prompt that hasn't completed yet.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - The prompt doesn't exist or already completed
    async fn cancel_prompt(&self, session: &SessionId, prompt: &PromptId) -> Result<(), PortError>;

    /// Attach to an interactive terminal for the session.
    ///
    /// Returns `Ok(Some(terminal_id))` if terminal support is available and
    /// the attachment succeeds.
    /// Returns `Ok(None)` if the agent doesn't support terminals.
    ///
    /// # Degraded Behavior
    ///
    /// If `capabilities.terminal_support == false`, this method returns
    /// `Ok(None)` without error. The caller should use transcript fallback.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - Terminal attachment fails (when terminal support is advertised)
    async fn attach_terminal(&self, session: &SessionId) -> Result<Option<TerminalId>, PortError>;

    /// Send input to an attached terminal.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - The terminal doesn't exist
    /// - Sending input fails
    async fn send_terminal_input(
        &self,
        terminal: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), PortError>;

    /// Resolve a pending permission request for a session.
    ///
    /// `approved = true` selects an allow/approve option when available.
    /// `approved = false` selects a deny/cancel option when available.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - The session transport does not support mediated permissions
    /// - The permission request no longer exists
    async fn resolve_permission(
        &self,
        session: &SessionId,
        permission_id: &str,
        approved: bool,
    ) -> Result<(), PortError>;

    /// Kill a session.
    ///
    /// Terminates the agent process. First attempts graceful shutdown,
    /// then forces termination after a timeout.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - Termination fails
    async fn kill_session(&self, session: &SessionId) -> Result<(), PortError>;

    /// Check the health of a session.
    ///
    /// Returns the current health status of the agent session.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if:
    /// - The session doesn't exist
    /// - Health check cannot be performed
    async fn health_check(&self, session: &SessionId) -> Result<HealthStatus, PortError>;

    /// Get the capabilities of a session.
    ///
    /// Returns the negotiated capabilities for this session.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if the session doesn't exist.
    async fn capabilities(&self, session: &SessionId) -> Result<AgentCapabilities, PortError>;

    /// List all active sessions.
    ///
    /// Returns information about all sessions managed by this gateway.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Agent` if listing fails.
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_error_agent_variant() {
        let e = PortError::Agent("spawn failed".into());
        assert!(matches!(e, PortError::Agent(_)));
    }
}
