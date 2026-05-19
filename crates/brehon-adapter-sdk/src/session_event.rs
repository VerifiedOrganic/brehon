//! Session update normalization.
//!
//! Converts standard ACP `session/update` notifications into domain events.

use std::borrow::Cow;

use tracing::debug;

use brehon_types::{EventKind, SessionId};

use crate::AdapterEvent;

/// Domain event produced by normalizing raw ACP `session/update` notifications.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// An agent operation has started.
    OperationStarted {
        session_id: SessionId,
        operation: String,
    },
    /// An agent operation has completed.
    OperationCompleted {
        session_id: SessionId,
        operation: String,
        success: bool,
    },
    /// The agent is requesting a permission grant from the supervisor.
    PermissionRequest {
        session_id: SessionId,
        permission_id: String,
        action: String,
        details: Option<serde_json::Value>,
    },
    /// A mediated permission request was resolved.
    PermissionResolved {
        session_id: SessionId,
        permission_id: String,
        approved: bool,
    },
    /// Incremental progress update from the agent.
    Progress {
        session_id: SessionId,
        message: String,
        percent: Option<u8>,
    },
    /// A tool invocation has begun.
    ToolCallStarted {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
        details: Option<serde_json::Value>,
    },
    /// A tool invocation has finished.
    ToolCallCompleted {
        session_id: SessionId,
        tool_id: String,
        tool_name: String,
        status: String,
        details: Option<serde_json::Value>,
    },
    /// Streamed text output from the agent.
    Output { session_id: SessionId, text: String },
}

impl SessionEvent {
    /// Returns the session ID associated with this event.
    pub fn session_id(&self) -> &SessionId {
        match self {
            SessionEvent::OperationStarted { session_id, .. } => session_id,
            SessionEvent::OperationCompleted { session_id, .. } => session_id,
            SessionEvent::PermissionRequest { session_id, .. } => session_id,
            SessionEvent::PermissionResolved { session_id, .. } => session_id,
            SessionEvent::Progress { session_id, .. } => session_id,
            SessionEvent::ToolCallStarted { session_id, .. } => session_id,
            SessionEvent::ToolCallCompleted { session_id, .. } => session_id,
            SessionEvent::Output { session_id, .. } => session_id,
        }
    }
}

/// Converts a normalized ACP session event into the common adapter event stream.
pub fn session_event_to_adapter_event(event: SessionEvent) -> Option<AdapterEvent> {
    match event {
        SessionEvent::Output { text, .. } => Some(AdapterEvent::Output { text }),
        SessionEvent::OperationStarted { operation, .. } => {
            Some(AdapterEvent::OperationStarted { operation })
        }
        SessionEvent::OperationCompleted {
            operation, success, ..
        } => Some(AdapterEvent::OperationCompleted { operation, success }),
        SessionEvent::PermissionRequest {
            permission_id,
            action,
            details,
            ..
        } => Some(AdapterEvent::PermissionRequest {
            permission_id,
            action,
            details,
        }),
        SessionEvent::PermissionResolved { .. } => None,
        SessionEvent::Progress {
            message, percent, ..
        } => Some(AdapterEvent::Progress { message, percent }),
        SessionEvent::ToolCallStarted {
            tool_id,
            tool_name,
            details,
            ..
        } => Some(AdapterEvent::ToolCallStarted {
            tool_id,
            tool_name,
            details,
        }),
        SessionEvent::ToolCallCompleted {
            tool_id,
            tool_name,
            status,
            details,
            ..
        } => Some(AdapterEvent::ToolCallCompleted {
            tool_id,
            tool_name,
            status,
            details,
        }),
    }
}

/// Parses a raw ACP `session/update` JSON value into a [`SessionEvent`].
///
/// Returns `Ok(None)` for update kinds that are intentionally ignored (e.g. thought chunks).
pub fn normalize_session_update_value(
    session_id: &SessionId,
    update: &serde_json::Value,
) -> Result<Option<SessionEvent>, UpdateError> {
    if session_id.as_str().trim().is_empty() {
        return Err(UpdateError::ParseError(
            "Missing non-empty session_id for session/update".to_string(),
        ));
    }

    let kind = update
        .get("sessionUpdate")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| UpdateError::ParseError("Missing sessionUpdate field".to_string()))?;

    let event = match kind {
        "operation_started" => Some(SessionEvent::OperationStarted {
            session_id: session_id.clone(),
            operation: string_field(update, &["operation", "title"]).unwrap_or_default(),
        }),
        "operation_completed" => Some(SessionEvent::OperationCompleted {
            session_id: session_id.clone(),
            operation: string_field(update, &["operation", "title"]).unwrap_or_default(),
            success: update
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }),
        "permission_request" => Some(SessionEvent::PermissionRequest {
            session_id: session_id.clone(),
            permission_id: string_field(update, &["permissionId", "permission_id"])
                .unwrap_or_else(|| "permission".to_string()),
            action: string_field(update, &["action", "kind"]).unwrap_or_default(),
            details: update.get("details").cloned(),
        }),
        "agent_message_chunk" => text_content(update).map(|text| SessionEvent::Output {
            session_id: session_id.clone(),
            text,
        }),
        "agent_thought_chunk" => None,
        "tool_call" => Some(SessionEvent::ToolCallStarted {
            session_id: session_id.clone(),
            tool_id: string_field(update, &["toolCallId", "tool_id"])
                .unwrap_or_else(|| "tool-call".to_string()),
            tool_name: string_field(update, &["title", "toolName", "tool_name"])
                .unwrap_or_else(|| "tool".to_string()),
            details: tool_call_details(update),
        }),
        "tool_call_update" => {
            let status = string_field(update, &["status"]).unwrap_or_else(|| "completed".into());
            if matches!(status.as_str(), "started" | "in_progress" | "running") {
                Some(SessionEvent::ToolCallStarted {
                    session_id: session_id.clone(),
                    tool_id: string_field(update, &["toolCallId", "tool_id"])
                        .unwrap_or_else(|| "tool-call".to_string()),
                    tool_name: string_field(update, &["title", "toolName", "tool_name"])
                        .unwrap_or_else(|| "tool".to_string()),
                    details: tool_call_details(update),
                })
            } else {
                Some(SessionEvent::ToolCallCompleted {
                    session_id: session_id.clone(),
                    tool_id: string_field(update, &["toolCallId", "tool_id"])
                        .unwrap_or_else(|| "tool-call".to_string()),
                    tool_name: string_field(update, &["title", "toolName", "tool_name"])
                        .unwrap_or_else(|| "tool".to_string()),
                    status,
                    details: tool_call_details(update),
                })
            }
        }
        "progress" => Some(SessionEvent::Progress {
            session_id: session_id.clone(),
            message: string_field(update, &["message", "title"]).unwrap_or_default(),
            percent: update
                .get("percent")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as u8),
        }),
        "available_commands_update"
        | "current_mode_update"
        | "plan"
        | "usage_update"
        | "user_message_chunk" => None,
        other => {
            if let Some(text) = text_content(update) {
                Some(SessionEvent::Output {
                    session_id: session_id.clone(),
                    text,
                })
            } else {
                Some(SessionEvent::Progress {
                    session_id: session_id.clone(),
                    message: format!("ACP update: {other}"),
                    percent: None,
                })
            }
        }
    };

    if let Some(event) = &event {
        debug!(session_id = %session_id, kind, event = ?event, "Normalized session update");
    }

    Ok(event)
}

/// Converts a [`SessionEvent`] into the corresponding [`EventKind`] domain event.
///
/// These mappings feed the nudge state machine in `brehon-supervisor::event_monitor`
/// (`Delivered → Acknowledged → ActedOn`). Every variant must carry a non-empty
/// `session_id`; event filters downstream match on it, so an empty string
/// silently drops the event.
///
/// Semantic mapping:
/// - `Output` / `Progress` represent the agent responding with text or status
///   updates — they are the agent acknowledging a prior prompt, so they map to
///   `ResponseReceived` (which drives `NudgeAcknowledged`).
/// - `ToolCallStarted` / `ToolCallCompleted` represent the agent actively acting
///   on a prompt — they map to `OperationStarted` / `OperationCompleted` (which
///   drive `NudgeActedOn`).
pub fn session_event_to_domain_event(event: SessionEvent) -> EventKind {
    match event {
        SessionEvent::OperationStarted {
            session_id,
            operation,
        } => EventKind::OperationStarted {
            session_id: session_id.as_str().to_string(),
            operation,
        },
        SessionEvent::OperationCompleted {
            session_id,
            operation,
            success,
        } => EventKind::OperationCompleted {
            session_id: session_id.as_str().to_string(),
            operation,
            success,
        },
        SessionEvent::PermissionRequest {
            session_id,
            permission_id,
            action,
            ..
        } => EventKind::PermissionRequested {
            session_id: session_id.as_str().to_string(),
            permission_id,
            action,
        },
        SessionEvent::PermissionResolved {
            session_id,
            permission_id,
            approved,
        } => EventKind::PermissionResolved {
            session_id: session_id.as_str().to_string(),
            permission_id,
            approved,
        },
        SessionEvent::Progress {
            session_id,
            message,
            percent: _,
        } => EventKind::ResponseReceived {
            session_id: session_id.as_str().to_string(),
            prompt_id: progress_prompt_id(&message),
            tokens_used: 0,
        },
        SessionEvent::ToolCallStarted {
            session_id,
            tool_id,
            tool_name,
            ..
        } => EventKind::OperationStarted {
            session_id: session_id.as_str().to_string(),
            operation: tool_operation_label(&tool_name, &tool_id),
        },
        SessionEvent::ToolCallCompleted {
            session_id,
            tool_id,
            tool_name,
            status,
            ..
        } => EventKind::OperationCompleted {
            session_id: session_id.as_str().to_string(),
            operation: tool_operation_label(&tool_name, &tool_id),
            success: matches!(
                status.as_str(),
                "completed" | "success" | "ok" | "succeeded"
            ),
        },
        SessionEvent::Output { session_id, text } => EventKind::ResponseReceived {
            session_id: session_id.as_str().to_string(),
            prompt_id: output_prompt_id(&text),
            tokens_used: 0,
        },
    }
}

fn tool_operation_label(tool_name: &str, tool_id: &str) -> String {
    let name = tool_name.trim();
    let id = tool_id.trim();
    match (name.is_empty(), id.is_empty()) {
        (false, false) => format!("{name} ({id})"),
        (false, true) => name.to_string(),
        (true, false) => id.to_string(),
        (true, true) => "tool".to_string(),
    }
}

fn progress_prompt_id(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        "progress".to_string()
    } else {
        format!("progress:{}", truncate_for_id(trimmed, 80))
    }
}

fn output_prompt_id(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "output".to_string()
    } else {
        format!("output:{}", truncate_for_id(trimmed, 80))
    }
}

fn truncate_for_id(value: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max_chars));
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn string_field(update: &serde_json::Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| update.get(*name).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn tool_call_details(update: &serde_json::Value) -> Option<serde_json::Value> {
    let mut object = serde_json::Map::new();

    if let Some(input) = first_value(
        update,
        &[
            "rawInput",
            "raw_input",
            "input",
            "arguments",
            "args",
            "parameters",
        ],
    ) {
        object.insert("input".to_string(), input.clone());
    }

    if let Some(output) = first_value(update, &["output", "rawOutput", "raw_output", "result"]) {
        object.insert("output".to_string(), output.clone());
    } else if let Some(output) = update
        .get("state")
        .and_then(|state| first_value(state, &["output", "result"]))
    {
        object.insert("output".to_string(), output.clone());
    }

    (!object.is_empty()).then_some(serde_json::Value::Object(object))
}

fn first_value<'a>(value: &'a serde_json::Value, names: &[&str]) -> Option<&'a serde_json::Value> {
    names.iter().find_map(|name| value.get(*name))
}

fn text_content(update: &serde_json::Value) -> Option<String> {
    let from_object = update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(serde_json::Value::as_str);

    let from_array = update
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| item.get("content").and_then(serde_json::Value::as_str))
            })
        });

    from_object
        .or(from_array)
        .and_then(preserve_streamed_text)
        .map(Cow::into_owned)
}

fn preserve_streamed_text(text: &str) -> Option<Cow<'_, str>> {
    if text.is_empty() {
        return None;
    }

    if text.contains('\r') {
        Some(Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n")))
    } else {
        Some(Cow::Borrowed(text))
    }
}

/// Errors that can occur when normalizing a session update.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Failed to parse update: {0}")]
    ParseError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_agent_message_chunk() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-123");
        let update = serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": " hello " }
        });

        let event = normalize_session_update_value(&session_id, &update)?;

        match event {
            Some(SessionEvent::Output {
                session_id: sid,
                text,
            }) => {
                assert_eq!(sid.as_str(), "s-123");
                assert_eq!(text, " hello ");
            }
            other => panic!("Wrong event type: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_normalize_agent_message_chunk_keeps_newline_only_delta() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-124");
        let update = serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "\n" }
        });

        let event = normalize_session_update_value(&session_id, &update)?;

        match event {
            Some(SessionEvent::Output { text, .. }) => assert_eq!(text, "\n"),
            other => panic!("Wrong event type: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_normalize_tool_call_update_completed() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-456");
        let update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-1",
            "title": "cargo test",
            "status": "completed"
        });

        let event = normalize_session_update_value(&session_id, &update)?;

        match event {
            Some(SessionEvent::ToolCallCompleted {
                session_id: sid,
                tool_id,
                tool_name,
                status,
                details,
            }) => {
                assert_eq!(sid.as_str(), "s-456");
                assert_eq!(tool_id, "tool-1");
                assert_eq!(tool_name, "cargo test");
                assert_eq!(status, "completed");
                assert!(details.is_none());
            }
            other => panic!("Wrong event type: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn test_normalize_tool_call_preserves_input_and_output_details() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-details");
        let started = serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-1",
            "title": "file_change",
            "rawInput": {
                "path": "src/lib.rs",
                "operation": "edit"
            }
        });
        let completed = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-1",
            "title": "file_change",
            "status": "completed",
            "output": "updated src/lib.rs"
        });

        match normalize_session_update_value(&session_id, &started)? {
            Some(SessionEvent::ToolCallStarted { details, .. }) => {
                assert_eq!(details.as_ref().unwrap()["input"]["path"], "src/lib.rs");
            }
            other => panic!("Wrong event type: {other:?}"),
        }

        match normalize_session_update_value(&session_id, &completed)? {
            Some(SessionEvent::ToolCallCompleted { details, .. }) => {
                assert_eq!(details.as_ref().unwrap()["output"], "updated src/lib.rs");
            }
            other => panic!("Wrong event type: {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn test_normalize_available_commands_update_ignored() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-789");
        let update = serde_json::json!({
            "sessionUpdate": "available_commands_update"
        });

        assert!(normalize_session_update_value(&session_id, &update)?.is_none());
        Ok(())
    }

    #[test]
    fn test_normalize_agent_thought_chunk_ignored() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-thought");
        let update = serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": " clean work tree " }
        });

        assert!(normalize_session_update_value(&session_id, &update)?.is_none());
        Ok(())
    }

    #[test]
    fn test_normalize_usage_update_ignored() -> Result<(), UpdateError> {
        let session_id = SessionId::new("s-usage");
        let update = serde_json::json!({
            "sessionUpdate": "usage_update",
            "usage": { "input_tokens": 42, "output_tokens": 7 }
        });

        assert!(normalize_session_update_value(&session_id, &update)?.is_none());
        Ok(())
    }

    #[test]
    fn test_normalize_rejects_blank_session_id() {
        let session_id = SessionId::new("  ");
        let update = serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" }
        });

        let err = normalize_session_update_value(&session_id, &update).unwrap_err();
        assert!(err.to_string().contains("Missing non-empty session_id"));
    }
}
