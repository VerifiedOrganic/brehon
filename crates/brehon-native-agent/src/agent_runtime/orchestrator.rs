// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's `zeph-core/src/agent/tool_orchestrator.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::hash::{DefaultHasher, Hasher};

use serde_json::Value;

use crate::agent_runtime::doom_loop::DoomLoopDetector;
use crate::agent_runtime::message::{AgentMessage, ToolUseRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolLoopControl {
    Continue,
    Stop(String),
}

#[derive(Debug, Clone)]
pub(crate) struct ToolOrchestrator {
    doom_loop: DoomLoopDetector,
    recent_tool_calls: VecDeque<(String, u64)>,
    repeat_threshold: usize,
    /// Hard ceiling on consecutive tool-call rounds in a single turn. `None`
    /// leaves the turn bounded only by the doom-loop/repeat detectors and the
    /// run-level continuation limit. Set for weak local models so an unbounded
    /// tool loop fails the turn cleanly instead of burning the endpoint.
    max_tool_rounds: Option<usize>,
    /// Number of tool-call rounds observed in the current turn.
    tool_rounds: usize,
}

impl ToolOrchestrator {
    pub(crate) fn new() -> Self {
        let repeat_threshold = 3;
        Self {
            doom_loop: DoomLoopDetector::default(),
            recent_tool_calls: VecDeque::with_capacity(2 * repeat_threshold),
            repeat_threshold,
            max_tool_rounds: None,
            tool_rounds: 0,
        }
    }

    /// Set the per-turn consecutive-tool-round ceiling. A value of `Some(0)` is
    /// treated as no limit, matching the "unset" semantics callers expect.
    pub(crate) fn with_max_tool_rounds(mut self, max_tool_rounds: Option<usize>) -> Self {
        self.max_tool_rounds = max_tool_rounds.filter(|max| *max > 0);
        self
    }

    pub(crate) fn begin_turn(&mut self) {
        self.doom_loop = DoomLoopDetector::default();
        self.recent_tool_calls.clear();
        self.tool_rounds = 0;
    }

    pub(crate) fn observe_assistant_message(&mut self, message: &AgentMessage) -> ToolLoopControl {
        let signature = doom_loop_signature(message);
        if self.doom_loop.observe(&signature) {
            return ToolLoopControl::Stop(format!(
                "native-agent stopped a repeated provider loop after {} identical turns",
                self.doom_loop.repeats()
            ));
        }
        ToolLoopControl::Continue
    }

    pub(crate) fn observe_tool_calls(&mut self, tool_calls: &[ToolUseRequest]) -> ToolLoopControl {
        self.tool_rounds += 1;
        if let Some(max_tool_rounds) = self.max_tool_rounds {
            if self.tool_rounds > max_tool_rounds {
                return ToolLoopControl::Stop(format!(
                    "native-agent reached the maximum of {max_tool_rounds} tool-call rounds in one \
                     turn; stopping to avoid an unbounded tool loop"
                ));
            }
        }
        for tool_call in tool_calls {
            let repeat_key = repeat_key(&tool_call.name, &tool_call.arguments);
            self.push_tool_call(&tool_call.name, repeat_key);
            if self.is_repeat(&tool_call.name, repeat_key) {
                return ToolLoopControl::Stop(format!(
                    "native-agent stopped repeated tool call {} with identical arguments",
                    tool_call.name
                ));
            }
        }
        ToolLoopControl::Continue
    }

    fn push_tool_call(&mut self, name: &str, args_hash: u64) {
        if self.repeat_threshold == 0 {
            return;
        }
        let window = 2 * self.repeat_threshold;
        if self.recent_tool_calls.len() >= window {
            self.recent_tool_calls.pop_front();
        }
        self.recent_tool_calls
            .push_back((truncate_tool_name(name).to_string(), args_hash));
    }

    fn is_repeat(&self, name: &str, args_hash: u64) -> bool {
        if self.repeat_threshold == 0 {
            return false;
        }
        let name = truncate_tool_name(name);
        let count = self
            .recent_tool_calls
            .iter()
            .filter(|(stored_name, stored_hash)| stored_name == name && *stored_hash == args_hash)
            .count();
        count >= self.repeat_threshold
    }
}

pub(crate) fn doom_loop_signature(message: &AgentMessage) -> String {
    let mut signature = message.text_content();
    for tool_call in message.tool_calls() {
        signature.push_str("\n[tool_use: ");
        signature.push_str(&tool_call.name);
        signature.push('(');
        signature.push_str(&tool_call.id);
        signature.push_str(")] ");
        signature.push_str(&stable_args_text(&tool_call.arguments));
    }
    signature
}

#[cfg(test)]
fn stable_args_hash(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(stable_args_text(value).as_bytes());
    hasher.finish()
}

fn repeat_key(name: &str, args: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(truncate_tool_name(name).as_bytes());
    hasher.write(b":");
    if let Some(key) = semantic_read_key(name, args) {
        hasher.write(key.as_bytes());
    } else {
        hasher.write(stable_args_text(args).as_bytes());
    }
    hasher.finish()
}

fn semantic_read_key(name: &str, args: &Value) -> Option<String> {
    match name {
        "read_file" => Some(format!(
            "path={};start={};end={}",
            string_arg(args, &["path", "file_path", "filepath", "file", "filename"])?,
            int_arg(args, &["start_line", "start", "line_start"]).unwrap_or(1),
            int_arg(args, &["end_line", "end", "line_end"])
                .map(|value| value.to_string())
                .unwrap_or_default()
        )),
        "list_files" => Some(format!(
            "path={}",
            string_arg(args, &["path", "dir", "directory"]).unwrap_or(".")
        )),
        "search_text" => Some(format!(
            "pattern={};path={}",
            string_arg(args, &["pattern", "query", "regex"])?,
            string_arg(args, &["path", "dir", "directory"]).unwrap_or(".")
        )),
        _ => None,
    }
}

fn stable_args_text(value: &Value) -> String {
    let normalized = sort_json(value);
    serde_json::to_string(&normalized).unwrap_or_else(|_| String::new())
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), sort_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(sort_json).collect()),
        other => other.clone(),
    }
}

fn string_arg<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        args.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn int_arg(args: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        args.get(*key).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
    })
}

fn truncate_tool_name(name: &str) -> &str {
    const MAX_TOOL_NAME_BYTES: usize = 256;
    if name.len() <= MAX_TOOL_NAME_BYTES {
        return name;
    }
    let mut idx = MAX_TOOL_NAME_BYTES;
    while !name.is_char_boundary(idx) {
        idx -= 1;
    }
    &name[..idx]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeat_detector_trips_on_identical_tool_calls() {
        let mut orchestrator = ToolOrchestrator::new();
        let calls = vec![ToolUseRequest::new(
            "call-1",
            "bash",
            json!({"command": "pwd"}),
        )];

        assert_eq!(
            orchestrator.observe_tool_calls(&calls),
            ToolLoopControl::Continue
        );
        assert_eq!(
            orchestrator.observe_tool_calls(&calls),
            ToolLoopControl::Continue
        );
        assert!(matches!(
            orchestrator.observe_tool_calls(&calls),
            ToolLoopControl::Stop(_)
        ));
    }

    fn distinct_call(n: usize) -> Vec<ToolUseRequest> {
        vec![ToolUseRequest::new(
            format!("call-{n}"),
            "bash",
            json!({ "command": format!("echo {n}") }),
        )]
    }

    #[test]
    fn max_tool_rounds_stops_after_limit() {
        let mut orchestrator = ToolOrchestrator::new().with_max_tool_rounds(Some(2));

        assert_eq!(
            orchestrator.observe_tool_calls(&distinct_call(1)),
            ToolLoopControl::Continue
        );
        assert_eq!(
            orchestrator.observe_tool_calls(&distinct_call(2)),
            ToolLoopControl::Continue
        );
        assert!(matches!(
            orchestrator.observe_tool_calls(&distinct_call(3)),
            ToolLoopControl::Stop(_)
        ));
    }

    #[test]
    fn max_tool_rounds_none_is_unlimited() {
        let mut orchestrator = ToolOrchestrator::new();
        for n in 0..12 {
            assert_eq!(
                orchestrator.observe_tool_calls(&distinct_call(n)),
                ToolLoopControl::Continue
            );
        }
    }

    #[test]
    fn max_tool_rounds_zero_is_treated_as_unlimited() {
        let mut orchestrator = ToolOrchestrator::new().with_max_tool_rounds(Some(0));
        for n in 0..12 {
            assert_eq!(
                orchestrator.observe_tool_calls(&distinct_call(n)),
                ToolLoopControl::Continue
            );
        }
    }

    #[test]
    fn begin_turn_resets_tool_round_count() {
        let mut orchestrator = ToolOrchestrator::new().with_max_tool_rounds(Some(1));
        assert_eq!(
            orchestrator.observe_tool_calls(&distinct_call(1)),
            ToolLoopControl::Continue
        );
        orchestrator.begin_turn();
        // Without the reset this second round would exceed the limit and stop.
        assert_eq!(
            orchestrator.observe_tool_calls(&distinct_call(2)),
            ToolLoopControl::Continue
        );
    }

    #[test]
    fn repeat_detector_canonicalizes_read_file_target_aliases() {
        let mut orchestrator = ToolOrchestrator::new();
        let calls = [
            ToolUseRequest::new("call-1", "read_file", json!({"path": "a.md"})),
            ToolUseRequest::new("call-2", "read_file", json!({"file_path": "a.md"})),
            ToolUseRequest::new("call-3", "read_file", json!({"path": "a.md"})),
        ];

        assert_eq!(
            orchestrator.observe_tool_calls(&calls[..1]),
            ToolLoopControl::Continue
        );
        assert_eq!(
            orchestrator.observe_tool_calls(&calls[1..2]),
            ToolLoopControl::Continue
        );
        assert!(matches!(
            orchestrator.observe_tool_calls(&calls[2..]),
            ToolLoopControl::Stop(_)
        ));
    }

    #[test]
    fn repeat_detector_allows_distinct_read_ranges() {
        let mut orchestrator = ToolOrchestrator::new();
        for line in [1, 20, 40, 60] {
            assert_eq!(
                orchestrator.observe_tool_calls(&[ToolUseRequest::new(
                    format!("call-{line}"),
                    "read_file",
                    json!({"path": "a.md", "start_line": line, "end_line": line + 10}),
                )]),
                ToolLoopControl::Continue
            );
        }
    }

    #[test]
    fn stable_args_hash_ignores_object_key_order() {
        assert_eq!(
            stable_args_hash(&json!({"path": "a", "old": "x", "new": "y"})),
            stable_args_hash(&json!({"new": "y", "old": "x", "path": "a"}))
        );
    }

    #[test]
    fn doom_loop_signature_normalizes_to_existing_hash_input() {
        let message = AgentMessage::assistant(
            Some("checking".to_string()),
            vec![ToolUseRequest::new(
                "call-1",
                "read_file",
                json!({"path": "a"}),
            )],
        );

        let signature = doom_loop_signature(&message);

        assert!(signature.contains("[tool_use: read_file(call-1)]"));
        assert!(signature.contains("\"path\":\"a\""));
    }
}
