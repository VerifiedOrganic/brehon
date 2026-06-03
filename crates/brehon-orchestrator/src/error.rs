//! Error types for the orchestrator.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("Dependency cycle detected: {0}")]
    CycleError(String),

    #[error("Invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("Assignment failed: {0}")]
    AssignmentError(String),

    #[error("Worker pool error: {0}")]
    WorkerPoolError(String),

    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Worker not found: {0}")]
    WorkerNotFound(String),

    #[error("Dependency not found: {0}")]
    DependencyNotFound(String),

    #[error("Git operations unavailable: {0}")]
    GitUnavailable(String),

    #[error("No available workers")]
    NoAvailableWorkers,

    #[error("Port error: {0}")]
    PortError(String),

    #[error("Storage error: {0}")]
    StorageError(String),
}

impl From<brehon_ports::PortError> for OrchestratorError {
    fn from(err: brehon_ports::PortError) -> Self {
        match err {
            brehon_ports::PortError::Storage(msg) => OrchestratorError::StorageError(msg),
            brehon_ports::PortError::Agent(msg) => OrchestratorError::PortError(msg),
            brehon_ports::PortError::Git(msg) => OrchestratorError::GitUnavailable(msg),
            _ => OrchestratorError::PortError(err.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, OrchestratorError>;
