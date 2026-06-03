// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adapted from Zeph's native tool execution dispatch path.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::agent_runtime::events::{RuntimeEvent, ToolOutputEvent, ToolStartEvent};
use crate::agent_runtime::executor::{
    ToolCall, ToolError, ToolExecutionContext, ToolExecutor, ToolOutput,
};
use crate::agent_runtime::message::ToolUseRequest;
use crate::runtime::CancellationToken;
use crate::server::RpcHandle;
use crate::tools::NativeTools;

const MAX_TOOL_RETRIES: usize = 2;
const MAX_TOOL_RETRY_DURATION: Duration = Duration::from_secs(30);
const TOOL_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const TOOL_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolDispatchResult {
    pub(crate) tool_call_id: String,
    pub(crate) content: String,
    pub(crate) is_error: bool,
}

pub(crate) async fn dispatch_tool_calls(
    rpc: &RpcHandle,
    session_id: &str,
    cancel: &CancellationToken,
    tools: &NativeTools,
    max_parallel_tool_calls: usize,
    tool_calls: Vec<ToolUseRequest>,
) -> Vec<ToolDispatchResult> {
    let semaphore = Arc::new(Semaphore::new(max_parallel_tool_calls.max(1)));
    let mut handles = Vec::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        let tool_title = tool_action_title(&tool_call.name, &tool_call.arguments);
        let rpc = rpc.clone();
        let tools = tools.clone();
        let cancel = cancel.clone();
        let semaphore = semaphore.clone();
        let session_id = session_id.to_string();
        let tool_call_id = tool_call.id.clone();
        let tool_name = tool_call.name.clone();
        let arguments = tool_call.arguments.clone();
        handles.push((
            tool_call_id.clone(),
            tokio::spawn(async move {
                if cancel.is_cancelled() {
                    return cancelled_result(tool_call_id);
                }

                let permit = tokio::select! {
                    permit = semaphore.acquire_owned() => permit,
                    _ = cancel.cancelled() => return cancelled_result(tool_call_id),
                };
                let Ok(_permit) = permit else {
                    return ToolDispatchResult {
                        tool_call_id,
                        content: "ERROR: tool scheduler closed".to_string(),
                        is_error: true,
                    };
                };

                emit_runtime_event(
                    &rpc,
                    RuntimeEvent::ToolStart(ToolStartEvent {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        display: tool_title.clone(),
                        input: arguments.clone(),
                    }),
                )
                .await;

                let call = ToolCall::from_request(
                    ToolUseRequest::new(tool_call_id.clone(), tool_name.clone(), arguments),
                    Some(session_id.clone()),
                );
                let ctx = ToolExecutionContext {
                    rpc: &rpc,
                    session_id: &session_id,
                    cancel: &cancel,
                };
                let tool_result = tokio::select! {
                    result = execute_tool_call_with_retries(&tools, ctx, &call) => result,
                    _ = cancel.cancelled() => Err(ToolError::Cancelled),
                };
                let (content, is_error) = match tool_result {
                    Ok(Some(result)) => {
                        let is_error = tool_output_is_error(&result.summary);
                        (result.summary, is_error)
                    }
                    Ok(None) => (format!("ERROR: unsupported tool: {tool_name}"), true),
                    Err(err) => (format!("ERROR: {err}"), true),
                };

                emit_runtime_event(
                    &rpc,
                    RuntimeEvent::ToolOutput(ToolOutputEvent {
                        tool_call_id: tool_call_id.clone(),
                        tool_name,
                        display: tool_title.clone(),
                        output: content.clone(),
                        is_error,
                    }),
                )
                .await;
                if is_error {
                    emit_runtime_event(
                        &rpc,
                        RuntimeEvent::Progress(format!(
                            "{} failed: {}",
                            tool_title,
                            failure_summary(&content)
                        )),
                    )
                    .await;
                }

                ToolDispatchResult {
                    tool_call_id,
                    content,
                    is_error,
                }
            }),
        ));
    }

    let mut results = Vec::with_capacity(handles.len());
    for (tool_call_id, handle) in handles {
        match handle.await {
            Ok(result) => results.push(result),
            Err(err) => results.push(ToolDispatchResult {
                tool_call_id,
                content: format!("ERROR: tool task failed: {err}"),
                is_error: true,
            }),
        }
    }
    results
}

fn cancelled_result(tool_call_id: String) -> ToolDispatchResult {
    ToolDispatchResult {
        tool_call_id,
        content: "ERROR: tool invocation cancelled".to_string(),
        is_error: true,
    }
}

async fn execute_tool_call_with_retries(
    tools: &dyn ToolExecutor,
    ctx: ToolExecutionContext<'_>,
    call: &ToolCall,
) -> Result<Option<ToolOutput>, ToolError> {
    let retryable = tools.is_tool_retryable(&call.tool_id);
    let max_attempts = if retryable { 1 + MAX_TOOL_RETRIES } else { 1 };
    let started_at = Instant::now();
    let mut attempt = 0;
    loop {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        attempt += 1;
        match tools.execute_tool_call(ctx, call).await {
            Err(err) if retryable && attempt < max_attempts && err.is_transient() => {
                let elapsed = started_at.elapsed();
                if elapsed >= MAX_TOOL_RETRY_DURATION {
                    return Err(err);
                }
                let delay =
                    retry_delay(attempt).min(MAX_TOOL_RETRY_DURATION.saturating_sub(elapsed));
                emit_runtime_event(
                    ctx.rpc,
                    RuntimeEvent::Progress(format!(
                        "{} transient failure, retrying attempt {}/{} in {}ms: {}",
                        call.tool_id,
                        attempt + 1,
                        max_attempts,
                        delay.as_millis(),
                        err
                    )),
                )
                .await;
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
                }
            }
            result => return result,
        }
    }
}

fn retry_delay(failed_attempt: usize) -> Duration {
    let shift = failed_attempt.saturating_sub(1).min(10) as u32;
    TOOL_RETRY_BASE_DELAY
        .saturating_mul(2u32.saturating_pow(shift))
        .min(TOOL_RETRY_MAX_DELAY)
}

pub(crate) async fn emit_runtime_event(rpc: &RpcHandle, event: RuntimeEvent) {
    let update = match event {
        RuntimeEvent::Progress(message) => json!({
            "sessionUpdate": "progress",
            "message": message,
        }),
        RuntimeEvent::MessageChunk(text) => json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {
                "type": "text",
                "text": text,
            }
        }),
        RuntimeEvent::ToolStart(event) => json!({
            "sessionUpdate": "tool_call",
            "toolCallId": event.tool_call_id,
            "title": event.display,
            "rawInput": event.input,
        }),
        RuntimeEvent::ToolOutput(event) => json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": event.tool_call_id,
            "title": event.display,
            "status": if event.is_error { "failed" } else { "completed" },
            "output": compact_one_line(&event.output),
        }),
    };
    let _ = rpc
        .send_notification("session/update", Some(json!({ "update": update })))
        .await;
}

pub(crate) fn tool_action_title(name: &str, args: &Value) -> String {
    let detail = match name {
        "bash" => string_arg(args, &["command", "cmd"]).map(str::to_string),
        "read_file" => read_file_display(args),
        "write_file" | "replace_in_file" => {
            string_arg(args, &["path", "file_path", "filepath", "file", "filename"])
                .map(str::to_string)
        }
        "search_text" => {
            string_arg(args, &["pattern", "query", "regex", "text"]).map(str::to_string)
        }
        "list_files" => Some(
            string_arg(args, &["path", "dir", "directory"])
                .unwrap_or(".")
                .to_string(),
        ),
        _ => None,
    }
    .map(|value| compact_one_line(&value))
    .filter(|value| !value.is_empty());

    match detail {
        Some(detail) => format!("{name}: {detail}"),
        None => name.to_string(),
    }
}

fn read_file_display(args: &Value) -> Option<String> {
    let path = string_arg(args, &["path", "file_path", "filepath", "file", "filename"])?;
    let start = value_arg(args, &["start_line", "start", "line_start"]);
    let end = value_arg(args, &["end_line", "end", "line_end"]);
    match (start, end) {
        (Some(start), Some(end)) => Some(format!("{path}:{start}-{end}")),
        (Some(start), None) => Some(format!("{path}:{start}-")),
        (None, Some(end)) => Some(format!("{path}:-{end}")),
        (None, None) => Some(path.to_string()),
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

fn value_arg(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        args.get(*key).and_then(|value| {
            value.as_u64().map(|value| value.to_string()).or_else(|| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
        })
    })
}

fn failure_summary(content: &str) -> String {
    compact_one_line(content.trim_start_matches("ERROR:").trim())
}

pub(crate) fn tool_output_is_error(content: &str) -> bool {
    content.trim_start().starts_with("ERROR:")
}

pub(crate) fn compact_one_line(value: &str) -> String {
    const MAX_LEN: usize = 180;
    let mut compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() > MAX_LEN {
        let mut end = MAX_LEN;
        while !compact.is_char_boundary(end) {
            end -= 1;
        }
        compact.truncate(end);
        compact.push_str("...");
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::new_permission_grant_store;
    use crate::runtime::PermissionMode;
    use brehon_types::config::PermissionsConfig;

    #[tokio::test]
    async fn dispatch_tool_calls_returns_results_in_model_order() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let results = dispatch_tool_calls(
            &rpc,
            "session-1",
            &cancel,
            &tools,
            8,
            vec![
                ToolUseRequest::new("call-1", "list_files", json!({"path": "."})),
                ToolUseRequest::new("call-2", "read_file", json!({"path": "a.txt"})),
            ],
        )
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id, "call-1");
        assert_eq!(results[1].tool_call_id, "call-2");
        assert!(!results[0].is_error);
        assert!(!results[1].is_error);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_returns_cancelled_results_for_each_call() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let results = dispatch_tool_calls(
            &rpc,
            "session-1",
            &cancel,
            &tools,
            8,
            vec![
                ToolUseRequest::new("call-1", "list_files", json!({"path": "."})),
                ToolUseRequest::new("call-2", "read_file", json!({"path": "a.txt"})),
            ],
        )
        .await;

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        assert!(results
            .iter()
            .all(|result| result.content.contains("cancelled")));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_clamps_zero_parallelism_to_one() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let rpc = RpcHandle::new(tokio::io::sink());
        let cancel = CancellationToken::new();

        let results = dispatch_tool_calls(
            &rpc,
            "session-1",
            &cancel,
            &tools,
            0,
            vec![ToolUseRequest::new(
                "call-1",
                "read_file",
                json!({"path": "a.txt"}),
            )],
        )
        .await;

        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error, "{results:?}");
    }

    #[test]
    fn tool_action_title_includes_visible_bash_command() {
        assert_eq!(
            tool_action_title(
                "bash",
                &json!({"command": "cargo test -p brehon-native-agent"})
            ),
            "bash: cargo test -p brehon-native-agent"
        );
    }

    #[test]
    fn tool_action_title_includes_file_path() {
        assert_eq!(
            tool_action_title("write_file", &json!({"path": "src/lib.rs"})),
            "write_file: src/lib.rs"
        );
    }

    #[test]
    fn tool_action_title_includes_read_alias_path_and_range() {
        assert_eq!(
            tool_action_title(
                "read_file",
                &json!({"file_path": "src/lib.rs", "start": 10, "line_end": 25})
            ),
            "read_file: src/lib.rs:10-25"
        );
    }

    #[test]
    fn retry_delay_uses_bounded_exponential_backoff() {
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_millis(1000));
        assert_eq!(retry_delay(10), Duration::from_secs(5));
    }
}
