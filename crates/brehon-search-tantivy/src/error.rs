//! Error types for the Tantivy search index.

use thiserror::Error;

/// Errors that can occur when interacting with the Tantivy search index.
#[derive(Debug, Error)]
pub enum TantivyError {
    /// Filesystem I/O error (e.g. opening or creating the index directory).
    #[error("IO error: {0}")]
    Io(String),

    /// Index-level error (e.g. creating a writer, committing documents).
    #[error("Index error: {0}")]
    Index(String),

    /// Query parsing or execution error.
    #[error("Query error: {0}")]
    Query(String),

    /// Schema construction error.
    #[error("Schema error: {0}")]
    Schema(String),
}
