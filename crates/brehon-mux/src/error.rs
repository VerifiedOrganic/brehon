//! Error types for brehon-mux

use std::path::PathBuf;
use thiserror::Error;

/// Result type alias
pub type Result<T> = std::result::Result<T, Error>;

/// Multiplexer errors
#[derive(Error, Debug)]
pub enum Error {
    /// PTY operation failed
    #[error("PTY error: {0}")]
    Pty(String),

    /// Pane not found
    #[error("Pane not found: {0}")]
    PaneNotFound(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Terminal error
    #[error("Terminal error: {0}")]
    Terminal(String),

    /// Channel send error
    #[error("Channel closed")]
    ChannelClosed,

    /// Recording error
    #[error("Recording error: {0}")]
    Recording(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Inbox file contained malformed JSON
    #[error("Corrupt inbox file at {path}: {reason}")]
    CorruptInbox { path: PathBuf, reason: String },
}

impl From<brehon_pty::Error> for Error {
    fn from(err: brehon_pty::Error) -> Self {
        Self::Pty(err.to_string())
    }
}

impl Error {
    /// Create a PTY error with the given message.
    pub fn pty(msg: impl Into<String>) -> Self {
        Self::Pty(msg.into())
    }

    /// Create a terminal error with the given message.
    pub fn terminal(msg: impl Into<String>) -> Self {
        Self::Terminal(msg.into())
    }

    /// Create a pane-not-found error for the given pane ID.
    pub fn pane_not_found(id: impl Into<String>) -> Self {
        Self::PaneNotFound(id.into())
    }

    /// Create a recording error with the given message.
    pub fn recording(msg: impl Into<String>) -> Self {
        Self::Recording(msg.into())
    }

    /// Create a corrupt-inbox error for the given path and reason.
    pub fn corrupt_inbox(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self::CorruptInbox {
            path: path.into(),
            reason: reason.into(),
        }
    }
}
