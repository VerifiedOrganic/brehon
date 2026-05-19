//! Terminal support.
//!
//! Handles terminal attachment and input forwarding when ACP supports it,
//! plus transcript fallback for non-terminal sessions.

use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, warn};

use brehon_types::{SessionId, TerminalId};

use super::peer::AcpPeer;

#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub(crate) terminal_id: TerminalId,
    pub(crate) session_id: SessionId,
    #[allow(dead_code)]
    pub(crate) cols: u16,
    #[allow(dead_code)]
    pub(crate) rows: u16,
    #[allow(dead_code)]
    pub(crate) attached: bool,
}

pub struct TerminalManager {
    terminals: Arc<Mutex<Vec<TerminalSession>>>,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            terminals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn attach(
        &self,
        process: &mut dyn AcpPeer,
        session_id: &SessionId,
        cols: u16,
        rows: u16,
    ) -> Result<Option<TerminalId>, TerminalError> {
        let _terminal_id = TerminalId::new(format!("term-{}", uuid::Uuid::new_v4()));

        let request =
            super::acp_types::create_terminal_attach_request(session_id.as_str(), cols, rows);

        let line = super::protocol::serialize_request(&request)
            .map_err(|e| TerminalError::Protocol(e.message))?;

        process
            .send_line(&line)
            .await
            .map_err(|e| TerminalError::Io(e.to_string()))?;

        let response = wait_for_terminal_response(process, request.id.clone()).await?;

        match response.error {
            Some(err) => {
                warn!(error = ?err, "Terminal attach failed");

                // If the error indicates terminals are not supported, return None
                if err.code == -32601 || err.message.contains("not supported") {
                    return Ok(None);
                }

                Err(TerminalError::AttachFailed(err.message))
            }
            None => {
                let result = parse_terminal_attach_result(&response)?;

                let terminal = TerminalSession {
                    terminal_id: TerminalId::new(&result.terminal_id),
                    session_id: session_id.clone(),
                    cols,
                    rows,
                    attached: true,
                };

                self.terminals.lock().await.push(terminal);
                debug!(terminal_id = %result.terminal_id, "Terminal attached");

                Ok(Some(TerminalId::new(&result.terminal_id)))
            }
        }
    }

    pub(crate) async fn send_input(
        &self,
        process: &mut dyn AcpPeer,
        terminal_id: &TerminalId,
        input: Vec<u8>,
    ) -> Result<(), TerminalError> {
        let request = super::acp_types::create_terminal_input_request(terminal_id.as_str(), &input);

        let line = super::protocol::serialize_request(&request)
            .map_err(|e| TerminalError::Protocol(e.message))?;

        process
            .send_line(&line)
            .await
            .map_err(|e| TerminalError::Io(e.to_string()))?;

        debug!(terminal_id = %terminal_id, len = input.len(), "Sent terminal input");
        Ok(())
    }

    pub async fn register_attached_terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) {
        let terminal = TerminalSession {
            terminal_id: TerminalId::new(terminal_id),
            session_id: session_id.clone(),
            cols,
            rows,
            attached: true,
        };

        self.terminals.lock().await.push(terminal);
        debug!(terminal_id, session_id = %session_id, "Registered attached terminal");
    }

    pub async fn detach(&self, terminal_id: &TerminalId) -> Result<(), TerminalError> {
        let mut terminals = self.terminals.lock().await;

        if let Some(pos) = terminals.iter().position(|t| t.terminal_id == *terminal_id) {
            let terminal = terminals.remove(pos);
            debug!(terminal_id = %terminal_id, session_id = %terminal.session_id, "Terminal detached");
        }

        Ok(())
    }

    pub async fn get_terminal(&self, terminal_id: &TerminalId) -> Option<TerminalSession> {
        let terminals = self.terminals.lock().await;
        terminals
            .iter()
            .find(|t| t.terminal_id == *terminal_id)
            .cloned()
    }

    pub async fn list_terminals(&self) -> Vec<TerminalSession> {
        self.terminals.lock().await.clone()
    }
}

#[allow(dead_code)]
async fn wait_for_terminal_response(
    process: &mut dyn AcpPeer,
    expected_id: String,
) -> Result<super::protocol::JsonRpcResponse, TerminalError> {
    loop {
        match process.recv_line(30000).await {
            Ok(Some(line)) => {
                if line.is_empty() {
                    continue;
                }

                match super::protocol::parse_message(&line) {
                    Ok(super::protocol::JsonRpcMessage::Response(response)) => {
                        if response.id == expected_id {
                            return Ok(response);
                        }
                        // Wrong response ID, keep waiting
                        continue;
                    }
                    Ok(super::protocol::JsonRpcMessage::Notification(_)) => {
                        // Ignore notifications
                        continue;
                    }
                    Ok(super::protocol::JsonRpcMessage::Request(_)) => {
                        // Ignore requests (shouldn't happen during terminal attach)
                        continue;
                    }
                    Err(e) => {
                        return Err(TerminalError::Protocol(format!(
                            "Failed to parse response: {}",
                            e.message
                        )));
                    }
                }
            }
            Ok(None) => {
                return Err(TerminalError::ProcessDied);
            }
            Err(_) => {
                return Err(TerminalError::Timeout);
            }
        }
    }
}

#[allow(dead_code)]
fn parse_terminal_attach_result(
    response: &super::protocol::JsonRpcResponse,
) -> Result<super::acp_types::TerminalAttachResult, TerminalError> {
    match &response.result {
        Some(result) => serde_json::from_value(result.clone())
            .map_err(|e| TerminalError::Protocol(format!("Failed to parse result: {}", e))),
        None => Err(TerminalError::Protocol("No result in response".into())),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("Terminal attach failed: {0}")]
    AttachFailed(String),
    #[error("Terminal not found: {0}")]
    NotFound(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("Process died")]
    ProcessDied,
    #[error("Timeout")]
    Timeout,
    #[error("Terminal not supported by agent")]
    NotSupported,
}

#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub is_stdout: bool,
}

pub struct TranscriptBuffer {
    lines: Arc<Mutex<Vec<TranscriptLine>>>,
    max_lines: usize,
}

impl TranscriptBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(Vec::new())),
            max_lines,
        }
    }

    pub async fn append(&self, content: String, is_stdout: bool) {
        let line = TranscriptLine {
            content,
            timestamp: chrono::Utc::now(),
            is_stdout,
        };

        let mut lines = self.lines.lock().await;
        lines.push(line);

        if lines.len() > self.max_lines {
            lines.remove(0);
        }
    }

    #[allow(dead_code)]
    pub async fn get_lines(&self) -> Vec<TranscriptLine> {
        self.lines.lock().await.clone()
    }

    pub async fn get_recent(&self, count: usize) -> Vec<TranscriptLine> {
        let lines = self.lines.lock().await;
        let start = lines.len().saturating_sub(count);
        lines[start..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_id() {
        let id = TerminalId::new("test-123");
        assert_eq!(id.as_str(), "test-123");
    }

    #[tokio::test]
    async fn test_transcript_buffer() {
        let buffer = TranscriptBuffer::new(100);

        buffer.append("line 1".to_string(), true).await;
        buffer.append("line 2".to_string(), false).await;

        let lines = buffer.get_lines().await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "line 1");
        assert!(lines[0].is_stdout);
        assert_eq!(lines[1].content, "line 2");
        assert!(!lines[1].is_stdout);
    }

    #[tokio::test]
    async fn test_transcript_buffer_max() {
        let buffer = TranscriptBuffer::new(3);

        buffer.append("line 1".to_string(), true).await;
        buffer.append("line 2".to_string(), true).await;
        buffer.append("line 3".to_string(), true).await;
        buffer.append("line 4".to_string(), true).await;

        let lines = buffer.get_lines().await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content, "line 2");
        assert_eq!(lines[1].content, "line 3");
        assert_eq!(lines[2].content, "line 4");
    }

    #[tokio::test]
    async fn test_transcript_recent() {
        let buffer = TranscriptBuffer::new(100);

        for i in 0..10 {
            buffer.append(format!("line {}", i), true).await;
        }

        let recent = buffer.get_recent(3).await;
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].content, "line 7");
        assert_eq!(recent[1].content, "line 8");
        assert_eq!(recent[2].content, "line 9");
    }

    #[tokio::test]
    async fn test_terminal_manager() {
        let manager = TerminalManager::new();
        let terminals = manager.list_terminals().await;
        assert!(terminals.is_empty());
    }
}
