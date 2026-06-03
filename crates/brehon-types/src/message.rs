//! Message-related types for agent communication.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PromptId(pub String);

impl PromptId {
    /// Create a new `PromptId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PromptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Handle to an active prompt (for cancellation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PromptHandle {
    /// Identifier of the active prompt.
    pub prompt_id: PromptId,
    /// Session that owns this prompt.
    pub session_id: String,
    /// When the prompt was created.
    pub created_at: DateTime<Utc>,
}

/// Kind of nudge to send to a stuck agent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NudgeKind {
    /// Soft check-in: "Are you still working?"
    Soft,
    /// Guidance: specific suggestion.
    Guidance,
    /// Redirect: stop this, try that.
    Redirect,
    /// Resume: operation finished, continue.
    Resume,
}

/// Kind of message content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageKind {
    /// Task assignment.
    TaskAssignment,
    /// Review request.
    ReviewRequest,
    /// Consolidated feedback.
    Feedback,
    /// Nudge/check-in.
    Nudge,
    /// Human intervention.
    HumanIntervention,
    /// System message.
    System,
}

/// Target for a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MessageTarget {
    /// Send to specific session.
    Session(String),
    /// Send to all agents with a role.
    Role(String),
    /// Broadcast to all.
    Broadcast,
}

/// A single prompt/response turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptTurn {
    /// Prompt identifier.
    pub prompt_id: PromptId,
    /// Prompt content.
    pub content: String,
    /// Kind of prompt.
    pub kind: MessageKind,
    /// When prompt was sent.
    pub sent_at: DateTime<Utc>,
}

/// Terminal identifier for interactive sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TerminalId(pub String);

impl TerminalId {
    /// Create a new `TerminalId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Message from/to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMessage {
    /// Message content.
    pub content: String,
    /// Kind of message.
    pub kind: MessageKind,
    /// Target or source.
    pub target: MessageTarget,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nudge_kind_serialization() {
        let kinds = vec![
            NudgeKind::Soft,
            NudgeKind::Guidance,
            NudgeKind::Redirect,
            NudgeKind::Resume,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: NudgeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn prompt_handle() {
        let handle = PromptHandle {
            prompt_id: PromptId::new("p-123"),
            session_id: "s-456".into(),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&handle).unwrap();
        let parsed: PromptHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(handle, parsed);
    }

    #[test]
    fn message_target_roundtrip() {
        let targets = vec![
            MessageTarget::Session("s-1".into()),
            MessageTarget::Role("worker".into()),
            MessageTarget::Broadcast,
        ];
        for target in targets {
            let json = serde_json::to_string(&target).unwrap();
            let parsed: MessageTarget = serde_json::from_str(&json).unwrap();
            assert_eq!(target, parsed);
        }
    }
}
