// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's `zeph-llm/src/provider.rs` message and tool-use model.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRole {
    System,
    User,
    Assistant,
    Tool,
}

impl AgentRole {
    pub(crate) fn as_openai_role(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolUseRequest {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

impl ToolUseRequest {
    pub(crate) fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    pub(crate) fn to_openai_tool_call(&self) -> Value {
        json!({
            "id": &self.id,
            "type": "function",
            "function": {
                "name": &self.name,
                "arguments": raw_arguments_string(&self.arguments),
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MessagePart {
    Text(String),
    ToolUse(ToolUseRequest),
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentMessage {
    role: AgentRole,
    parts: Vec<MessagePart>,
    assistant_extension_fields: Map<String, Value>,
}

impl AgentMessage {
    pub(crate) fn system(content: impl Into<String>) -> Self {
        Self::text(AgentRole::System, content)
    }

    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self::text(AgentRole::User, content)
    }

    pub(crate) fn assistant(content: Option<String>, tool_calls: Vec<ToolUseRequest>) -> Self {
        Self::assistant_with_extension_fields(content, tool_calls, Map::new())
    }

    pub(crate) fn assistant_with_extension_fields(
        content: Option<String>,
        tool_calls: Vec<ToolUseRequest>,
        assistant_extension_fields: Map<String, Value>,
    ) -> Self {
        let mut parts = Vec::with_capacity(tool_calls.len() + usize::from(content.is_some()));
        if let Some(content) = content.filter(|value| !value.is_empty()) {
            parts.push(MessagePart::Text(content));
        }
        parts.extend(tool_calls.into_iter().map(MessagePart::ToolUse));
        Self {
            role: AgentRole::Assistant,
            parts,
            assistant_extension_fields,
        }
    }

    pub(crate) fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: AgentRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: tool_call_id.into(),
                content: content.into(),
                is_error,
            }],
            assistant_extension_fields: Map::new(),
        }
    }

    fn text(role: AgentRole, content: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![MessagePart::Text(content.into())],
            assistant_extension_fields: Map::new(),
        }
    }

    pub(crate) fn role(&self) -> AgentRole {
        self.role
    }

    pub(crate) fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                MessagePart::ToolResult { content, .. } => Some(content.as_str()),
                MessagePart::ToolUse(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub(crate) fn tool_calls(&self) -> Vec<ToolUseRequest> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::ToolUse(tool_call) => Some(tool_call.clone()),
                MessagePart::Text(_) | MessagePart::ToolResult { .. } => None,
            })
            .collect()
    }

    pub(crate) fn first_tool_result(&self) -> Option<(&str, &str, bool)> {
        self.parts.iter().find_map(|part| match part {
            MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => Some((tool_call_id.as_str(), content.as_str(), *is_error)),
            MessagePart::Text(_) | MessagePart::ToolUse(_) => None,
        })
    }

    fn tool_result_id(&self) -> Option<&str> {
        self.parts.iter().find_map(|part| match part {
            MessagePart::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            MessagePart::Text(_) | MessagePart::ToolUse(_) => None,
        })
    }

    pub(crate) fn to_openai_json(&self) -> Value {
        match self.role {
            AgentRole::System | AgentRole::User => json!({
                "role": self.role.as_openai_role(),
                "content": self.text_content(),
            }),
            AgentRole::Assistant => {
                let content = self.text_content();
                let tool_calls = self.tool_calls();
                let mut message = json!({
                    "role": "assistant",
                    "content": if content.is_empty() { Value::Null } else { Value::String(content) },
                });
                if !tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(ToolUseRequest::to_openai_tool_call)
                            .collect(),
                    );
                }
                for (field, value) in &self.assistant_extension_fields {
                    message[field] = value.clone();
                }
                message
            }
            AgentRole::Tool => {
                let Some((tool_call_id, content, _)) = self.first_tool_result() else {
                    return json!({
                        "role": "tool",
                        "tool_call_id": "",
                        "content": "",
                    });
                };
                json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AssistantTurn {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Vec<ToolUseRequest>,
    pub(crate) history_message: AgentMessage,
    pub(crate) tokens_used: Option<u64>,
    pub(crate) stop_reason: Option<String>,
}

pub(crate) fn trim_message_history(messages: &mut Vec<AgentMessage>, max_messages: usize) {
    if messages.len() <= max_messages {
        sanitize_tool_result_adjacency(messages);
        return;
    }
    let leading_system_messages = messages
        .iter()
        .take_while(|message| message.role() == AgentRole::System)
        .cloned()
        .collect::<Vec<_>>();
    let keep_tail = max_messages.saturating_sub(leading_system_messages.len());
    let mut trimmed = Vec::with_capacity(max_messages);
    trimmed.extend(leading_system_messages);
    let tail_start = messages.len().saturating_sub(keep_tail);
    trimmed.extend(
        messages
            .iter()
            .skip(tail_start)
            .filter(|message| message.role() != AgentRole::System)
            .cloned(),
    );
    sanitize_tool_result_adjacency(&mut trimmed);
    *messages = trimmed;
}

pub(crate) fn sanitize_provider_message_history(messages: &mut Vec<AgentMessage>) {
    let last_user_index = messages
        .iter()
        .rposition(|message| message.role() == AgentRole::User);

    let mut sanitized = Vec::with_capacity(messages.len());
    let mut index = 0usize;
    while index < messages.len() {
        let message = messages[index].clone();
        let before_latest_user = last_user_index.is_some_and(|last_user| index < last_user);
        match message.role() {
            AgentRole::Assistant => {
                let tool_calls = message.tool_calls();
                if tool_calls.is_empty() {
                    sanitized.push(message);
                    index += 1;
                    continue;
                }

                let expected: HashSet<String> = tool_calls
                    .into_iter()
                    .map(|tool_call| tool_call.id)
                    .collect();
                let mut seen = HashSet::new();
                let mut tool_results = Vec::new();
                let mut cursor = index + 1;
                while cursor < messages.len() && messages[cursor].role() == AgentRole::Tool {
                    if let Some(tool_call_id) = messages[cursor].tool_result_id() {
                        if expected.contains(tool_call_id) && seen.insert(tool_call_id.to_string())
                        {
                            tool_results.push(messages[cursor].clone());
                        }
                    }
                    cursor += 1;
                }

                if before_latest_user {
                    index = cursor;
                    continue;
                }

                if seen.len() == expected.len() {
                    sanitized.push(message);
                    sanitized.extend(tool_results);
                }
                index = cursor;
            }
            AgentRole::Tool => {
                index += 1;
            }
            AgentRole::System | AgentRole::User => {
                sanitized.push(message);
                index += 1;
            }
        }
    }
    *messages = sanitized;
}

fn sanitize_tool_result_adjacency(messages: &mut Vec<AgentMessage>) {
    let mut pending_tool_call_ids = HashSet::new();
    messages.retain(|message| match message.role() {
        AgentRole::Assistant => {
            pending_tool_call_ids = message
                .tool_calls()
                .into_iter()
                .map(|tool_call| tool_call.id)
                .collect();
            true
        }
        AgentRole::Tool => {
            let Some(tool_call_id) = message.tool_result_id() else {
                return false;
            };
            pending_tool_call_ids.remove(tool_call_id)
        }
        AgentRole::System | AgentRole::User => {
            pending_tool_call_ids.clear();
            true
        }
    });
}

fn raw_arguments_string(value: &Value) -> String {
    if let Some(raw) = value.as_str() {
        raw.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_message_serializes_tool_calls_for_openai_chat() {
        let message = AgentMessage::assistant(
            None,
            vec![ToolUseRequest::new(
                "call-1",
                "bash",
                json!({"command": "cargo test"}),
            )],
        );

        let openai = message.to_openai_json();
        assert_eq!(openai["role"], "assistant");
        assert!(openai["content"].is_null());
        assert_eq!(openai["tool_calls"][0]["id"], "call-1");
        assert_eq!(openai["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            openai["tool_calls"][0]["function"]["arguments"].as_str(),
            Some("{\"command\":\"cargo test\"}")
        );
    }

    #[test]
    fn assistant_message_serializes_provider_reasoning_extension_when_present() {
        let message = AgentMessage::assistant_with_extension_fields(
            None,
            vec![ToolUseRequest::new(
                "call-1",
                "bash",
                json!({"command": "pwd"}),
            )],
            Map::from_iter([(
                "reasoning_content".to_string(),
                Value::String("provider reasoning".to_string()),
            )]),
        );

        let openai = message.to_openai_json();
        assert_eq!(openai["reasoning_content"], "provider reasoning");
        assert_eq!(openai["tool_calls"][0]["id"], "call-1");
    }

    #[test]
    fn trim_keeps_system_message_and_recent_tail() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("one"),
            AgentMessage::assistant(Some("two".to_string()), Vec::new()),
            AgentMessage::user("three"),
        ];

        trim_message_history(&mut messages, 3);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text_content(), "sys");
        assert_eq!(messages[1].text_content(), "two");
        assert_eq!(messages[2].text_content(), "three");
    }

    #[test]
    fn trim_keeps_leading_system_context_messages() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::system("context"),
            AgentMessage::user("one"),
            AgentMessage::assistant(Some("two".to_string()), Vec::new()),
            AgentMessage::user("three"),
        ];

        trim_message_history(&mut messages, 4);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text_content(), "sys");
        assert_eq!(messages[1].text_content(), "context");
        assert_eq!(messages[2].text_content(), "two");
        assert_eq!(messages[3].text_content(), "three");
    }

    #[test]
    fn trim_drops_orphaned_tool_results() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("one"),
            AgentMessage::assistant(
                None,
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
            ),
            AgentMessage::tool_result("call-1", "ok", false),
            AgentMessage::assistant(Some("done".to_string()), Vec::new()),
        ];

        trim_message_history(&mut messages, 3);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), AgentRole::System);
        assert_eq!(messages[1].text_content(), "done");
    }

    #[test]
    fn trim_keeps_tool_results_when_matching_assistant_is_kept() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("one"),
            AgentMessage::assistant(
                None,
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
            ),
            AgentMessage::tool_result("call-1", "ok", false),
        ];

        trim_message_history(&mut messages, 4);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role(), AgentRole::Assistant);
        assert_eq!(messages[3].role(), AgentRole::Tool);
    }

    #[test]
    fn provider_sanitize_drops_assistant_tool_use_without_results() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("one"),
            AgentMessage::assistant(
                Some("checking".to_string()),
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
            ),
            AgentMessage::user("next"),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role(), AgentRole::System);
        assert_eq!(messages[1].role(), AgentRole::User);
        assert_eq!(messages[2].text_content(), "next");
    }

    #[test]
    fn provider_sanitize_keeps_complete_tool_use_groups() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::assistant_with_extension_fields(
                None,
                vec![
                    ToolUseRequest::new("call-1", "bash", json!({"command": "pwd"})),
                    ToolUseRequest::new("call-2", "read_file", json!({"path": "a.txt"})),
                ],
                Map::from_iter([(
                    "reasoning_content".to_string(),
                    Value::String("same turn reasoning".to_string()),
                )]),
            ),
            AgentMessage::tool_result("call-1", "ok", false),
            AgentMessage::tool_result("call-2", "file", false),
            AgentMessage::assistant(Some("done".to_string()), Vec::new()),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1].role(), AgentRole::Assistant);
        assert_eq!(messages[2].role(), AgentRole::Tool);
        assert_eq!(messages[3].role(), AgentRole::Tool);
        assert_eq!(
            messages[1].to_openai_json()["reasoning_content"],
            "same turn reasoning"
        );
    }

    #[test]
    fn provider_sanitize_keeps_current_turn_tool_reasoning_after_user_prompt() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("do work"),
            AgentMessage::assistant_with_extension_fields(
                None,
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
                Map::from_iter([(
                    "reasoning_content".to_string(),
                    Value::String("active tool reasoning".to_string()),
                )]),
            ),
            AgentMessage::tool_result("call-1", "ok", false),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[2].to_openai_json()["reasoning_content"],
            "active tool reasoning"
        );
    }

    #[test]
    fn provider_sanitize_drops_prior_tool_group_but_keeps_final_reasoning_before_new_user_prompt() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("first task"),
            AgentMessage::assistant_with_extension_fields(
                None,
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
                Map::from_iter([(
                    "reasoning_content".to_string(),
                    Value::String("old tool reasoning".to_string()),
                )]),
            ),
            AgentMessage::tool_result("call-1", "ok", false),
            AgentMessage::assistant_with_extension_fields(
                Some("done".to_string()),
                Vec::new(),
                Map::from_iter([(
                    "reasoning_content".to_string(),
                    Value::String("old final reasoning".to_string()),
                )]),
            ),
            AgentMessage::user("second task"),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), AgentRole::System);
        assert_eq!(messages[1].text_content(), "first task");
        assert_eq!(messages[2].text_content(), "done");
        assert_eq!(
            messages[2].to_openai_json()["reasoning_content"],
            "old final reasoning"
        );
        assert_eq!(messages[3].text_content(), "second task");
        assert!(messages
            .iter()
            .all(|message| message.role() != AgentRole::Tool));
    }

    #[test]
    fn provider_sanitize_drops_prior_tool_groups_without_extension_state() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("first task"),
            AgentMessage::assistant(
                None,
                vec![ToolUseRequest::new(
                    "call-1",
                    "bash",
                    json!({"command": "pwd"}),
                )],
            ),
            AgentMessage::tool_result("call-1", "ok", false),
            AgentMessage::assistant(Some("done".to_string()), Vec::new()),
            AgentMessage::user("second task"),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), AgentRole::System);
        assert_eq!(messages[1].text_content(), "first task");
        assert_eq!(messages[2].text_content(), "done");
        assert_eq!(messages[3].text_content(), "second task");
        assert!(messages
            .iter()
            .all(|message| message.role() != AgentRole::Tool));
    }

    #[test]
    fn provider_sanitize_keeps_reasoning_content_before_new_user_turn() {
        let mut messages = vec![
            AgentMessage::system("sys"),
            AgentMessage::user("one"),
            AgentMessage::assistant_with_extension_fields(
                Some("done".to_string()),
                Vec::new(),
                Map::from_iter([(
                    "reasoning_content".to_string(),
                    Value::String("old reasoning".to_string()),
                )]),
            ),
            AgentMessage::user("two"),
        ];

        sanitize_provider_message_history(&mut messages);

        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[2].to_openai_json()["reasoning_content"],
            "old reasoning"
        );
    }
}
