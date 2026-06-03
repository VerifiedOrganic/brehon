use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use brehon_ports::{AgentGateway, PortError};
use brehon_test_harness::MockGateway;
use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId,
};

pub(crate) const INJECTED_KILL_FAILURE: &str = "injected kill failure";

#[derive(Debug, Default)]
struct KillFailureConfig {
    fail_all: bool,
    failed_sessions: HashSet<SessionId>,
}

#[derive(Debug, Clone)]
pub(crate) struct ShutdownTestGateway {
    inner: MockGateway,
    kill_failures: Arc<RwLock<KillFailureConfig>>,
}

impl ShutdownTestGateway {
    pub(crate) fn new() -> Self {
        Self {
            inner: MockGateway::new(),
            kill_failures: Arc::new(RwLock::new(KillFailureConfig::default())),
        }
    }

    pub(crate) fn always_fail_kill() -> Self {
        let gateway = Self::new();
        gateway.kill_failures.write().fail_all = true;
        gateway
    }

    pub(crate) fn fail_kill_for_session(&self, session: SessionId) {
        self.kill_failures.write().failed_sessions.insert(session);
    }

    pub(crate) fn is_session_alive(&self, session: &SessionId) -> bool {
        self.inner.is_session_alive(session)
    }
}

#[async_trait]
impl AgentGateway for ShutdownTestGateway {
    async fn spawn(&self, spec: SessionSpec) -> Result<SessionId, PortError> {
        self.inner.spawn(spec).await
    }

    async fn set_config(
        &self,
        session: &SessionId,
        option: &str,
        value: &str,
    ) -> Result<(), PortError> {
        self.inner.set_config(session, option, value).await
    }

    async fn send_prompt(
        &self,
        session: &SessionId,
        prompt: PromptTurn,
    ) -> Result<PromptHandle, PortError> {
        self.inner.send_prompt(session, prompt).await
    }

    async fn cancel_prompt(&self, session: &SessionId, prompt: &PromptId) -> Result<(), PortError> {
        self.inner.cancel_prompt(session, prompt).await
    }

    async fn attach_terminal(&self, session: &SessionId) -> Result<Option<TerminalId>, PortError> {
        self.inner.attach_terminal(session).await
    }

    async fn send_terminal_input(
        &self,
        terminal: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), PortError> {
        self.inner.send_terminal_input(terminal, input).await
    }

    async fn resolve_permission(
        &self,
        session: &SessionId,
        permission_id: &str,
        approved: bool,
    ) -> Result<(), PortError> {
        self.inner
            .resolve_permission(session, permission_id, approved)
            .await
    }

    async fn kill_session(&self, session: &SessionId) -> Result<(), PortError> {
        let should_fail = {
            let config = self.kill_failures.read();
            config.fail_all || config.failed_sessions.contains(session)
        };
        if should_fail {
            return Err(PortError::Agent(INJECTED_KILL_FAILURE.into()));
        }
        self.inner.kill_session(session).await
    }

    async fn health_check(&self, session: &SessionId) -> Result<HealthStatus, PortError> {
        self.inner.health_check(session).await
    }

    async fn capabilities(&self, session: &SessionId) -> Result<AgentCapabilities, PortError> {
        self.inner.capabilities(session).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, PortError> {
        self.inner.list_sessions().await
    }
}
