//! Context and memory types for shared agent knowledge.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Trust level for information.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrustLevel {
    /// Unverified, possibly incorrect.
    Unverified,
    /// Likely correct but not confirmed.
    Likely,
    /// Verified and trusted.
    Verified,
    /// Authoritative source.
    Authoritative,
}

/// Stored knowledge/memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Memory {
    /// Memory identifier.
    pub id: String,
    /// Memory content.
    pub content: String,
    /// Tags for categorization.
    pub tags: Vec<String>,
    /// Source agent.
    pub source_agent: String,
    /// Trust level.
    pub trust: TrustLevel,
    /// When created.
    pub created_at: DateTime<Utc>,
    /// When last accessed.
    pub last_accessed: DateTime<Utc>,
}

/// Project coding rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    /// Rule identifier.
    pub id: String,
    /// Rule name/title.
    pub name: String,
    /// Rule description.
    pub description: String,
    /// Rule content (the actual rule).
    pub content: String,
    /// Tags for categorization.
    pub tags: Vec<String>,
    /// Whether the rule is active.
    pub active: bool,
}

/// Reusable skill/pattern.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Skill {
    /// Skill identifier.
    pub id: String,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Skill content (template, pattern, etc.).
    pub content: String,
    /// Tags for categorization.
    pub tags: Vec<String>,
}

/// Entry to index for search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchEntry {
    /// Entry identifier.
    pub id: String,
    /// Content to index.
    pub content: String,
    /// Tags for filtering.
    pub tags: Vec<String>,
    /// Source (e.g., agent id).
    pub source: String,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Search result from the index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    /// Entry identifier.
    pub id: String,
    /// Content snippet.
    pub snippet: String,
    /// Relevance score.
    pub score: f32,
    /// Matching tags.
    pub matched_tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_level_ordering() {
        assert!(TrustLevel::Authoritative > TrustLevel::Verified);
        assert!(TrustLevel::Verified > TrustLevel::Likely);
        assert!(TrustLevel::Likely > TrustLevel::Unverified);
    }

    #[test]
    fn memory_serialization() {
        let memory = Memory {
            id: "mem-1".into(),
            content: "This project uses bcrypt for hashing".into(),
            tags: vec!["auth".into(), "security".into()],
            source_agent: "claude-code".into(),
            trust: TrustLevel::Verified,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
        };
        let json = serde_json::to_string(&memory).unwrap();
        let parsed: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(memory, parsed);
    }

    #[test]
    fn search_result() {
        let result = SearchResult {
            id: "mem-42".into(),
            snippet: "This project uses...".into(),
            score: 0.95,
            matched_tags: vec!["auth".into()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, parsed);
    }
}
