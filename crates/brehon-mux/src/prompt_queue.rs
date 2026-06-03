use serde::{Deserialize, Serialize};

/// Durable prompt-queue payload delivered from MCP tooling to the TUI loop.
///
/// Session scoping is enforced by `SessionScopedQueue<PromptQueueEntry>` at
/// the envelope level (`StoredScopedEntry::session_name`), not by embedding a
/// session field inside this payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptQueueEntry {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_key: Option<String>,
    /// Monotonic per-prompt identifier the TUI writes back as a delivery ACK
    /// once it has successfully injected this prompt into the target pane.
    /// Optional for backward compatibility with queue entries written before
    /// this field existed; always populated by `PromptQueueEntry::new`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
}

impl PromptQueueEntry {
    pub fn new(target: &str, from: Option<&str>, message: &str) -> Self {
        Self {
            target: target.to_string(),
            from: from.map(str::to_string),
            message: message.to_string(),
            retry_key: None,
            prompt_id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    pub fn with_retry_key(mut self, retry_key: impl Into<String>) -> Self {
        self.retry_key = Some(retry_key.into());
        self
    }

    /// Override the auto-generated prompt id. Primarily for tests; production
    /// code should rely on the id minted by `new`.
    pub fn with_prompt_id(mut self, prompt_id: impl Into<String>) -> Self {
        self.prompt_id = Some(prompt_id.into());
        self
    }
}
