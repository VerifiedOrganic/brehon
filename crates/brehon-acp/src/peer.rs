//! ACP transport peers.
//!
//! This module keeps JSON-RPC session logic independent from the concrete
//! transport. Stdio subprocesses remain the default, while supervised agents
//! can later expose ACP over a Unix-domain sidecar socket.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::process::{AgentProcess, ProcessError};

#[derive(Debug, Error)]
pub enum AcpIoError {
    #[error("transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport timeout")]
    Timeout,
    #[error("transport closed")]
    Closed,
    #[error("process transport error: {0}")]
    Process(String),
}

impl From<ProcessError> for AcpIoError {
    fn from(value: ProcessError) -> Self {
        match value {
            ProcessError::Io(err) => Self::Io(err),
            ProcessError::Timeout => Self::Timeout,
            ProcessError::ProcessDied | ProcessError::StdinClosed => Self::Closed,
            other => Self::Process(other.to_string()),
        }
    }
}

#[async_trait]
pub(crate) trait AcpPeer: Send + Sync {
    async fn send_line(&mut self, line: &str) -> Result<(), AcpIoError>;
    async fn recv_line(&mut self, timeout_ms: u64) -> Result<Option<String>, AcpIoError>;
    async fn shutdown(&mut self) -> Result<(), AcpIoError>;
    fn is_alive(&self) -> bool;
}

pub(crate) struct SubprocessAcpPeer {
    process: AgentProcess,
}

impl SubprocessAcpPeer {
    pub(crate) fn new(process: AgentProcess) -> Self {
        Self { process }
    }
}

#[async_trait]
impl AcpPeer for SubprocessAcpPeer {
    async fn send_line(&mut self, line: &str) -> Result<(), AcpIoError> {
        self.process.send_line(line).await.map_err(Into::into)
    }

    async fn recv_line(&mut self, timeout_ms: u64) -> Result<Option<String>, AcpIoError> {
        self.process.recv_line(timeout_ms).await.map_err(Into::into)
    }

    async fn shutdown(&mut self) -> Result<(), AcpIoError> {
        self.process.kill().await.map_err(Into::into)
    }

    fn is_alive(&self) -> bool {
        self.process.is_alive()
    }
}

pub(crate) struct UnixSocketAcpPeer {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    alive: AtomicBool,
}

impl UnixSocketAcpPeer {
    pub(crate) async fn connect(path: impl AsRef<Path>) -> Result<Self, AcpIoError> {
        let stream = UnixStream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            alive: AtomicBool::new(true),
        })
    }
}

#[async_trait]
impl AcpPeer for UnixSocketAcpPeer {
    async fn send_line(&mut self, line: &str) -> Result<(), AcpIoError> {
        let line = format!("{line}\n");
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv_line(&mut self, timeout_ms: u64) -> Result<Option<String>, AcpIoError> {
        let mut line = String::new();
        match timeout(
            std::time::Duration::from_millis(timeout_ms),
            self.reader.read_line(&mut line),
        )
        .await
        {
            Ok(Ok(0)) => {
                self.alive.store(false, Ordering::SeqCst);
                Ok(None)
            }
            Ok(Ok(_)) => {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                Ok(Some(trimmed.to_string()))
            }
            Ok(Err(err)) => {
                self.alive.store(false, Ordering::SeqCst);
                Err(AcpIoError::Io(err))
            }
            Err(_) => Err(AcpIoError::Timeout),
        }
    }

    async fn shutdown(&mut self) -> Result<(), AcpIoError> {
        self.alive.store(false, Ordering::SeqCst);
        self.writer.shutdown().await?;
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}
