use brehon_ports::PortError;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Storage error: {0}")]
    Storage(String),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Claim not found: {0}")]
    ClaimNotFound(String),
    #[error("Claim expired: {0}")]
    ClaimExpired(String),
    #[error("Version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: u64, actual: u64 },
}

impl From<StoreError> for PortError {
    fn from(err: StoreError) -> Self {
        PortError::Storage(err.to_string())
    }
}

impl From<fjall::Error> for StoreError {
    fn from(err: fjall::Error) -> Self {
        StoreError::Storage(err.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        StoreError::Serialization(err.to_string())
    }
}
