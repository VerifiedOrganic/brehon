//! SearchIndex trait for memory/rule/skill search.

use async_trait::async_trait;

use brehon_types::{SearchEntry, SearchResult};

use crate::PortError;

/// Trait for indexing and searching memories, rules, and skills.
///
/// This trait abstracts the search backend (e.g., Tantivy) so that
/// different implementations can be plugged in.
///
/// Implementations should:
/// - Persist indexed data to durable storage
/// - Support concurrent read operations
/// - Handle concurrent index and search without deadlock
#[async_trait]
pub trait SearchIndex: Send + Sync {
    /// Index a new entry.
    ///
    /// The entry is added to the search index and becomes searchable
    /// immediately.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if indexing fails.
    async fn index(&self, entry: SearchEntry) -> Result<(), PortError>;

    /// Search for entries matching a query.
    ///
    /// Returns up to `limit` results ranked by relevance (BM25 or similar).
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if the search fails.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, PortError>;

    /// Delete an entry by ID.
    ///
    /// Removes the entry from the index. Searches will no longer return
    /// this entry.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if deletion fails.
    async fn delete(&self, id: &str) -> Result<(), PortError>;

    /// Rebuild the entire index from authoritative data.
    ///
    /// Clears the index and repopulates it from the provided entries,
    /// which should be the complete set of searchable items sourced
    /// from the authoritative durable store (e.g., memories.json or
    /// event replay). This ensures the search index is consistent
    /// with the source of truth after corruption or schema changes.
    ///
    /// # Errors
    ///
    /// Returns `PortError::Storage` if reindexing fails.
    async fn reindex(&self, entries: Vec<SearchEntry>) -> Result<(), PortError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_error_storage_variant() {
        let e = PortError::Storage("index corrupted".into());
        assert!(matches!(e, PortError::Storage(_)));
    }
}
