use crate::error::{Error, Result};
use crate::pane::{ActivityEntry, ActivityKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn format_acp_session_event(
    event: &brehon_acp::updates::SessionEvent,
) -> Option<Vec<u8>> {
    let line = match event {
        brehon_acp::updates::SessionEvent::OperationStarted { operation, .. } => {
            return format_operation_started(operation)
                .map(|line| format!("{line}\r\n").into_bytes());
        }
        brehon_acp::updates::SessionEvent::OperationCompleted {
            operation, success, ..
        } => {
            return format_operation_completed(operation, *success)
                .map(|line| format!("{line}\r\n").into_bytes());
        }
        brehon_acp::updates::SessionEvent::PermissionRequest { action, .. } => {
            accent_status_line("1;33", &format!("permission request: {action}"))
        }
        brehon_acp::updates::SessionEvent::PermissionResolved { approved, .. } => {
            let decision = if *approved { "approved" } else { "denied" };
            accent_status_line("1;33", &format!("permission {decision}"))
        }
        brehon_acp::updates::SessionEvent::Progress {
            message, percent, ..
        } => format_gateway_progress(message, *percent)?,
        brehon_acp::updates::SessionEvent::ToolCallStarted { tool_name, .. } => {
            return format_tool_started(tool_name).map(|line| format!("{line}\r\n").into_bytes());
        }
        brehon_acp::updates::SessionEvent::ToolCallCompleted {
            tool_name, status, ..
        } => {
            return format_tool_completed(tool_name, status)
                .map(|line| format!("{line}\r\n").into_bytes());
        }
        brehon_acp::updates::SessionEvent::Output { text, .. } => {
            return Some(normalize_terminal_output(text).into_bytes());
        }
    };
    Some(format!("{line}\r\n").into_bytes())
}

pub(crate) fn session_event_to_activity_entry(
    event: &brehon_acp::updates::SessionEvent,
) -> Option<ActivityEntry> {
    use std::time::Instant;

    let entry = match event {
        brehon_acp::updates::SessionEvent::OperationStarted { operation, .. } => ActivityEntry {
            kind: ActivityKind::Operation,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some("started".to_string()),
            message: Some(operation.clone()),
            output_chunks: None,
            duration: None,
        },
        brehon_acp::updates::SessionEvent::OperationCompleted {
            operation, success, ..
        } => ActivityEntry {
            kind: ActivityKind::Operation,
            ingested_at: Instant::now(),
            tool_id: None,
            tool_name: None,
            status: Some(if *success {
                "completed".to_string()
            } else {
                "failed".to_string()
            }),
            message: Some(operation.clone()),
            output_chunks: None,
            duration: None,
        },
        brehon_acp::updates::SessionEvent::PermissionRequest {
            permission_id,
            action,
            ..
        } => ActivityEntry {
            kind: ActivityKind::Permission,
            ingested_at: Instant::now(),
            tool_id: Some(permission_id.clone()),
            tool_name: None,
            status: None,
            message: Some(action.clone()),
            output_chunks: None,
            duration: None,
        },
        brehon_acp::updates::SessionEvent::PermissionResolved {
            permission_id,
            approved,
            ..
        } => ActivityEntry {
            kind: ActivityKind::Permission,
            ingested_at: Instant::now(),
            tool_id: Some(permission_id.clone()),
            tool_name: None,
            status: Some(if *approved {
                "approved".to_string()
            } else {
                "denied".to_string()
            }),
            message: None,
            output_chunks: None,
            duration: None,
        },
        brehon_acp::updates::SessionEvent::Progress {
            message, percent, ..
        } => {
            if should_filter_progress(message) {
                return None;
            }
            ActivityEntry {
                kind: ActivityKind::Progress,
                ingested_at: Instant::now(),
                tool_id: None,
                tool_name: None,
                status: percent.map(|p| format!("{}%", p)),
                message: Some(message.clone()),
                output_chunks: None,
                duration: None,
            }
        }
        brehon_acp::updates::SessionEvent::ToolCallStarted {
            tool_id,
            tool_name,
            details,
            ..
        } => {
            let tool_name = display_tool_name(tool_name).into_owned();
            if is_low_signal_tool_name(&tool_name) {
                return None;
            }
            ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: Instant::now(),
                tool_id: Some(tool_id.clone()),
                tool_name: Some(tool_name),
                status: Some("started".to_string()),
                message: tool_call_detail_message(details.as_ref()),
                output_chunks: None,
                duration: None,
            }
        }
        brehon_acp::updates::SessionEvent::ToolCallCompleted {
            tool_id,
            tool_name,
            status,
            details,
            ..
        } => {
            let tool_name = display_tool_name(tool_name).into_owned();
            if is_low_signal_tool_name(&tool_name)
                && matches!(status.as_str(), "completed" | "success" | "ok")
            {
                return None;
            }
            ActivityEntry {
                kind: ActivityKind::ToolCall,
                ingested_at: Instant::now(),
                tool_id: Some(tool_id.clone()),
                tool_name: Some(tool_name),
                status: Some(status.clone()),
                message: tool_call_detail_message(details.as_ref()),
                output_chunks: None,
                duration: None,
            }
        }
        brehon_acp::updates::SessionEvent::Output { text, .. } => {
            if text.is_empty() {
                return None;
            }
            ActivityEntry {
                kind: ActivityKind::Output,
                ingested_at: Instant::now(),
                tool_id: None,
                tool_name: None,
                status: None,
                message: None,
                output_chunks: Some(vec![text.clone()]),
                duration: None,
            }
        }
    };
    Some(entry)
}

fn should_filter_progress(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return true;
    }

    if trimmed.starts_with("ACP update: ") || trimmed.starts_with("Gemini update: ") {
        return true;
    }

    if trimmed.ends_with("thread status: active")
        || trimmed.ends_with("thread status: idle")
        || trimmed == "Codex thread started"
        || trimmed == "session idle"
        || trimmed.ends_with("reasoning started")
        || trimmed.ends_with("reasoning completed")
        || trimmed.ends_with("response started")
        || trimmed.ends_with("response completed")
    {
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("MCP server ")
        && !rest.contains('(')
        && matches!(
            rest.rsplit_once(':').map(|(_, status)| status.trim()),
            Some("starting" | "ready" | "started" | "connected" | "ok")
        )
    {
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("Codex MCP tool ")
        && (rest.ends_with(" completed") || is_low_value_codex_mcp_progress(rest))
    {
        return true;
    }

    if let Some(rest) = trimmed.strip_prefix("Approved Codex MCP tool call on ")
        && is_low_value_codex_mcp_progress(rest)
    {
        return true;
    }

    false
}

fn tool_call_detail_message(details: Option<&serde_json::Value>) -> Option<String> {
    let details = details?;
    let object = details.as_object()?;
    let mut sections = Vec::new();

    if let Some(input) = object.get("input") {
        sections.push(format!("input\n{}", render_detail_value(input)));
    }

    if let Some(output) = object.get("output") {
        sections.push(format!("output\n{}", render_detail_value(output)));
    }

    if sections.is_empty() {
        None
    } else {
        Some(truncate_detail_message(&sections.join("\n\n"), 4096))
    }
}

fn render_detail_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn truncate_detail_message(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn normalize_terminal_output(text: &str) -> String {
    if text.contains('\n') {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

fn format_gateway_progress(message: &str, percent: Option<u8>) -> Option<String> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with("ACP update: ") || trimmed.starts_with("Gemini update: ") {
        return None;
    }

    if trimmed.ends_with("thread status: active")
        || trimmed.ends_with("thread status: idle")
        || trimmed == "Codex thread started"
        || trimmed == "session idle"
        || trimmed.ends_with("reasoning started")
        || trimmed.ends_with("reasoning completed")
        || trimmed.ends_with("response started")
        || trimmed.ends_with("response completed")
    {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("MCP server ") {
        if !rest.contains('(')
            && matches!(
                rest.rsplit_once(':').map(|(_, status)| status.trim()),
                Some("starting" | "ready" | "started" | "connected" | "ok")
            )
        {
            return None;
        }
        return Some(subtle_status_line(&format!("mcp: {rest}")));
    }

    if let Some(rest) = trimmed.strip_prefix("Codex MCP tool ") {
        if rest.ends_with(" completed") || is_low_value_codex_mcp_progress(rest) {
            return None;
        }
        return Some(subtle_status_line(&format!("mcp: {rest}")));
    }

    if let Some(rest) = trimmed.strip_prefix("Approved Codex MCP tool call on ") {
        if is_low_value_codex_mcp_progress(rest) {
            return None;
        }
        return Some(subtle_status_line(&format!("mcp: {rest}")));
    }

    if let Some(rest) = trimmed.strip_prefix("Gemini update: ") {
        return Some(subtle_status_line(rest));
    }

    if let Some(rest) = trimmed.strip_prefix("Codex thread status: ") {
        return Some(subtle_status_line(&format!("session {rest}")));
    }

    if let Some(rest) = trimmed.strip_prefix("Codex error: ") {
        return Some(accent_status_line("1;31", &format!("error: {rest}")));
    }

    if let Some(rest) = trimmed.strip_prefix("Codex ") {
        return Some(subtle_status_line(&rest.to_lowercase()));
    }

    match percent {
        Some(percent) => Some(subtle_status_line(&format!("{trimmed} ({percent}%)"))),
        None => Some(subtle_status_line(trimmed)),
    }
}

pub(crate) fn prompt_delivery_notice(prompt: &str, from: Option<&str>) -> String {
    let source = from
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" from {value}"))
        .unwrap_or_default();

    let label = if prompt.starts_with("You have been idle for ") {
        "idle nudge delivered"
    } else if prompt.starts_with("You have been assigned task ") {
        "task assignment delivered"
    } else if prompt.contains("review request") || prompt.contains("request_review") {
        "review prompt delivered"
    } else {
        "prompt delivered"
    };

    format!("\x1b[2;36m[brehon] {label}{source}.\x1b[0m\r\n")
}

fn humanize_operation(operation: &str) -> &str {
    if is_turn_operation(operation) {
        "response"
    } else {
        operation
    }
}

fn format_operation_started(operation: &str) -> Option<String> {
    if is_low_signal_operation(operation) {
        return None;
    }

    let operation = humanize_operation(operation);
    (!matches!(operation, "response")).then(|| subtle_status_line(&format!("{operation} started")))
}

fn format_operation_completed(operation: &str, success: bool) -> Option<String> {
    let low_signal = is_low_signal_operation(operation);
    let operation = humanize_operation(operation);
    if success && low_signal {
        return None;
    }

    Some(subtle_status_line(&format!(
        "{} {}",
        operation,
        if success { "complete" } else { "failed" }
    )))
}

fn is_low_signal_operation(operation: &str) -> bool {
    let normalized = operation.trim().to_ascii_lowercase();
    normalized == "response" || normalized == "step" || is_turn_operation(operation)
}

fn is_turn_operation(operation: &str) -> bool {
    let normalized = operation.trim().to_ascii_lowercase();
    normalized == "turn" || normalized.ends_with(" turn")
}

fn format_tool_started(tool_name: &str) -> Option<String> {
    let tool_name = display_tool_name(tool_name);
    if is_low_signal_tool_name(tool_name.as_ref()) {
        None
    } else {
        Some(subtle_status_line(&format!("tool: {tool_name}")))
    }
}

fn format_tool_completed(tool_name: &str, status: &str) -> Option<String> {
    let tool_name = display_tool_name(tool_name);
    if matches!(status, "completed" | "success" | "ok")
        && is_low_signal_tool_name(tool_name.as_ref())
    {
        return None;
    }

    let color = if matches!(status, "completed" | "success" | "ok") {
        "2"
    } else {
        "1;31"
    };
    Some(accent_status_line(
        color,
        &format!("tool: {tool_name} {status}"),
    ))
}

fn is_low_signal_tool_name(tool_name: &str) -> bool {
    let normalized = tool_name.trim().to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "tool"
            | "submit_review"
            | "brehon_agent"
            | "brehon_task"
            | "brehon_verification"
            | "brehon_factory"
            | "brehon:session_start"
            | "brehon:whoami"
            | "brehon:review_status"
            | "brehon:status=inreview"
            | "brehon:status=inprogress"
    )
}

fn display_tool_name(tool_name: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = tool_name.trim();
    if trimmed.is_empty() {
        return std::borrow::Cow::Borrowed(trimmed);
    }

    if let Some(normalized) = normalize_json_tool_name(trimmed) {
        return std::borrow::Cow::Owned(normalized);
    }

    std::borrow::Cow::Borrowed(trimmed)
}

fn normalize_json_tool_name(tool_name: &str) -> Option<String> {
    if let Some((prefix, rest)) = tool_name.split_once(':') {
        let prefix = prefix.trim().to_ascii_lowercase().replace('-', "_");
        let rest = rest.trim();
        if rest.starts_with('{') {
            let parsed: serde_json::Value = serde_json::from_str(rest).ok()?;
            if matches!(
                prefix.as_str(),
                "agent" | "task" | "verification" | "factory"
            ) {
                return Some(format!("brehon_{prefix}"));
            }
            if let Some(action) = parsed.get("action").and_then(serde_json::Value::as_str) {
                return Some(format!("brehon:{action}"));
            }
            if let Some(status) = parsed.get("status").and_then(serde_json::Value::as_str) {
                return Some(format!("brehon:status={}", status.to_ascii_lowercase()));
            }
            return None;
        }
    }

    if !tool_name.starts_with('{') {
        return None;
    }

    let parsed: serde_json::Value = serde_json::from_str(tool_name).ok()?;
    let object = parsed.as_object()?;

    if let Some(action) = object.get("action").and_then(serde_json::Value::as_str) {
        return Some(format!("brehon:{action}"));
    }

    if let Some(status) = object.get("status").and_then(serde_json::Value::as_str) {
        return Some(format!("brehon:status={}", status.to_ascii_lowercase()));
    }

    None
}

pub(crate) fn normalize_gateway_tool_event(
    event: brehon_acp::updates::SessionEvent,
    active_tool_names: &mut HashMap<String, String>,
) -> (brehon_acp::updates::SessionEvent, bool) {
    match event {
        brehon_acp::updates::SessionEvent::ToolCallStarted {
            session_id,
            tool_id,
            tool_name,
            details,
        } => {
            let normalized_tool_name = display_tool_name(&tool_name).into_owned();
            let duplicate_start = active_tool_names.contains_key(&tool_id);
            active_tool_names.insert(tool_id.clone(), normalized_tool_name.clone());
            (
                brehon_acp::updates::SessionEvent::ToolCallStarted {
                    session_id,
                    tool_id,
                    tool_name: normalized_tool_name,
                    details,
                },
                duplicate_start,
            )
        }
        brehon_acp::updates::SessionEvent::ToolCallCompleted {
            session_id,
            tool_id,
            tool_name,
            status,
            details,
        } => {
            let normalized_tool_name = if tool_name.trim().is_empty() || tool_name == "tool" {
                active_tool_names
                    .remove(&tool_id)
                    .unwrap_or_else(|| display_tool_name(&tool_name).into_owned())
            } else {
                let normalized = display_tool_name(&tool_name).into_owned();
                active_tool_names.remove(&tool_id);
                normalized
            };
            (
                brehon_acp::updates::SessionEvent::ToolCallCompleted {
                    session_id,
                    tool_id,
                    tool_name: normalized_tool_name,
                    status,
                    details,
                },
                false,
            )
        }
        other => (other, false),
    }
}

fn is_low_value_codex_mcp_progress(rest: &str) -> bool {
    rest.contains("/agent started")
        || rest.ends_with(": session_start")
        || rest.ends_with(": whoami")
        || rest.ends_with(": review_status")
}

fn subtle_status_line(text: &str) -> String {
    accent_status_line("2", text)
}

fn accent_status_line(color: &str, text: &str) -> String {
    format!("\x1b[{color}m{text}\x1b[0m")
}

pub(super) fn ensure_isolated_cwd_is_not_shared_root(
    shared_root: &Path,
    pane_cwd: &Path,
    role: &str,
    name: &str,
) -> Result<()> {
    if !pane_cwd.exists() {
        return Err(Error::terminal(format!(
            "Worktree isolation is enabled, but {role} '{name}' isolated cwd '{}' does not exist.",
            pane_cwd.display()
        )));
    }
    let shared_root = shared_root
        .canonicalize()
        .unwrap_or_else(|_| shared_root.to_path_buf());
    let pane_cwd = pane_cwd
        .canonicalize()
        .unwrap_or_else(|_| pane_cwd.to_path_buf());
    if pane_cwd == shared_root {
        return Err(Error::terminal(format!(
            "Worktree isolation is enabled, but {role} '{name}' resolves to the shared repo root '{}'.",
            shared_root.display()
        )));
    }
    let git_file = pane_cwd.join(".git");
    let git_contents = std::fs::read_to_string(&git_file).map_err(|_| {
        Error::terminal(format!(
            "Worktree isolation is enabled, but {role} '{name}' cwd '{}' is not a linked git worktree.",
            pane_cwd.display()
        ))
    })?;
    let gitdir = git_contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::terminal(format!(
                "Worktree isolation is enabled, but {role} '{name}' cwd '{}' is not a linked git worktree.",
                pane_cwd.display()
            ))
        })?;
    let gitdir_path = PathBuf::from(gitdir);
    let gitdir_path = if gitdir_path.is_absolute() {
        gitdir_path
    } else {
        pane_cwd.join(gitdir_path)
    };
    if !gitdir_path.exists() {
        return Err(Error::terminal(format!(
            "Worktree isolation is enabled, but {role} '{name}' linked gitdir '{}' is missing.",
            gitdir_path.display()
        )));
    }
    Ok(())
}
