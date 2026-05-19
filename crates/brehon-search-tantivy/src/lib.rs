//! Tantivy-based implementation of the SearchIndex trait.
//!
//! This crate provides a full-text search implementation using Tantivy,
//! a full-text search engine library in Rust. It supports BM25 ranking,
//! tag filtering, and persistent storage.

mod error;
mod indexing;
mod queries;
mod schema;

pub use error::TantivyError;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tantivy::directory::MmapDirectory;
use tantivy::Index as TantivyIndex;
use tracing::{debug, info};

use brehon_ports::{PortError, SearchIndex};
use brehon_types::{SearchEntry, SearchResult};

const INDEX_DIR_NAME: &str = "index";

/// Full-text search index backed by Tantivy with BM25 ranking.
///
/// Supports persistent (disk-backed) and in-memory modes, tag filtering,
/// and concurrent read/write access.
pub struct TantivySearchIndex {
    index: TantivyIndex,
    schema: Arc<schema::SearchSchema>,
}

impl TantivySearchIndex {
    /// Open an existing index at `path` or create a new one if it does not exist.
    pub async fn new(path: &Path) -> Result<Self, TantivyError> {
        let schema = Arc::new(schema::SearchSchema::new());

        let index_path = path.join(INDEX_DIR_NAME);

        if index_path.exists() {
            debug!("Opening existing index at {:?}", index_path);
            let directory = MmapDirectory::open(&index_path)
                .map_err(|e| TantivyError::Io(format!("Failed to open directory: {}", e)))?;
            let index = TantivyIndex::open_or_create(directory, schema.schema.clone())
                .map_err(|e| TantivyError::Index(format!("Failed to open index: {}", e)))?;

            Ok(Self { index, schema })
        } else {
            info!("Creating new index at {:?}", index_path);
            std::fs::create_dir_all(&index_path)
                .map_err(|e| TantivyError::Io(format!("Failed to create directory: {}", e)))?;

            let directory = MmapDirectory::open(&index_path)
                .map_err(|e| TantivyError::Io(format!("Failed to open directory: {}", e)))?;
            let index = TantivyIndex::create(
                directory,
                schema.schema.clone(),
                tantivy::IndexSettings::default(),
            )
            .map_err(|e| TantivyError::Index(format!("Failed to create index: {}", e)))?;

            Ok(Self { index, schema })
        }
    }

    /// Create a new in-memory index (useful for testing).
    pub fn create_in_memory() -> Result<Self, TantivyError> {
        let schema = Arc::new(schema::SearchSchema::new());
        let index = TantivyIndex::create_in_ram(schema.schema.clone());

        Ok(Self { index, schema })
    }

    /// Open an existing index at `path`, returning an error if it does not exist.
    pub fn load_existing(path: &Path) -> Result<Self, TantivyError> {
        let schema = Arc::new(schema::SearchSchema::new());
        let index_path = path.join(INDEX_DIR_NAME);

        if !index_path.exists() {
            return Err(TantivyError::Io(format!(
                "Index directory does not exist: {:?}",
                index_path
            )));
        }

        let directory = MmapDirectory::open(&index_path)
            .map_err(|e| TantivyError::Io(format!("Failed to open directory: {}", e)))?;
        let index = TantivyIndex::open(directory)
            .map_err(|e| TantivyError::Index(format!("Failed to open index: {}", e)))?;

        Ok(Self { index, schema })
    }
}

#[async_trait]
impl SearchIndex for TantivySearchIndex {
    async fn index(&self, entry: SearchEntry) -> Result<(), PortError> {
        indexing::index_entry(&self.index, &self.schema, entry)
            .map_err(|e| PortError::Storage(e.to_string()))
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, PortError> {
        queries::search(&self.index, &self.schema, query, limit)
            .map_err(|e| PortError::Storage(e.to_string()))
    }

    async fn delete(&self, id: &str) -> Result<(), PortError> {
        indexing::delete_entry(&self.index, &self.schema, id)
            .map_err(|e| PortError::Storage(e.to_string()))
    }

    async fn reindex(&self, entries: Vec<SearchEntry>) -> Result<(), PortError> {
        indexing::clear_and_index_batch(&self.index, &self.schema, entries)
            .map_err(|e| PortError::Storage(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn create_test_entry(id: &str, content: &str, tags: &[&str], source: &str) -> SearchEntry {
        SearchEntry {
            id: id.to_string(),
            content: content.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            source: source.to_string(),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_basic_index_and_search() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        let entry = create_test_entry(
            "mem-1",
            "This project uses bcrypt for authentication and password hashing",
            &["auth", "security"],
            "claude-code",
        );
        index.index(entry).await.unwrap();

        let results = index.search("bcrypt", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "mem-1");
    }

    #[tokio::test]
    async fn test_multiple_entries_ranked() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        index
            .index(create_test_entry(
                "mem-1",
                "Python is a great programming language",
                &["programming"],
                "agent-1",
            ))
            .await
            .unwrap();

        index
            .index(create_test_entry(
                "mem-2",
                "Rust is a systems programming language focused on safety",
                &["programming", "rust"],
                "agent-2",
            ))
            .await
            .unwrap();

        index
            .index(create_test_entry(
                "mem-3",
                "JavaScript is used for web programming",
                &["programming", "web"],
                "agent-3",
            ))
            .await
            .unwrap();

        let results = index.search("programming language", 10).await.unwrap();
        assert!(results.len() >= 2);

        assert!(results.iter().any(|r| r.id == "mem-1"));
        assert!(results.iter().any(|r| r.id == "mem-2"));
    }

    #[tokio::test]
    async fn test_delete() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        index
            .index(create_test_entry(
                "mem-1",
                "Test content to delete",
                &[],
                "agent",
            ))
            .await
            .unwrap();

        let results = index.search("Test", 10).await.unwrap();
        assert_eq!(results.len(), 1);

        index.delete("mem-1").await.unwrap();

        let results = index.search("Test", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_reindex_repopulates_from_entries() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        index
            .index(create_test_entry("mem-1", "Alpha data", &[], "agent"))
            .await
            .unwrap();

        index
            .index(create_test_entry("mem-2", "Beta data", &[], "agent"))
            .await
            .unwrap();

        let results = index.search("Alpha", 10).await.unwrap();
        assert_eq!(results.len(), 1);

        let replacement_entries = vec![create_test_entry(
            "mem-3",
            "Gamma rebuilt from authoritative source",
            &["fresh"],
            "source-agent",
        )];
        index.reindex(replacement_entries).await.unwrap();

        let old_results = index.search("Alpha", 10).await.unwrap();
        assert!(
            old_results.is_empty(),
            "old entries should be gone after reindex"
        );

        let new_results = index.search("Gamma", 10).await.unwrap();
        assert_eq!(new_results.len(), 1);
        assert_eq!(new_results[0].id, "mem-3");
    }

    #[tokio::test]
    async fn test_reindex_with_empty_entries_clears_index() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        index
            .index(create_test_entry("mem-1", "Alpha data", &[], "agent"))
            .await
            .unwrap();

        let results = index.search("Alpha", 10).await.unwrap();
        assert_eq!(results.len(), 1);

        index.reindex(vec![]).await.unwrap();

        let results = index.search("Alpha", 10).await.unwrap();
        assert!(
            results.is_empty(),
            "old entries should be gone after reindex with empty entries"
        );
    }

    #[tokio::test]
    async fn test_100_memories_search() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        for i in 0..100 {
            let content = format!(
                "Memory number {} about {} in {}",
                i,
                if i % 3 == 0 {
                    "authentication"
                } else {
                    "database"
                },
                if i % 2 == 0 { "Rust" } else { "Python" }
            );
            let tags = if i % 3 == 0 {
                vec!["auth".to_string()]
            } else {
                vec!["database".to_string()]
            };

            index
                .index(SearchEntry {
                    id: format!("mem-{}", i),
                    content,
                    tags,
                    source: format!("agent-{}", i % 5),
                    timestamp: Utc::now(),
                })
                .await
                .unwrap();
        }

        let results = index.search("authentication", 20).await.unwrap();
        assert!(!results.is_empty(), "Should find authentication results");

        let results = index.search("Rust programming", 10).await.unwrap();
        assert!(!results.is_empty(), "Should find Rust results");
    }

    #[tokio::test]
    async fn test_persistent_index() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path();

        {
            let index = TantivySearchIndex::new(path).await.unwrap();

            index
                .index(create_test_entry(
                    "mem-1",
                    "Persistent content for testing",
                    &["test"],
                    "agent-1",
                ))
                .await
                .unwrap();

            index
                .index(create_test_entry(
                    "mem-2",
                    "Another persistent entry",
                    &[],
                    "agent-2",
                ))
                .await
                .unwrap();
        }

        {
            let index = TantivySearchIndex::load_existing(path).unwrap();

            let results = index.search("Persistent", 10).await.unwrap();
            assert!(!results.is_empty(), "Should find results after reload");

            let results = index.search("another", 10).await.unwrap();
            assert!(!results.is_empty(), "Should find second entry after reload");
        }
    }

    #[tokio::test]
    async fn test_tag_filtering() {
        let index = TantivySearchIndex::create_in_memory().unwrap();

        index
            .index(create_test_entry(
                "mem-1",
                "Authentication system with bcrypt",
                &["auth", "security"],
                "agent-1",
            ))
            .await
            .unwrap();

        index
            .index(create_test_entry(
                "mem-2",
                "Database connection pooling",
                &["database", "performance"],
                "agent-2",
            ))
            .await
            .unwrap();

        index
            .index(create_test_entry(
                "mem-3",
                "Security best practices",
                &["security", "guidelines"],
                "agent-3",
            ))
            .await
            .unwrap();

        let results = queries::search_by_tag(&index.index, &index.schema, "security", 10).unwrap();
        assert!(
            !results.is_empty(),
            "Should find entries tagged with 'security'"
        );

        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"mem-1") || ids.contains(&"mem-3"),
            "Should find mem-1 or mem-3 with security tag"
        );
    }

    #[tokio::test]
    async fn test_concurrent_index_and_search() {
        let index = std::sync::Arc::new(TantivySearchIndex::create_in_memory().unwrap());

        let mut handles = vec![];

        for i in 0..10 {
            let idx = index.clone();
            let handle = tokio::spawn(async move {
                for j in 0..10 {
                    let entry = SearchEntry {
                        id: format!("mem-{}-{}", i, j),
                        content: format!("Content {} from thread {}", j, i),
                        tags: vec![format!("tag-{}", i)],
                        source: format!("agent-{}", i),
                        timestamp: Utc::now(),
                    };
                    idx.index(entry).await.unwrap();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let results = index.search("Content", 100).await.unwrap();
        assert_eq!(results.len(), 100, "All 100 entries should be indexed");
    }
}
