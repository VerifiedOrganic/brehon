//! Configuration error types.

use thiserror::Error;

/// Errors that can occur during configuration loading and validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to parse configuration file.
    #[error("Parse error: {0}")]
    Parse(String),

    /// IO error reading or writing config files.
    #[error("IO error: {0}")]
    Io(String),

    /// Configuration validation failed.
    #[error("Validation error: {0}")]
    Validation(String),

    /// Project already initialized.
    #[error("Project already initialized")]
    AlreadyInitialized,

    /// Missing required configuration.
    #[error("Missing configuration: {0}")]
    Missing(String),
}
