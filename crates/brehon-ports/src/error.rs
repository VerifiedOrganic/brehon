//! Error types for port traits.

use thiserror::Error;

/// Error type for all port operations.
#[derive(Debug, Error)]
pub enum PortError {
    /// Storage backend error.
    #[error("Storage error: {0}")]
    Storage(String),

    /// Agent communication error.
    #[error("Agent error: {0}")]
    Agent(String),

    /// Git operation error.
    #[error("Git error: {0}")]
    Git(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Notification error.
    #[error("Notification error: {0}")]
    Notification(String),

    /// Runtime side-channel or command-routing error.
    #[error("Runtime error: {0}")]
    Runtime(String),

    /// Policy evaluation error.
    #[error("Policy error: {0}")]
    Policy(String),

    /// Semantic detection error.
    #[error("Detection error: {0}")]
    Detection(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    IO(String),
}

impl From<std::io::Error> for PortError {
    fn from(err: std::io::Error) -> Self {
        PortError::IO(err.to_string())
    }
}
