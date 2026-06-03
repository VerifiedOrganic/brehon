//! Mock Gateway implementation for testing.
//!
//! Provides scripted responses, configurable delays, simulated crashes,
//! capability negotiation, and terminal behavior simulation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use uuid::Uuid;

use brehon_ports::{AgentGateway, PortError};
use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};

use crate::mock_agent::MockBehavior;

/// Recorded call for assertions.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub method: String,
    pub args: serde_json::Value,
    pub timestamp: chrono::DateTime<Utc>,
}

/// Scripted response for mock gateway.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub response: Result<serde_json::Value, String>,
    pub delay: Option<Duration>,
}

/// Mock agent session state.
#[derive(Debug)]
struct SessionState {
    spec: SessionSpec,
    capabilities: AgentCapabilities,
    behavior: MockBehavior,
    prompts_sent: usize,
    alive: bool,
}

/// Mock implementation of AgentGateway for testing.
///
/// Features:
/// - Scripted responses (sequence of predetermined responses)
/// - Configurable delays
/// - Simulated crashes
/// - Capability negotiation
/// - Terminal behavior simulation
/// - Track all calls for assertions
#[derive(Debug, Clone)]
pub struct MockGateway {
    inner: Arc<RwLock<MockGatewayInner>>,
}

#[derive(Debug)]
struct MockGatewayInner {
    sessions: HashMap<String, SessionState>,
    sessions_by_agent: HashMap<String, Vec<String>>,
    calls: Vec<RecordedCall>,
    default_behavior: MockBehavior,
    default_capabilities: AgentCapabilities,
    crash_after_create: Option<usize>,
    sessions_created: usize,
}

impl Default for MockGatewayInner {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            sessions_by_agent: HashMap::new(),
            calls: Vec::new(),
            default_behavior: MockBehavior::default(),
            default_capabilities: AgentCapabilities {
                content_block_types: vec!["text".into()],
                session_config_options: vec![],
                permission_support: true,
                terminal_support: true,
                tool_call_streaming: ToolCallStreaming::Full,
            },
            crash_after_create: None,
            sessions_created: 0,
        }
    }
}

impl MockGateway {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MockGatewayInner::default())),
        }
    }

    pub fn with_default_behavior(behavior: MockBehavior) -> Self {
        let gw = Self::new();
        gw.inner.write().default_behavior = behavior;
        gw
    }

    pub fn with_capabilities(capabilities: AgentCapabilities) -> Self {
        let gw = Self::new();
        gw.inner.write().default_capabilities = capabilities;
        gw
    }

    pub fn set_crash_after_create(&self, count: usize) {
        self.inner.write().crash_after_create = Some(count);
    }

    pub fn add_session_with_behavior(
        &self,
        session_id: &str,
        spec: SessionSpec,
        behavior: MockBehavior,
        _responses: Vec<ScriptedResponse>,
    ) {
        let mut inner = self.inner.write();

        let capabilities = inner.default_capabilities.clone();
        let session_state = SessionState {
            spec,
            capabilities,
            behavior,
            prompts_sent: 0,
            alive: true,
        };

        inner
            .sessions_by_agent
            .entry(session_state.spec.agent_id.as_str().to_string())
            .or_default()
            .push(session_id.to_string());

        inner.sessions.insert(session_id.to_string(), session_state);
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.inner.read().calls.clone()
    }

    pub fn prompts_sent(&self, session: &SessionId) -> usize {
        self.inner
            .read()
            .sessions
            .get(session.as_str())
            .map(|s| s.prompts_sent)
            .unwrap_or(0)
    }

    pub fn session_count(&self) -> usize {
        self.inner.read().sessions.len()
    }

    pub fn live_sessions(&self) -> Vec<SessionId> {
        self.inner
            .read()
            .sessions
            .iter()
            .filter(|(_, s)| s.alive)
            .map(|(id, _)| SessionId::new(id))
            .collect()
    }

    pub fn is_session_alive(&self, session: &SessionId) -> bool {
        self.inner
            .read()
            .sessions
            .get(session.as_str())
            .map(|s| s.alive)
            .unwrap_or(false)
    }

    fn record_call(&self, method: &str, args: serde_json::Value) {
        self.inner.write().calls.push(RecordedCall {
            method: method.to_string(),
            args,
            timestamp: Utc::now(),
        });
    }

    async fn apply_delay(behavior: &MockBehavior) {
        if !behavior.response_delay.is_zero() {
            tokio::time::sleep(behavior.response_delay).await;
        }
    }
}

impl Default for MockGateway {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentGateway for MockGateway {
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId, PortError> {
        self.record_call("spawn", serde_json::to_value(&spec).unwrap());

        let mut inner = self.inner.write();

        inner.sessions_created += 1;

        if let Some(crash_after) = inner.crash_after_create {
            if inner.sessions_created > crash_after {
                return Err(PortError::Agent("simulated crash".into()));
            }
        }

        let session_id = SessionId::new(Uuid::new_v4().to_string());
        let behavior = inner.default_behavior.clone();
        let capabilities = inner.default_capabilities.clone();

        let session_state = SessionState {
            spec,
            capabilities,
            behavior,
            prompts_sent: 0,
            alive: true,
        };

        inner
            .sessions_by_agent
            .entry(session_state.spec.agent_id.as_str().to_string())
            .or_default()
            .push(session_id.as_str().to_string());

        inner
            .sessions
            .insert(session_id.as_str().to_string(), session_state);

        Ok(session_id)
    }

    async fn set_config(
        &self,
        session: &SessionId,
        option: &str,
        value: &str,
    ) -> Result<(), PortError> {
        self.record_call(
            "set_config",
            serde_json::json!({
                "session": session.as_str(),
                "option": option,
                "value": value
            }),
        );

        let inner = self.inner.read();
        if !inner.sessions.contains_key(session.as_str()) {
            return Err(PortError::Agent("session not found".into()));
        }

        Ok(())
    }

    async fn send_prompt(
        &self,
        session: &SessionId,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, PortError> {
        self.record_call(
            "send_prompt",
            serde_json::json!({
                "session": session.as_str(),
                "prompt": prompt.content
            }),
        );

        let behavior = {
            let mut inner = self.inner.write();

            let session_state = inner
                .sessions
                .get_mut(session.as_str())
                .ok_or_else(|| PortError::Agent("session not found".into()))?;

            if !session_state.alive {
                return Err(PortError::Agent("session is dead".into()));
            }

            let behavior = session_state.behavior.clone();

            if let Some(crash_after) = behavior.crash_after_message {
                session_state.prompts_sent += 1;
                if session_state.prompts_sent >= crash_after {
                    session_state.alive = false;
                    return Err(PortError::Agent("simulated crash".into()));
                }
            }

            behavior
        };

        Self::apply_delay(&behavior).await;

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id.clone(),
            session_id: session.as_str().to_string(),
            created_at: Utc::now(),
        })
    }

    async fn cancel_prompt(&self, session: &SessionId, prompt: &PromptId) -> Result<(), PortError> {
        self.record_call(
            "cancel_prompt",
            serde_json::json!({
                "session": session.as_str(),
                "prompt": prompt.as_str()
            }),
        );

        let inner = self.inner.read();
        if !inner.sessions.contains_key(session.as_str()) {
            return Err(PortError::Agent("session not found".into()));
        }

        Ok(())
    }

    async fn attach_terminal(&self, session: &SessionId) -> Result<Option<TerminalId>, PortError> {
        self.record_call(
            "attach_terminal",
            serde_json::json!({ "session": session.as_str() }),
        );

        let inner = self.inner.read();
        let session_state = inner
            .sessions
            .get(session.as_str())
            .ok_or_else(|| PortError::Agent("session not found".into()))?;

        if !session_state.alive {
            return Err(PortError::Agent("session is dead".into()));
        }

        if !session_state.capabilities.terminal_support {
            return Ok(None);
        }

        Ok(Some(TerminalId::new(Uuid::new_v4().to_string())))
    }

    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), PortError> {
        self.record_call(
            "send_terminal_input",
            serde_json::json!({
                "input_len": input.len()
            }),
        );

        Ok(())
    }

    async fn resolve_permission(
        &self,
        session: &SessionId,
        permission_id: &str,
        approved: bool,
    ) -> Result<(), PortError> {
        self.record_call(
            "resolve_permission",
            serde_json::json!({
                "session": session.as_str(),
                "permission_id": permission_id,
                "approved": approved
            }),
        );

        let inner = self.inner.read();
        if !inner.sessions.contains_key(session.as_str()) {
            return Err(PortError::Agent("session not found".into()));
        }

        Ok(())
    }

    async fn kill_session(&self, session: &SessionId) -> Result<(), PortError> {
        self.record_call(
            "kill_session",
            serde_json::json!({ "session": session.as_str() }),
        );

        let mut inner = self.inner.write();

        if let Some(session_state) = inner.sessions.get_mut(session.as_str()) {
            session_state.alive = false;
            Ok(())
        } else {
            Err(PortError::Agent("session not found".into()))
        }
    }

    async fn health_check(&self, session: &SessionId) -> Result<HealthStatus, PortError> {
        self.record_call(
            "health_check",
            serde_json::json!({ "session": session.as_str() }),
        );

        let inner = self.inner.read();
        let session_state = inner
            .sessions
            .get(session.as_str())
            .ok_or_else(|| PortError::Agent("session not found".into()))?;

        if session_state.alive {
            Ok(HealthStatus::Healthy)
        } else {
            Ok(HealthStatus::Unhealthy)
        }
    }

    async fn capabilities(&self, session: &SessionId) -> Result<AgentCapabilities, PortError> {
        self.record_call(
            "capabilities",
            serde_json::json!({ "session": session.as_str() }),
        );

        let inner = self.inner.read();
        let session_state = inner
            .sessions
            .get(session.as_str())
            .ok_or_else(|| PortError::Agent("session not found".into()))?;

        Ok(session_state.capabilities.clone())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, PortError> {
        self.record_call("list_sessions", serde_json::json!({}));

        let inner = self.inner.read();

        let sessions: Vec<SessionInfo> = inner
            .sessions
            .iter()
            .filter(|(_, s)| s.alive)
            .map(|(id, s)| SessionInfo {
                session_id: SessionId::new(id),
                agent_id: s.spec.agent_id.clone(),
                role: s.spec.role.clone(),
                health: if s.alive {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unhealthy
                },
                created_at: Utc::now(),
                capabilities: s.capabilities.clone(),
            })
            .collect();

        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_and_list_sessions() {
        let gw = MockGateway::new();

        let session = gw
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let sessions = gw.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session);
    }

    #[tokio::test]
    async fn kill_session() {
        let gw = MockGateway::new();

        let session = gw
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        assert!(gw.is_session_alive(&session));

        gw.kill_session(&session).await.unwrap();

        assert!(!gw.is_session_alive(&session));
    }

    #[tokio::test]
    async fn simulate_crash_after_message() {
        let behavior = MockBehavior {
            crash_after_message: Some(3),
            ..Default::default()
        };
        let gw = MockGateway::with_default_behavior(behavior);

        let session = gw
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let prompt1 = PromptTurn {
            prompt_id: PromptId::new("p1"),
            content: "message 1".into(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: Utc::now(),
        };
        let prompt2 = PromptTurn {
            prompt_id: PromptId::new("p2"),
            content: "message 2".into(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: Utc::now(),
        };
        let prompt3 = PromptTurn {
            prompt_id: PromptId::new("p3"),
            content: "message 3".into(),
            kind: brehon_types::MessageKind::TaskAssignment,
            sent_at: Utc::now(),
        };

        gw.send_prompt(&session, prompt1).await.unwrap();
        gw.send_prompt(&session, prompt2).await.unwrap();

        let result = gw.send_prompt(&session, prompt3).await;
        assert!(result.is_err());
        assert!(!gw.is_session_alive(&session));
    }

    #[tokio::test]
    async fn calls_recorded() {
        let gw = MockGateway::new();

        gw.spawn(SessionSpec::new(
            brehon_types::AgentId::new("agent-1"),
            "worker".into(),
            "/tmp/test".into(),
        ))
        .await
        .unwrap();

        let calls = gw.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "spawn");
    }

    #[tokio::test]
    async fn terminal_support_capability() {
        let caps = AgentCapabilities {
            content_block_types: vec!["text".into()],
            session_config_options: vec![],
            permission_support: true,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::None,
        };
        let gw = MockGateway::with_capabilities(caps);

        let session = gw
            .spawn(SessionSpec::new(
                brehon_types::AgentId::new("agent-1"),
                "worker".into(),
                "/tmp/test".into(),
            ))
            .await
            .unwrap();

        let terminal = gw.attach_terminal(&session).await.unwrap();
        assert!(terminal.is_none());
    }
}
