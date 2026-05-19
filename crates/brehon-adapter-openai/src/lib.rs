//! OpenAI-compatible HTTP adapter for Brehon.
//!
//! Implements the [`AgentAdapter`] trait for direct OpenAI-compatible
//! chat-completions endpoints.

pub mod openai_compatible;
mod stability;

use std::sync::Arc;

use brehon_adapter_sdk::{
    AdapterError, AdapterErrorKind, AdapterEvent, AdapterResult, AgentAdapter, PromptResult,
};
use brehon_types::{
    AdapterKind, AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId,
    SessionInfo, SessionSpec, TerminalId, ToolCallStreaming,
};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::warn;

use crate::openai_compatible::OpenAiCompatibleSession;

/// Configuration for an OpenAI-compatible adapter instance.
#[derive(Clone, Default)]
pub struct OpenAiCompatibleConfig {
    /// Base URL for the OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Environment variable containing the API key.
    pub api_key_env: Option<String>,
    /// Extra headers to send with every request.
    pub extra_headers: Vec<(String, String)>,
    /// Default model to use.
    pub model: Option<String>,
    /// Tool-name prefix for Brehon coordination tools.
    pub tool_prefix: Option<String>,
    /// Optional direct tool bridge.
    pub tool_bridge: Option<Arc<dyn brehon_adapter_sdk::direct_tools::DirectToolBridge>>,
}

/// Adapter that implements [`AgentAdapter`] for OpenAI-compatible endpoints.
pub struct OpenAiCompatibleAdapter {
    config: OpenAiCompatibleConfig,
    session: Mutex<Option<OpenAiCompatibleSession>>,
    event_broadcast: std::sync::Mutex<Option<tokio::sync::broadcast::Sender<AdapterEvent>>>,
}

impl OpenAiCompatibleAdapter {
    /// Create a new adapter with the given configuration.
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self {
            config,
            session: Mutex::new(None),
            event_broadcast: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl AgentAdapter for OpenAiCompatibleAdapter {
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId> {
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(64);
        let (mpsc_tx, mut mpsc_rx) = mpsc::channel(64);

        // Bridge: forward AdapterEvents from the mpsc channel into the broadcast.
        let broadcast_tx_clone = broadcast_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = mpsc_rx.recv().await {
                let _ = broadcast_tx_clone.send(event);
            }
        });

        let session = OpenAiCompatibleSession::spawn(
            spec,
            self.config.base_url.clone(),
            self.config.api_key_env.clone(),
            self.config.extra_headers.clone(),
            self.config.model.clone(),
            self.config.tool_prefix.clone(),
            self.config.tool_bridge.clone(),
            Some(mpsc_tx),
        )
        .await
        .map_err(|e| AdapterError::spawn_failed(e.to_string()))?;

        let session_id = session.session_id().clone();
        *self.session.lock().await = Some(session);
        *self.event_broadcast.lock().unwrap() = Some(broadcast_tx);
        Ok(session_id)
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::send_failed("session not spawned"))?
            .clone();
        session
            .send_prompt(prompt)
            .await
            .map_err(|e| AdapterError::send_failed(e.to_string()))
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::timed_out("session not spawned"))?
            .clone();
        session
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(|e| AdapterError::timed_out(e.to_string()))
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let broadcast = self.event_broadcast.lock().unwrap();
        let (mpsc_tx, mpsc_rx) = mpsc::channel(64);
        if let Some(broadcast) = broadcast.as_ref() {
            let mut broadcast_rx = broadcast.subscribe();
            tokio::spawn(async move {
                loop {
                    match broadcast_rx.recv().await {
                        Ok(event) => {
                            if mpsc_tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("Broadcast receiver lagged by {} messages", skipped);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        mpsc_rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        let session = self.session.lock().await.clone();
        if let Some(session) = session {
            session
                .kill()
                .await
                .map_err(|e| AdapterError::new(AdapterErrorKind::TransportClosed, e.to_string()))?;
        }
        Ok(())
    }

    fn kind(&self) -> AdapterKind {
        AdapterKind::OpenAiCompatible
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::spawn_failed("session not spawned"))?
            .capabilities();
        Ok(session)
    }

    async fn session_id(&self) -> SessionId {
        let session = self.session.lock().await;
        session
            .as_ref()
            .map(|s| s.session_id().clone())
            .unwrap_or_else(|| SessionId::new("openai-unknown"))
    }

    async fn session_info(&self) -> SessionInfo {
        let session = self.session.lock().await;
        session
            .as_ref()
            .map(|s| s.session_info())
            .unwrap_or_else(|| SessionInfo {
                session_id: SessionId::new("openai-unknown"),
                agent_id: brehon_types::AgentId::new("openai"),
                role: "worker".to_string(),
                health: HealthStatus::Unknown,
                created_at: chrono::Utc::now(),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec![],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: ToolCallStreaming::None,
                },
            })
    }

    async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        let session = self.session.lock().await.clone();
        if let Some(session) = session {
            session.stability_counters().await
        } else {
            brehon_types::StabilityCounters::default()
        }
    }

    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::send_failed("session not spawned"))?
            .clone();
        session
            .set_config(option, value)
            .await
            .map_err(|e| AdapterError::send_failed(e.to_string()))?;
        Ok(())
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::send_failed("session not spawned"))?
            .clone();
        session
            .cancel_prompt(prompt)
            .await
            .map_err(|e| AdapterError::send_failed(e.to_string()))?;
        Ok(())
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        let session = self
            .session
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| AdapterError::send_failed("session not spawned"))?
            .clone();
        session
            .health_check()
            .await
            .map_err(|e| AdapterError::send_failed(e.to_string()))
    }

    async fn attach_terminal(&self, _cols: u16, _rows: u16) -> AdapterResult<Option<TerminalId>> {
        Ok(None)
    }

    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        _input: Vec<u8>,
    ) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Terminal input is not supported for OpenAI-compatible sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for OpenAI-compatible sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_kind_is_openai_compatible() {
        let adapter = OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig::default());
        assert_eq!(adapter.kind(), AdapterKind::OpenAiCompatible);
    }
}
