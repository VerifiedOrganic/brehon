//! Shared freshness metadata for MCP context responses.

use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolFreshness {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<u64>,
    pub generated_at: String,
    pub truncated: bool,
    pub state_source: String,
    pub stale: bool,
    pub compacted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ToolFreshness {
    pub fn new(source_event_id: Option<u64>, state_source: impl Into<String>) -> Self {
        Self {
            source_event_id,
            generated_at: Utc::now().to_rfc3339(),
            truncated: false,
            state_source: state_source.into(),
            stale: false,
            compacted: false,
            warnings: Vec::new(),
        }
    }

    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        if truncated {
            self.stale = true;
        }
        self
    }

    pub fn compacted(mut self, compacted: bool) -> Self {
        self.compacted = compacted;
        self
    }

    pub fn stale(mut self, stale: bool) -> Self {
        self.stale = stale;
        self
    }

    pub fn warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }
}
