//! In-memory SearchIndex implementation for testing.
//!
//! Simple linear scan implementation with tag filtering.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use brehon_ports::{PortError, SearchIndex};
use brehon_types::{SearchEntry, SearchResult};

/// In-memory search index using linear scan.
#[derive(Debug, Clone)]
pub struct InMemorySearchIndex {
    inner: Arc<RwLock<IndexInner>>,
}

#[derive(Debug, Default)]
struct IndexInner {
    entries: HashMap<String, SearchEntry>,
}

impl InMemorySearchIndex {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(IndexInner::default())),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().entries.is_empty()
    }
}

impl Default for InMemorySearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchIndex for InMemorySearchIndex {
    async fn index(&self, entry: SearchEntry) -> Result<(), PortError> {
        self.inner
            .write()
            .entries
            .insert(entry.id.clone(), entry);
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, PortError> {
        let inner = self.inner.read();
        let query_lower = query.to_lowercase();

        let mut results: Vec<(SearchEntry, f32)> = inner
            .entries
            .values()
            .filter_map(|entry| {
                let content_lower = entry.content.to_lowercase();

                let score = if content_lower.contains(&query_lower) {
                    let occurrences = content_lower.matches(&query_lower).count() as f32;
                    let base_score = 1.0 / (1.0 + entry.id.len() as f32);
                    base_score * (1.0 + occurrences * 0.1)
                } else {
                    for tag in &entry.tags {
                        if tag.to_lowercase().contains(&query_lower) {
                            return Some((entry.clone(), 0.5));
                        }
                    }
                    return None;
                };

                Some((entry.clone(), score))
            })
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(results
            .into_iter()
            .take(limit)
            .map(|(entry, score)| SearchResult {
                id: entry.id,
                snippet: entry
                    .content
                    .chars()
                    .take(100)
                    .collect::<String>()
                    + if entry.content.len() > 100 { "..." } else { "" },
                score,
                matched_tags: entry
                    .tags
                    .into_iter()
                    .filter(|t| t.to_lowercase().contains(&query_lower))
                    .collect(),
            })
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<(), PortError> {
        self.inner.write().entries.remove(id);
        Ok(())
    }

    async fn reindex(&self, entries: Vec<SearchEntry>) -> Result<(), PortError> {
        let mut inner = self.inner.write();
        inner.entries.clear();
        for entry in entries {
            inner.entries.insert(entry.id.clone(), entry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[tokio::test]
    async fn index_and_search() {
        let idx = InMemorySearchIndex::new();

        idx.index(SearchEntry {
            id: "mem-1".into(),
            content: "This project uses bcrypt for password hashing".into(),
            tags: vec!["auth".into(), "security".into()],
            source: "agent-1".into(),
            timestamp: Utc::now(),
        })
        .await
        .unwrap();

        idx.index(SearchEntry {
            id: "mem-2".into(),
            content: "JWT tokens expire after 24 hours".into(),
            tags: vec!["auth".into()],
            source: "agent-1".into(),
            timestamp: Utc::now(),
        })
        .await
        .unwrap();

        let results = idx.search("bcrypt", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "mem-1");

        let results = idx.search("auth", 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn delete_entry() {
        let idx = InMemorySearchIndex::new();

        idx.index(SearchEntry {
            id: "mem-1".into(),
            content: "test content".into(),
            tags: vec![],
            source: "agent-1".into(),
            timestamp: Utc::now(),
        })
        .await
        .unwrap();

        assert_eq!(idx.len(), 1);

        idx.delete("mem-1").await.unwrap();
        assert_eq!(idx.len(), 0);
    }

    #[tokio::test]
    async fn reindex_repopulates_from_entries() {
        let idx = InMemorySearchIndex::new();

        idx.index(SearchEntry {
            id: "mem-1".into(),
            content: "content 1".into(),
            tags: vec![],
            source: "agent-1".into(),
            timestamp: Utc::now(),
        })
        .await
        .unwrap();

        idx.index(SearchEntry {
            id: "mem-2".into(),
            content: "content 2".into(),
            tags: vec![],
            source: "agent-1".into(),
            timestamp: Utc::now(),
        })
        .await
        .unwrap();

        assert_eq!(idx.len(), 2);

        let replacement = vec![SearchEntry {
            id: "mem-3".into(),
            content: "reindexed content".into(),
            tags: vec!["fresh".into()],
            source: "agent-2".into(),
            timestamp: Utc::now(),
        }];
        idx.reindex(replacement).await.unwrap();
        assert_eq!(idx.len(), 1);

        let results = idx.search("reindexed", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "mem-3");
    }
}