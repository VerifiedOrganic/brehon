//! Session lifecycle management.
//!
//! Handles ACP session initialization, capability negotiation, and teardown.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, error, info};

use brehon_types::AgentCapabilities;

use super::acp_types::SessionMetadata;
use super::process::AgentProcess;

/// Timeout configuration for ACP session initialization and I/O.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Maximum time to wait for the agent handshake to complete.
    pub init_timeout: Duration,
    /// Maximum time to wait for a single I/O operation.
    pub io_timeout: Duration,
    /// Maximum time to wait for a mediated permission decision.
    pub permission_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            init_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(60),
            permission_timeout: Duration::from_secs(30),
        }
    }
}

/// Lifecycle state of an ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Handshake in progress.
    Initializing,
    /// Session is active and accepting prompts.
    Running,
    /// Graceful shutdown has been requested.
    ShuttingDown,
    /// Session has terminated.
    Dead,
}

/// Manages the full lifecycle of an ACP session: spawn, handshake, health-check, and shutdown.
pub struct SessionLifecycle {
    process: Arc<Mutex<Option<AgentProcess>>>,
    state: Arc<Mutex<SessionState>>,
    capabilities: Arc<Mutex<Option<AgentCapabilities>>>,
    config: SessionConfig,
}

impl SessionLifecycle {
    /// Creates a new lifecycle manager with the given configuration.
    pub fn new(config: SessionConfig) -> Self {
        Self {
            process: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(SessionState::Initializing)),
            capabilities: Arc::new(Mutex::new(None)),
            config,
        }
    }

    /// Spawns the agent subprocess, performs the ACP handshake, and returns negotiated capabilities.
    pub async fn initialize(
        &mut self,
        command: &str,
        args: &[String],
        cwd: &str,
        metadata: Option<SessionMetadata>,
    ) -> Result<AgentCapabilities, LifecycleError> {
        debug!(command, ?args, cwd, "Starting session lifecycle");

        let mut process = AgentProcess::spawn(command, args, cwd)
            .await
            .map_err(|e| LifecycleError::SpawnFailed(e.to_string()))?;

        let mut state = self.state.lock().await;
        *state = SessionState::Initializing;
        drop(state);

        let caps = self.do_handshake(&mut process, metadata).await?;

        let mut caps_guard = self.capabilities.lock().await;
        *caps_guard = Some(caps.clone());
        drop(caps_guard);

        let mut state = self.state.lock().await;
        *state = SessionState::Running;
        drop(state);

        let mut process_guard = self.process.lock().await;
        *process_guard = Some(process);

        debug!(capabilities = ?caps, "Session initialized successfully");
        Ok(caps)
    }

    /// Verifies that a process is attached and initialized.
    pub async fn attach_process(&self) -> Result<(), LifecycleError> {
        let process = self.process.lock().await;
        if process.is_none() {
            return Err(LifecycleError::NotInitialized);
        }
        Ok(())
    }

    async fn do_handshake(
        &self,
        process: &mut AgentProcess,
        metadata: Option<SessionMetadata>,
    ) -> Result<AgentCapabilities, LifecycleError> {
        let request = super::acp_types::create_initialize_request("", metadata);

        self.send_request(process, &request)
            .await
            .map_err(|e| LifecycleError::HandshakeFailed(e.to_string()))?;

        let response = self
            .recv_response(process, "initialize", self.config.init_timeout)
            .await
            .map_err(|e| LifecycleError::HandshakeFailed(e.to_string()))?;

        match response.error {
            Some(err) => {
                error!(error = ?err, "Initialize failed");
                Err(LifecycleError::HandshakeFailed(err.message))
            }
            None => {
                let init_result = self.parse_initialize_result(&response)?;
                let caps: AgentCapabilities = init_result.capabilities.into();
                Ok(caps)
            }
        }
    }

    fn parse_initialize_result(
        &self,
        response: &super::protocol::JsonRpcResponse,
    ) -> Result<super::acp_types::InitializeResult, LifecycleError> {
        match &response.result {
            Some(result) => serde_json::from_value(result.clone()).map_err(|e| {
                LifecycleError::ProtocolError(format!("Failed to parse initialize result: {}", e))
            }),
            None => Err(LifecycleError::ProtocolError(
                "No result in initialize response".into(),
            )),
        }
    }

    async fn send_request(
        &self,
        process: &mut AgentProcess,
        request: &super::protocol::JsonRpcRequest,
    ) -> Result<(), LifecycleError> {
        let line = super::protocol::serialize_request(request)
            .map_err(|e| LifecycleError::ProtocolError(e.message))?;

        process
            .send_line(&line)
            .await
            .map_err(|e| LifecycleError::Io(e.to_string()))?;

        debug!(id = %request.id, method = %request.method, "Sent request");
        Ok(())
    }

    async fn recv_response(
        &self,
        process: &mut AgentProcess,
        _method: &str,
        timeout: Duration,
    ) -> Result<super::protocol::JsonRpcResponse, LifecycleError> {
        loop {
            match process.recv_line(timeout.as_millis() as u64).await {
                Ok(Some(line)) => {
                    if line.is_empty() {
                        continue;
                    }

                    match super::protocol::parse_message(&line) {
                        Ok(super::protocol::JsonRpcMessage::Response(response)) => {
                            return Ok(response);
                        }
                        Ok(super::protocol::JsonRpcMessage::Notification(_)) => {
                            debug!("Received notification during handshake, continuing");
                            continue;
                        }
                        Ok(super::protocol::JsonRpcMessage::Request(_)) => {
                            debug!("Unexpected request during handshake");
                            continue;
                        }
                        Err(e) => {
                            error!(error = ?e, line = %line, "Failed to parse message");
                            return Err(LifecycleError::ProtocolError(
                                "Malformed JSON response".into(),
                            ));
                        }
                    }
                }
                Ok(None) => {
                    return Err(LifecycleError::ProcessDied);
                }
                Err(e) => {
                    return Err(LifecycleError::Timeout(e.to_string()));
                }
            }
        }
    }

    /// Sends a shutdown request to the agent and terminates the subprocess.
    pub async fn shutdown(&self, reason: Option<&str>) -> Result<(), LifecycleError> {
        let mut state = self.state.lock().await;
        if *state != SessionState::Running {
            debug!("Session not running, skipping shutdown");
            return Ok(());
        }
        *state = SessionState::ShuttingDown;
        drop(state);

        let mut process_guard = self.process.lock().await;
        if let Some(ref mut proc) = *process_guard {
            let request = super::acp_types::create_shutdown_request(reason);

            let line = super::protocol::serialize_request(&request)
                .map_err(|e| LifecycleError::ProtocolError(e.message))?;

            if let Err(e) = proc.send_line(&line).await {
                debug!(error = %e, "Failed to send shutdown, will kill process");
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let mut state = self.state.lock().await;
        *state = SessionState::Dead;

        if let Some(proc) = process_guard.take() {
            proc.kill()
                .await
                .map_err(|e| LifecycleError::ShutdownFailed(e.to_string()))?;
        }

        info!("Session shut down");
        Ok(())
    }

    /// Returns `true` if the underlying agent process is still alive.
    pub async fn health_check(&self) -> Result<bool, LifecycleError> {
        let process = self.process.lock().await;
        match process.as_ref() {
            Some(proc) => Ok(proc.is_alive()),
            None => Ok(false),
        }
    }

    /// Returns the current lifecycle state.
    pub async fn get_state(&self) -> SessionState {
        *self.state.lock().await
    }

    /// Returns the capabilities negotiated during initialization, if any.
    pub async fn get_capabilities(&self) -> Option<AgentCapabilities> {
        self.capabilities.lock().await.clone()
    }
}

/// Errors that can occur during session lifecycle management.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("Failed to spawn process: {0}")]
    SpawnFailed(String),
    #[error("Handshake failed: {0}")]
    HandshakeFailed(String),
    #[error("Protocol error: {0}")]
    ProtocolError(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Timeout: {0}")]
    Timeout(String),
    #[error("Process died")]
    ProcessDied,
    #[error("Session not initialized")]
    NotInitialized,
    #[error("Session already running")]
    AlreadyRunning,
    #[error("Shutdown failed: {0}")]
    ShutdownFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();
        assert_eq!(config.init_timeout, Duration::from_secs(30));
        assert_eq!(config.io_timeout, Duration::from_secs(60));
        assert_eq!(config.permission_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_lifecycle_error() {
        let err = LifecycleError::SpawnFailed("test".to_string());
        // The error message format is "Failed to spawn process: test"
        assert!(err.to_string().contains("spawn process"));
    }
}
