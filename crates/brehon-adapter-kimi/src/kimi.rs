use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{debug, warn};

use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};

use brehon_adapter_sdk::{
    process::AgentProcess,
    protocol::{
        parse_message, serialize_notification, serialize_request, JsonRpcError, JsonRpcMessage,
        JsonRpcRequest, JsonRpcResponse,
    },
    push_brehon_root_env,
    session_event::{normalize_session_update_value, session_event_to_adapter_event},
    stability_runtime::{
        clear_session_snapshot, persist_session_snapshot, schedule_clear_session_snapshot,
        schedule_persist_session_snapshot,
    },
    AdapterError, AdapterEvent, AdapterResult, AgentAdapter, PromptResult,
};

use crate::acp_types::{
    create_cancel_notification, create_initialize_request, create_new_session_request,
    create_prompt_request, parse_new_session_result, parse_prompt_result,
    PromptResult as AcpPromptResult, SessionMetadata,
};

// =============================================================================
// Constants
// =============================================================================

const KIMI_DEFAULT_MODEL_KEY: &str = "kimi-code/kimi-for-coding";
const KIMI_DEFAULT_MODEL_NAME: &str = "kimi-for-coding";
const KIMI_DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding/v1";
// Kimi Code documents `kimi-for-coding` as a 262144-token context window.
// Compact at 70% so a large ReadFile/ToolOutput result cannot push the next
// API request over the wire limit.
const KIMI_DEFAULT_MAX_CONTEXT_SIZE: u32 = 262_144;
const KIMI_RESERVED_CONTEXT_SIZE: u32 = 80_000;
const KIMI_COMPACTION_TRIGGER_RATIO: &str = "0.70";
const KIMI_DEFAULT_CAPABILITIES: &str = "thinking,image_in,video_in";
const KIMI_TOOL_CALL_TIMEOUT_MS: u64 = 600_000;

const KIMI_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const KIMI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const KIMI_PROMPT_ACCEPT_TIMEOUT: Duration = Duration::from_millis(1500);
const KIMI_LOG_POLL_INTERVAL: Duration = Duration::from_millis(250);
const KIMI_FATAL_LOG_MAX_CHARS: usize = 700;

// =============================================================================
// Kimi runtime preparation helpers (moved from brehon-pty)
// =============================================================================

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

fn current_brehon_session_name() -> Option<String> {
    std::env::var("BREHON_SESSION_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn push_inherited_worktree_root_env(env: &mut Vec<(String, String)>, worktree_root: Option<&str>) {
    let Some(worktree_root) = worktree_root
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    env.push((
        "BREHON_WORKTREE_ROOT".to_string(),
        worktree_root.to_string(),
    ));
}

fn kimi_brehon_mcp_env(
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    name: Option<&str>,
    role: Option<&str>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![(
        "BREHON_WORKSPACE_ROOT".to_string(),
        cwd.to_string_lossy().to_string(),
    )];

    if let Some(session_name) = current_brehon_session_name() {
        env.push(("BREHON_SESSION_NAME".to_string(), session_name));
    }

    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
        env.push(("BREHON_AGENT_NAME".to_string(), name.to_string()));
        env.push(("BREHON_AGENT_TYPE".to_string(), "kimi".to_string()));
        env.push((
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ));
    }

    if let Some(role) = role.filter(|value| !value.trim().is_empty()) {
        env.push(("BREHON_AGENT_ROLE".to_string(), role.to_string()));
    }

    if let Some(root) = brehon_root {
        push_brehon_root_env(&mut env, root);
    }
    let inherited_worktree_root = std::env::var("BREHON_WORKTREE_ROOT").ok();
    push_inherited_worktree_root_env(&mut env, inherited_worktree_root.as_deref());

    if let Some(supervisor_name) = supervisor_name.filter(|value| !value.trim().is_empty()) {
        env.push((
            "BREHON_SUPERVISOR_NAME".to_string(),
            supervisor_name.to_string(),
        ));
    }
    if let Some(factory_worker_cli) = factory_worker_cli.filter(|value| !value.trim().is_empty()) {
        env.push((
            "BREHON_FACTORY_WORKER_CLI".to_string(),
            factory_worker_cli.to_string(),
        ));
    }

    env
}

/// Build the MCP server configuration for Kimi.
pub fn desired_kimi_mcp_config(
    exe: &str,
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    name: Option<&str>,
    role: Option<&str>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
) -> serde_json::Value {
    let env = kimi_brehon_mcp_env(
        cwd,
        brehon_root,
        name,
        role,
        supervisor_name,
        factory_worker_cli,
    )
    .into_iter()
    .map(|(key, value)| (key, serde_json::Value::String(value)))
    .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "mcpServers": {
            "brehon": {
                "command": exe,
                "args": ["serve"],
                "env": env,
            }
        }
    })
}

fn desired_kimi_acp_mcp_servers_json(
    exe: &str,
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    name: Option<&str>,
    role: Option<&str>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
) -> String {
    let env = kimi_brehon_mcp_env(
        cwd,
        brehon_root,
        name,
        role,
        supervisor_name,
        factory_worker_cli,
    )
    .into_iter()
    .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
    .collect::<Vec<_>>();
    serde_json::to_string(&vec![serde_json::json!({
        "name": "brehon",
        "command": exe,
        "args": ["serve"],
        "env": env,
    })])
    .unwrap_or_else(|_| "[]".to_string())
}

fn parse_kimi_acp_mcp_servers(env: &[(String, String)]) -> Vec<serde_json::Value> {
    env.iter()
        .find_map(|(key, value)| (key == "BREHON_ACP_MCP_SERVERS_JSON").then_some(value))
        .and_then(|value| serde_json::from_str::<Vec<serde_json::Value>>(value).ok())
        .unwrap_or_default()
}

fn kimi_share_dir_from_env(env: &[(String, String)]) -> Option<PathBuf> {
    env.iter()
        .find_map(|(key, value)| (key == "KIMI_SHARE_DIR").then_some(value))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn kimi_fatal_provider_error_message(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let invalid_tool_history = (lower.contains("tool_calls")
        && lower.contains("must be followed by tool messages"))
        || lower.contains("did not have response messages");
    let context_limit = lower.contains("context_length_exceeded")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("too many tokens")
        || lower.contains("token limit exceeded")
        || lower.contains("exceeded model token limit")
        || lower.contains("model token limit")
        || lower.contains("maximum context length")
        || lower.contains("max context length")
        || lower.contains("context limit exceeded")
        || lower.contains("context length exceeded")
        || lower.contains("context window exceeded")
        || lower.contains("context window exceeds");
    let provider_rejected = lower.contains("apistatuserror")
        || lower.contains("badrequesterror")
        || lower.contains("invalid_request_error")
        || lower.contains("context_length_exceeded");
    if !((invalid_tool_history && provider_rejected) || context_limit) {
        return None;
    }

    let start = lower
        .find("an assistant message with")
        .or_else(|| lower.find("maximum context length"))
        .or_else(|| lower.find("context_length_exceeded"))
        .or_else(|| lower.find("prompt is too long"))
        .or_else(|| lower.find("error code"))
        .unwrap_or(0);
    let diagnostic = line
        .get(start..)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(line.trim());
    Some(format!(
        "Kimi provider/runtime failure: {}",
        truncate_chars(diagnostic, KIMI_FATAL_LOG_MAX_CHARS)
    ))
}

/// Return the local Kimi share directory for a workspace.
pub fn kimi_share_dir(cwd: &Path) -> PathBuf {
    cwd.join(".brehon/factory-runtime/kimi/share")
}

fn kimi_log_path(share_dir: &Path) -> PathBuf {
    share_dir.join("logs/kimi.log")
}

fn kimi_config_path(share_dir: &Path) -> PathBuf {
    share_dir.join("config.toml")
}

fn kimi_mcp_path(share_dir: &Path) -> PathBuf {
    share_dir.join("mcp.json")
}

fn kimi_global_share_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kimi"))
}

fn raw_kimi_model_name(model: &str) -> &str {
    model
        .trim()
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(KIMI_DEFAULT_MODEL_NAME)
}

pub fn kimi_thinking_enabled(reasoning_effort: &str) -> bool {
    !reasoning_effort.trim().eq_ignore_ascii_case("off")
}

fn quote_toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn parse_toml_string_assignment(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim();
    let prefix = format!("{key} = ");
    let value = trimmed.strip_prefix(&prefix)?;
    let value = value.trim();
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    Some(value.replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn extract_kimi_model_keys(config: &str) -> Vec<String> {
    config
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("[models.\"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(ToString::to_string)
        })
        .collect()
}

fn extract_kimi_default_model_key(config: &str) -> Option<String> {
    config
        .lines()
        .find_map(|line| parse_toml_string_assignment(line, "default_model"))
}

fn upsert_toml_assignment(content: &str, key: &str, value: &str) -> String {
    let assignment = format!("{key} = {value}");
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        if !replaced && line.trim_start().starts_with(&format!("{key} =")) {
            lines.push(assignment.clone());
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if !replaced {
        lines.insert(0, assignment);
    }

    let mut result = lines.join("\n");
    if content.ends_with('\n') || result.is_empty() {
        result.push('\n');
    }
    result
}

fn upsert_toml_assignment_in_table(
    content: &str,
    table_header: &str,
    key: &str,
    value: &str,
) -> String {
    let assignment = format!("{key} = {value}");
    let mut lines = content.lines().map(ToString::to_string).collect::<Vec<_>>();
    let Some(table_start) = lines.iter().position(|line| line.trim() == table_header) else {
        return upsert_toml_assignment(content, key, value);
    };

    let mut insert_at = lines.len();
    for idx in table_start + 1..lines.len() {
        let trimmed = lines[idx].trim_start();
        if trimmed.starts_with('[') {
            insert_at = idx;
            break;
        }
        if trimmed.starts_with(&format!("{key} =")) {
            lines[idx] = assignment;
            let mut result = lines.join("\n");
            if content.ends_with('\n') || result.is_empty() {
                result.push('\n');
            }
            return result;
        }
    }

    lines.insert(insert_at, assignment);
    let mut result = lines.join("\n");
    if content.ends_with('\n') || result.is_empty() {
        result.push('\n');
    }
    result
}

fn upsert_toml_assignment_in_table_or_create(
    content: &str,
    table_header: &str,
    key: &str,
    value: &str,
) -> String {
    if content.lines().any(|line| line.trim() == table_header) {
        return upsert_toml_assignment_in_table(content, table_header, key, value);
    }

    let mut result = content.to_string();
    if !result.trim().is_empty() {
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push('\n');
    }
    result.push_str(table_header);
    result.push('\n');
    result.push_str(&format!("{key} = {value}\n"));
    result
}

fn build_minimal_kimi_config(
    model_key: &str,
    raw_model_name: &str,
    thinking_enabled: bool,
    allow_yolo: bool,
) -> String {
    format!(
        "default_model = {default_model}\n\
         default_thinking = {default_thinking}\n\
         default_yolo = {default_yolo}\n\n\
         [models.{model_key}]\n\
         provider = \"managed:kimi-code\"\n\
         model = {raw_model_name}\n\
         max_context_size = {max_context_size}\n\
         capabilities = [\"thinking\", \"image_in\", \"video_in\"]\n\n\
         [providers.\"managed:kimi-code\"]\n\
         type = \"kimi\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"\"\n\n\
         [providers.\"managed:kimi-code\".oauth]\n\
         storage = \"file\"\n\
         key = \"oauth/kimi-code\"\n\n\
         [loop_control]\n\
         reserved_context_size = {reserved_context_size}\n\
         compaction_trigger_ratio = {compaction_trigger_ratio}\n\n\
        [mcp.client]\n\
        tool_call_timeout_ms = {tool_call_timeout_ms}\n",
        default_model = quote_toml_string(model_key),
        default_thinking = if thinking_enabled { "true" } else { "false" },
        default_yolo = if allow_yolo { "true" } else { "false" },
        model_key = quote_toml_string(model_key),
        raw_model_name = quote_toml_string(raw_kimi_model_name(raw_model_name)),
        max_context_size = KIMI_DEFAULT_MAX_CONTEXT_SIZE,
        base_url = KIMI_DEFAULT_BASE_URL,
        reserved_context_size = KIMI_RESERVED_CONTEXT_SIZE,
        compaction_trigger_ratio = KIMI_COMPACTION_TRIGGER_RATIO,
        tool_call_timeout_ms = KIMI_TOOL_CALL_TIMEOUT_MS,
    )
}

fn patch_existing_kimi_config(
    content: &str,
    model_key: &str,
    reasoning_effort: Option<&str>,
    allow_yolo: bool,
) -> String {
    let mut patched =
        upsert_toml_assignment(content, "default_model", &quote_toml_string(model_key));
    patched = upsert_toml_assignment(
        &patched,
        "default_yolo",
        if allow_yolo { "true" } else { "false" },
    );
    patched = upsert_toml_assignment_in_table(
        &patched,
        &format!("[models.{}]", quote_toml_string(model_key)),
        "max_context_size",
        &KIMI_DEFAULT_MAX_CONTEXT_SIZE.to_string(),
    );
    patched = upsert_toml_assignment_in_table_or_create(
        &patched,
        "[mcp.client]",
        "tool_call_timeout_ms",
        &KIMI_TOOL_CALL_TIMEOUT_MS.to_string(),
    );
    patched = upsert_toml_assignment_in_table_or_create(
        &patched,
        "[loop_control]",
        "reserved_context_size",
        &KIMI_RESERVED_CONTEXT_SIZE.to_string(),
    );
    patched = upsert_toml_assignment_in_table_or_create(
        &patched,
        "[loop_control]",
        "compaction_trigger_ratio",
        KIMI_COMPACTION_TRIGGER_RATIO,
    );
    if let Some(reasoning_effort) = reasoning_effort {
        let thinking_enabled = kimi_thinking_enabled(reasoning_effort);
        patched = upsert_toml_assignment(
            &patched,
            "default_thinking",
            if thinking_enabled { "true" } else { "false" },
        );
    }
    patched
}

fn selected_kimi_model_key(global_config: &str, requested_model: Option<&str>) -> Option<String> {
    let model_keys = extract_kimi_model_keys(global_config);
    if model_keys.is_empty() {
        return None;
    }

    requested_model
        .filter(|requested_model| {
            model_keys
                .iter()
                .any(|model_key| model_key == requested_model)
        })
        .map(ToString::to_string)
        .or_else(|| extract_kimi_default_model_key(global_config))
        .or_else(|| model_keys.first().cloned())
}

fn seed_kimi_share_from_global(
    local_share_dir: &Path,
    global_share_dir: &Path,
) -> std::result::Result<(), &'static str> {
    for relative_path in ["device_id", "credentials", "plugins"] {
        let source = global_share_dir.join(relative_path);
        if source.exists() {
            mirror_path(&source, &local_share_dir.join(relative_path))
                .map_err(|_| "Failed to seed local Kimi auth state.")?;
        }
    }
    Ok(())
}

/// Prepare local Kimi runtime, optionally seeding from a global share directory.
pub fn prepare_local_kimi_runtime_with_global_share(
    cwd: &Path,
    exe: &str,
    brehon_root: Option<&PathBuf>,
    global_share_dir: Option<&Path>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    allow_yolo: bool,
) -> std::result::Result<(PathBuf, Option<String>), &'static str> {
    let share_dir = kimi_share_dir(cwd);
    std::fs::create_dir_all(&share_dir)
        .map_err(|_| "Failed to create local Kimi runtime directory.")?;

    if let Some(global_share_dir) = global_share_dir.filter(|path| path.exists()) {
        seed_kimi_share_from_global(&share_dir, global_share_dir)?;
    }

    let requested_model = model.map(str::trim).filter(|value| !value.is_empty());
    let global_config_path = global_share_dir.map(|path| path.join("config.toml"));
    let global_config = global_config_path
        .as_deref()
        .filter(|path| path.exists())
        .and_then(|path| std::fs::read_to_string(path).ok());

    let (config_text, model_name_override) = if let Some(global_config) = global_config {
        if let Some(model_key) = selected_kimi_model_key(&global_config, requested_model) {
            let model_name_override = requested_model
                .filter(|requested_model| *requested_model != model_key)
                .map(|requested_model| raw_kimi_model_name(requested_model).to_string());
            (
                patch_existing_kimi_config(
                    &global_config,
                    &model_key,
                    reasoning_effort,
                    allow_yolo,
                ),
                model_name_override,
            )
        } else {
            let model_key = requested_model.unwrap_or(KIMI_DEFAULT_MODEL_KEY);
            (
                build_minimal_kimi_config(
                    model_key,
                    raw_kimi_model_name(model_key),
                    reasoning_effort.map(kimi_thinking_enabled).unwrap_or(true),
                    allow_yolo,
                ),
                None,
            )
        }
    } else {
        let model_key = requested_model.unwrap_or(KIMI_DEFAULT_MODEL_KEY);
        (
            build_minimal_kimi_config(
                model_key,
                raw_kimi_model_name(model_key),
                reasoning_effort.map(kimi_thinking_enabled).unwrap_or(true),
                allow_yolo,
            ),
            None,
        )
    };

    std::fs::write(kimi_config_path(&share_dir), config_text)
        .map_err(|_| "Failed to write local Kimi config.")?;
    write_json_config(
        &kimi_mcp_path(&share_dir),
        &desired_kimi_mcp_config(exe, cwd, brehon_root, None, None, None, None),
    )?;

    Ok((share_dir, model_name_override))
}

/// Prepare local Kimi runtime, seeding from the default global share directory.
pub fn prepare_local_kimi_runtime(
    cwd: &Path,
    exe: &str,
    brehon_root: Option<&PathBuf>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    allow_yolo: bool,
) -> std::result::Result<(PathBuf, Option<String>), &'static str> {
    let global_share_dir = kimi_global_share_dir();
    prepare_local_kimi_runtime_with_global_share(
        cwd,
        exe,
        brehon_root,
        global_share_dir.as_deref(),
        model,
        reasoning_effort,
        allow_yolo,
    )
}

// =============================================================================
// Filesystem helpers (copied from brehon-pty to avoid dependency)
// =============================================================================

fn write_json_config(
    path: &Path,
    value: &serde_json::Value,
) -> std::result::Result<(), &'static str> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "Failed to create directory for local factory config.")?;
    }

    let content = serde_json::to_string_pretty(value)
        .map_err(|_| "Failed to serialize local factory config.")?;
    std::fs::write(path, content).map_err(|_| "Failed to write local factory config.")?;
    Ok(())
}

#[cfg(unix)]
fn link_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn link_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn mirror_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Ok(());
    }

    if link_path(src, dst).is_ok() {
        return Ok(());
    }

    if src.is_dir() {
        copy_dir_recursive(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

// =============================================================================
// Kimi spawn config (public API for brehon-pty)
// =============================================================================

/// Configuration for spawning a Kimi CLI session.
#[derive(Debug, Clone)]
pub struct KimiSpawnConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

/// Build the base Kimi spawn configuration.
///
/// This is used by both the interactive TUI mode (via `brehon-pty`) and the
/// ACP adapter mode.  Callers that need startup prompts for the interactive
/// TUI should append them to `args` after calling this function.
#[allow(clippy::too_many_arguments)]
pub fn build_kimi_spawn_config(
    name: &str,
    role: &str,
    cwd: PathBuf,
    brehon_root: Option<&PathBuf>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    allow_yolo: bool,
) -> KimiSpawnConfig {
    let brehon_exe = current_brehon_exe();
    let (share_dir, model_name_override) = prepare_local_kimi_runtime(
        &cwd,
        &brehon_exe,
        brehon_root,
        model,
        reasoning_effort,
        allow_yolo,
    )
    .unwrap_or_else(|_| {
        (
            kimi_share_dir(&cwd),
            model.map(raw_kimi_model_name).map(ToString::to_string),
        )
    });

    let _ = write_json_config(
        &kimi_mcp_path(&share_dir),
        &desired_kimi_mcp_config(
            &brehon_exe,
            &cwd,
            brehon_root,
            Some(name),
            Some(role),
            supervisor_name,
            factory_worker_cli,
        ),
    );

    let session_id = uuid::Uuid::new_v4().to_string();

    let mut env = vec![
        ("BREHON_AGENT_NAME".to_string(), name.to_string()),
        ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
        ("BREHON_AGENT_TYPE".to_string(), "kimi".to_string()),
        ("BREHON_SESSION_ID".to_string(), session_id),
        (
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
        (
            "KIMI_SHARE_DIR".to_string(),
            share_dir.to_string_lossy().to_string(),
        ),
        ("KIMI_CLI_NO_AUTO_UPDATE".to_string(), "true".to_string()),
        (
            "BREHON_ACP_MCP_SERVERS_JSON".to_string(),
            desired_kimi_acp_mcp_servers_json(
                &brehon_exe,
                &cwd,
                brehon_root,
                Some(name),
                Some(role),
                supervisor_name,
                factory_worker_cli,
            ),
        ),
    ];
    brehon_adapter_sdk::prepend_current_exe_dir_to_path(&mut env);
    brehon_adapter_sdk::push_workspace_root_env(&mut env, &cwd);

    if let Some(root) = brehon_root {
        push_brehon_root_env(&mut env, root);
    }

    if let Some(supervisor_name) = supervisor_name {
        env.push((
            "BREHON_SUPERVISOR_NAME".to_string(),
            supervisor_name.to_string(),
        ));
    }
    if let Some(factory_worker_cli) = factory_worker_cli {
        env.push((
            "BREHON_FACTORY_WORKER_CLI".to_string(),
            factory_worker_cli.to_string(),
        ));
    }
    if let Some(model_name_override) = model_name_override {
        env.push(("KIMI_MODEL_NAME".to_string(), model_name_override));
        env.push((
            "KIMI_MODEL_MAX_CONTEXT_SIZE".to_string(),
            KIMI_DEFAULT_MAX_CONTEXT_SIZE.to_string(),
        ));
        env.push((
            "KIMI_MODEL_CAPABILITIES".to_string(),
            KIMI_DEFAULT_CAPABILITIES.to_string(),
        ));
        env.push((
            "KIMI_BASE_URL".to_string(),
            KIMI_DEFAULT_BASE_URL.to_string(),
        ));
    }

    let mut args = vec!["--work-dir".to_string(), cwd.to_string_lossy().to_string()];
    if allow_yolo {
        args.push("--yolo".to_string());
    }

    KimiSpawnConfig {
        command: "kimi".to_string(),
        args,
        env,
        cwd: Some(cwd),
        rows: 24,
        cols: 80,
    }
}

// =============================================================================
// KimiSession — ACP stdio session management
// =============================================================================

type PendingRequests = Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>;
type PromptResults = Arc<Mutex<HashMap<String, Result<AcpPromptResult, String>>>>;

#[derive(Debug, thiserror::Error)]
pub enum KimiError {
    #[error("failed to spawn kimi process: {0}")]
    Spawn(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out waiting for response to {0}")]
    Timeout(String),
    #[error("session not running")]
    #[allow(dead_code)]
    NotRunning,
}

pub struct KimiSessionInner {
    session_id: SessionId,
    spec: SessionSpec,
    capabilities: AgentCapabilities,
    process: Arc<Mutex<Option<AgentProcess>>>,
    event_tx: Mutex<Option<mpsc::Sender<AdapterEvent>>>,
    pending_requests: PendingRequests,
    prompt_results: PromptResults,
    prompt_wait_notify: Notify,
    active_prompt_ids: Mutex<HashSet<String>>,
    remote_session_id: Mutex<String>,
    fatal_error: Mutex<Option<String>>,
    alive: AtomicBool,
    shutdown: AtomicBool,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    log_watcher_handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct KimiSession {
    inner: Arc<KimiSessionInner>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl KimiSession {
    pub async fn spawn_with_env(
        spec: SessionSpec,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        event_tx: Option<mpsc::Sender<AdapterEvent>>,
    ) -> Result<Self, KimiError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();
        let log_path = kimi_share_dir_from_env(env).map(|share_dir| kimi_log_path(&share_dir));

        let process = AgentProcess::spawn_with_env(command, args, &spec.worktree_path, env)
            .await
            .map_err(|e| KimiError::Spawn(e.to_string()))?;
        let process = Arc::new(Mutex::new(Some(process)));
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let prompt_results = Arc::new(Mutex::new(HashMap::new()));

        let metadata = SessionMetadata {
            role: Some(spec.role.clone()),
            task_id: None,
            agent_id: Some(spec.agent_id.as_str().to_string()),
        };

        let init_response = send_request_sync(
            &process,
            create_initialize_request(&spec.worktree_path, Some(metadata)),
            KIMI_BOOTSTRAP_TIMEOUT,
        )
        .await?;

        if let Some(error) = init_response.error {
            return Err(KimiError::Protocol(describe_rpc_error(&error)));
        }

        let capabilities = acp_capabilities(init_response.result.as_ref());

        let new_session_response = send_request_sync(
            &process,
            create_new_session_request(&spec.worktree_path, parse_kimi_acp_mcp_servers(env)),
            KIMI_BOOTSTRAP_TIMEOUT,
        )
        .await?;

        if let Some(error) = new_session_response.error {
            return Err(KimiError::Protocol(describe_rpc_error(&error)));
        }

        let remote_session_id = parse_new_session_result(&new_session_response)
            .map_err(KimiError::Protocol)?
            .session_id;

        let inner = Arc::new(KimiSessionInner {
            session_id: session_id.clone(),
            spec: spec.clone(),
            capabilities,
            process: Arc::clone(&process),
            event_tx: Mutex::new(event_tx),
            pending_requests: Arc::clone(&pending_requests),
            prompt_results: Arc::clone(&prompt_results),
            prompt_wait_notify: Notify::new(),
            active_prompt_ids: Mutex::new(HashSet::new()),
            remote_session_id: Mutex::new(remote_session_id),
            fatal_error: Mutex::new(None),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
            log_watcher_handle: Mutex::new(None),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner));
        *inner.reader_handle.lock().await = Some(reader_handle);
        if let Some(log_path) = log_path {
            let log_watcher_handle = spawn_kimi_log_watcher(Arc::clone(&inner), log_path);
            *inner.log_watcher_handle.lock().await = Some(log_watcher_handle);
        }
        persist_session_snapshot(
            session_id.as_str(),
            brehon_types::StabilityCounters::default(),
        );

        Ok(Self { inner, created_at })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.inner.session_id
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities.clone()
    }

    pub fn session_info(&self) -> SessionInfo {
        let fatal = self
            .inner
            .fatal_error
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone())
            .is_some();
        SessionInfo {
            session_id: self.inner.session_id.clone(),
            agent_id: self.inner.spec.agent_id.clone(),
            role: self.inner.spec.role.clone(),
            health: if self.inner.alive.load(Ordering::SeqCst) && !fatal {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            created_at: self.created_at,
            capabilities: self.inner.capabilities.clone(),
        }
    }

    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        brehon_types::StabilityCounters {
            pending_requests: self.inner.pending_requests.lock().await.len(),
            pending_prompt_waiters: self.inner.prompt_results.lock().await.len(),
            ..Default::default()
        }
    }

    fn persist_runtime_stability(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            schedule_persist_session_snapshot(
                inner.session_id.as_str().to_string(),
                brehon_types::StabilityCounters {
                    pending_requests: inner.pending_requests.lock().await.len(),
                    pending_prompt_waiters: inner.prompt_results.lock().await.len(),
                    ..Default::default()
                },
            );
        });
    }

    pub async fn send_prompt(&self, prompt: PromptTurn) -> Result<PromptHandle, KimiError> {
        if let Some(error) = self.inner.fatal_error.lock().await.clone() {
            return Err(KimiError::Protocol(error));
        }
        let remote_session_id = self.inner.remote_session_id.lock().await.clone();
        let prompt_id = prompt.prompt_id.as_str().to_string();
        self.inner.prompt_results.lock().await.remove(&prompt_id);
        self.inner
            .active_prompt_ids
            .lock()
            .await
            .insert(prompt_id.clone());
        emit_kimi_turn_started(&self.inner).await;
        self.persist_runtime_stability();
        let request = create_prompt_request(&prompt_id, &remote_session_id, &prompt.content);
        let accepted = self
            .send_request_with_short_acceptance(request, KIMI_PROMPT_ACCEPT_TIMEOUT)
            .await;
        let response = match accepted {
            Ok(response) => response,
            Err(err) => {
                emit_kimi_turn_completed_if_active(&self.inner, &prompt_id, false).await;
                return Err(err);
            }
        };

        if let Some(response) = response {
            if let Some(error) = response.error {
                emit_kimi_turn_completed_if_active(&self.inner, &prompt_id, false).await;
                return Err(KimiError::Protocol(describe_rpc_error(&error)));
            }
            let prompt_result = match parse_prompt_result(&response) {
                Ok(result) => result,
                Err(err) => {
                    emit_kimi_turn_completed_if_active(&self.inner, &prompt_id, false).await;
                    return Err(KimiError::Protocol(err));
                }
            };
            self.inner
                .prompt_results
                .lock()
                .await
                .insert(prompt_id.clone(), Ok(prompt_result));
            self.inner.prompt_wait_notify.notify_waiters();
            self.persist_runtime_stability();
            emit_kimi_turn_completed_if_active(&self.inner, &prompt_id, true).await;
        }

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id,
            session_id: self.inner.session_id.as_str().to_string(),
            created_at: prompt.sent_at,
        })
    }

    #[cfg(test)]
    async fn wait_for_response_with_ready_signal(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
        ready: Arc<Notify>,
    ) -> Result<AcpPromptResult, KimiError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_id_str = prompt_id.as_str().to_string();

        match timeout(deadline, async {
            loop {
                if let Some(error) = self.inner.fatal_error.lock().await.clone() {
                    return Err(error);
                }
                let notified = self.inner.prompt_wait_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                ready.notify_waiters();
                if let Some(error) = self.inner.fatal_error.lock().await.clone() {
                    return Err(error);
                }
                if let Some(result) = self
                    .inner
                    .prompt_results
                    .lock()
                    .await
                    .remove(&prompt_id_str)
                {
                    return result;
                }
                notified.as_mut().await;
            }
        })
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(msg)) => Err(KimiError::Protocol(msg)),
            Err(_) => Err(KimiError::Timeout(format!(
                "timeout waiting for response to {prompt_id_str}"
            ))),
        }
    }

    pub async fn cancel_prompt(&self, _prompt_id: &PromptId) -> Result<(), KimiError> {
        let remote_session_id = self.inner.remote_session_id.lock().await.clone();
        let notification = create_cancel_notification(&remote_session_id);
        let line =
            serialize_notification(&notification).map_err(|e| KimiError::Protocol(e.message))?;

        let process = self.inner.process.lock().await;
        if let Some(process) = process.as_ref() {
            process
                .send_line(&line)
                .await
                .map_err(|e| KimiError::Spawn(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn set_config(&self, option: &str, value: &str) -> Result<(), KimiError> {
        let remote_session_id = self.inner.remote_session_id.lock().await.clone();
        let method = match option {
            "mode" => "session/set_mode",
            "model" => "session/set_model",
            _ => return Ok(()),
        };
        let key = if option == "mode" {
            "modeId"
        } else {
            "modelId"
        };
        let request = JsonRpcRequest::new(
            method,
            Some(serde_json::json!({
                "sessionId": remote_session_id,
                key: value,
            })),
        );
        let response = self.send_request(request).await?;
        if let Some(error) = response.error {
            return Err(KimiError::Protocol(describe_rpc_error(&error)));
        }
        Ok(())
    }

    pub async fn kill(&self) -> Result<(), KimiError> {
        self.inner.alive.store(false, Ordering::SeqCst);
        self.inner.shutdown.store(true, Ordering::SeqCst);

        let process = {
            let mut guard = self.inner.process.lock().await;
            guard.take()
        };
        let result = if let Some(process) = process {
            process
                .kill()
                .await
                .map_err(|e| KimiError::Spawn(e.to_string()))
        } else {
            Ok(())
        };

        if let Some(handle) = self.inner.reader_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }
        if let Some(handle) = self.inner.log_watcher_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        }

        clear_session_snapshot(self.inner.session_id.as_str());
        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, KimiError> {
        let process = self.inner.process.lock().await;
        Ok(
            if process.as_ref().map(|p| p.is_alive()).unwrap_or(false)
                && self.inner.alive.load(Ordering::SeqCst)
                && self.inner.fatal_error.lock().await.is_none()
            {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
        )
    }

    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<AcpPromptResult, KimiError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_id_str = prompt_id.as_str().to_string();

        match timeout(deadline, async {
            loop {
                if let Some(error) = self.inner.fatal_error.lock().await.clone() {
                    return Err(error);
                }
                let notified = self.inner.prompt_wait_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                if let Some(error) = self.inner.fatal_error.lock().await.clone() {
                    return Err(error);
                }
                if let Some(result) = self
                    .inner
                    .prompt_results
                    .lock()
                    .await
                    .remove(&prompt_id_str)
                {
                    return result;
                }
                notified.as_mut().await;
            }
        })
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(msg)) => Err(KimiError::Protocol(msg)),
            Err(_) => Err(KimiError::Timeout(format!(
                "timeout waiting for response to {prompt_id_str}"
            ))),
        }
    }

    pub fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = mpsc::channel(128);
        if let Ok(mut guard) = self.inner.event_tx.try_lock() {
            *guard = Some(tx);
        }
        rx
    }

    async fn send_request(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, KimiError> {
        let line = serialize_request(&request).map_err(|e| KimiError::Protocol(e.message))?;
        let request_id = request.id.clone();
        let method = request.method.clone();
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending_requests
            .lock()
            .await
            .insert(request_id.clone(), tx);
        self.persist_runtime_stability();

        {
            let process = self.inner.process.lock().await;
            if let Some(process) = process.as_ref() {
                if let Err(err) = process.send_line(&line).await {
                    self.inner.pending_requests.lock().await.remove(&request_id);
                    self.persist_runtime_stability();
                    return Err(KimiError::Spawn(err.to_string()));
                }
            } else {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                return Err(KimiError::Spawn("process not running".to_string()));
            }
        }

        match timeout(KIMI_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(KimiError::Protocol(format!(
                "Kimi request channel closed for {request_id}"
            ))),
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                Err(KimiError::Protocol(format!(
                    "Process timeout waiting for Kimi response to {method}"
                )))
            }
        }
    }

    async fn send_request_with_short_acceptance(
        &self,
        request: JsonRpcRequest,
        accept_timeout: Duration,
    ) -> Result<Option<JsonRpcResponse>, KimiError> {
        let line = serialize_request(&request).map_err(|e| KimiError::Protocol(e.message))?;
        let request_id = request.id.clone();
        let method = request.method.clone();
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending_requests
            .lock()
            .await
            .insert(request_id.clone(), tx);
        self.persist_runtime_stability();

        {
            let process = self.inner.process.lock().await;
            if let Some(process) = process.as_ref() {
                if let Err(err) = process.send_line(&line).await {
                    self.inner.pending_requests.lock().await.remove(&request_id);
                    self.persist_runtime_stability();
                    return Err(KimiError::Spawn(err.to_string()));
                }
            } else {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                return Err(KimiError::Spawn("process not running".to_string()));
            }
        }

        match timeout(accept_timeout, rx).await {
            Ok(Ok(response)) => Ok(Some(response)),
            Ok(Err(_)) => Err(KimiError::Protocol(format!(
                "Kimi request channel closed for {request_id}"
            ))),
            Err(_) => {
                self.inner.pending_requests.lock().await.remove(&request_id);
                self.persist_runtime_stability();
                debug!(request_id = %request_id, method = %method, "Kimi prompt accepted without immediate response");
                Ok(None)
            }
        }
    }
}

async fn send_request_sync(
    process: &Arc<Mutex<Option<AgentProcess>>>,
    request: JsonRpcRequest,
    timeout_duration: Duration,
) -> Result<JsonRpcResponse, KimiError> {
    let line = serialize_request(&request).map_err(|e| KimiError::Protocol(e.message))?;
    let request_id = request.id.clone();
    let mut process = process.lock().await;
    let process = process.as_mut().ok_or_else(|| {
        KimiError::Spawn("Kimi process not available for sync request".to_string())
    })?;
    process
        .send_line(&line)
        .await
        .map_err(|e| KimiError::Spawn(e.to_string()))?;

    loop {
        let line = process
            .recv_line(timeout_duration.as_millis() as u64)
            .await
            .map_err(|e| KimiError::Protocol(e.to_string()))?
            .ok_or_else(|| {
                KimiError::Protocol("Kimi process exited during bootstrap".to_string())
            })?;
        if line.is_empty() {
            continue;
        }
        match parse_kimi_message(&line) {
            Ok(KimiParsedMessage::Typed(JsonRpcMessage::Response(response)))
                if response.id == request_id =>
            {
                return Ok(response);
            }
            Ok(KimiParsedMessage::Typed(JsonRpcMessage::Notification(_))) => continue,
            Ok(KimiParsedMessage::Typed(JsonRpcMessage::Response(_))) => continue,
            Ok(KimiParsedMessage::Typed(JsonRpcMessage::Request(request))) => {
                let response = response_for_kimi_server_request(&request);
                let line =
                    serialize_response(&response).map_err(|e| KimiError::Protocol(e.message))?;
                process
                    .send_line(&line)
                    .await
                    .map_err(|e| KimiError::Spawn(e.to_string()))?;
                continue;
            }
            Ok(KimiParsedMessage::RawServerRequest(request)) => {
                let line = serialize_raw_response(&response_for_raw_kimi_server_request(&request))
                    .map_err(|e| KimiError::Protocol(e.message))?;
                process
                    .send_line(&line)
                    .await
                    .map_err(|e| KimiError::Spawn(e.to_string()))?;
                continue;
            }
            Err(err) => {
                return Err(KimiError::Protocol(format!(
                    "Failed to parse Kimi bootstrap response: {}",
                    err.message
                )));
            }
        }
    }
}

async fn emit_adapter_event(inner: &Arc<KimiSessionInner>, event: AdapterEvent) {
    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(event).await;
    }
}

async fn emit_kimi_turn_started(inner: &Arc<KimiSessionInner>) {
    emit_adapter_event(
        inner,
        AdapterEvent::OperationStarted {
            operation: "turn".to_string(),
        },
    )
    .await;
}

async fn emit_kimi_turn_completed_if_active(
    inner: &Arc<KimiSessionInner>,
    prompt_id: &str,
    success: bool,
) -> bool {
    let was_active = inner.active_prompt_ids.lock().await.remove(prompt_id);
    if was_active {
        emit_adapter_event(
            inner,
            AdapterEvent::OperationCompleted {
                operation: "turn".to_string(),
                success,
            },
        )
        .await;
    }
    was_active
}

fn spawn_reader(inner: Arc<KimiSessionInner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if inner.shutdown.load(Ordering::SeqCst) {
                debug!(session_id = %inner.session_id, "Kimi reader exiting due to shutdown signal");
                break;
            }

            let next = {
                let mut process = inner.process.lock().await;
                if let Some(process) = process.as_mut() {
                    process.recv_line(100).await
                } else {
                    break;
                }
            };

            let line = match next {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(brehon_adapter_sdk::process::ProcessError::Timeout) => {
                    if !inner.alive.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                Err(err) => {
                    warn!(error = %err, "Kimi ACP reader failed");
                    break;
                }
            };

            if line.is_empty() {
                continue;
            }

            match parse_kimi_message(&line) {
                Ok(KimiParsedMessage::Typed(JsonRpcMessage::Response(response))) => {
                    let request_id = response.id.clone();
                    let pending_tx = inner.pending_requests.lock().await.remove(&request_id);
                    if let Some(tx) = pending_tx {
                        match tx.send(response) {
                            Ok(()) => {}
                            Err(response) => {
                                // Receiver dropped (short-accept timeout), store as orphaned result
                                let prompt_result = if let Some(rpc_error) = response.error {
                                    Err(format!(
                                        "RPC error {}: {}",
                                        rpc_error.code, rpc_error.message
                                    ))
                                } else {
                                    parse_prompt_result(&response).map_err(|err| {
                                        format!("Failed to parse Kimi prompt result: {err}")
                                    })
                                };
                                if let Err(ref err) = prompt_result {
                                    warn!(response_id = %request_id, error = %err, "Kimi orphaned prompt response error");
                                }
                                let success = prompt_result.is_ok();
                                inner
                                    .prompt_results
                                    .lock()
                                    .await
                                    .insert(request_id.clone(), prompt_result);
                                inner.prompt_wait_notify.notify_waiters();
                                emit_kimi_turn_completed_if_active(&inner, &request_id, success)
                                    .await;
                            }
                        }
                    } else {
                        let prompt_result = if let Some(rpc_error) = response.error {
                            Err(format!(
                                "RPC error {}: {}",
                                rpc_error.code, rpc_error.message
                            ))
                        } else {
                            parse_prompt_result(&response)
                                .map_err(|err| format!("Failed to parse Kimi prompt result: {err}"))
                        };
                        if let Err(ref err) = prompt_result {
                            warn!(response_id = %request_id, error = %err, "Kimi prompt response error");
                        }
                        let success = prompt_result.is_ok();
                        inner
                            .prompt_results
                            .lock()
                            .await
                            .insert(request_id.clone(), prompt_result);
                        inner.prompt_wait_notify.notify_waiters();
                        emit_kimi_turn_completed_if_active(&inner, &request_id, success).await;
                    }
                    let pending_requests_len = inner.pending_requests.lock().await.len();
                    let pending_prompt_waiters_len = inner.prompt_results.lock().await.len();
                    schedule_persist_session_snapshot(
                        inner.session_id.as_str().to_string(),
                        brehon_types::StabilityCounters {
                            pending_requests: pending_requests_len,
                            pending_prompt_waiters: pending_prompt_waiters_len,
                            ..Default::default()
                        },
                    );
                }
                Ok(KimiParsedMessage::Typed(JsonRpcMessage::Notification(notification))) => {
                    forward_kimi_notification(
                        &inner,
                        &notification.method,
                        notification.params.as_ref(),
                    )
                    .await;
                }
                Ok(KimiParsedMessage::Typed(JsonRpcMessage::Request(request))) => {
                    respond_to_kimi_server_request(&inner, request).await;
                }
                Ok(KimiParsedMessage::RawServerRequest(request)) => {
                    respond_to_raw_kimi_server_request(&inner, request).await;
                }
                Err(err) => {
                    warn!(error = ?err, raw = %line, "Failed to parse Kimi ACP line");
                }
            }
        }

        inner.alive.store(false, Ordering::SeqCst);
        schedule_clear_session_snapshot(inner.session_id.as_str().to_string());
    })
}

fn spawn_kimi_log_watcher(inner: Arc<KimiSessionInner>, log_path: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut offset = std::fs::metadata(&log_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        loop {
            if inner.shutdown.load(Ordering::SeqCst)
                || !inner.alive.load(Ordering::SeqCst)
                || inner.fatal_error.lock().await.is_some()
            {
                break;
            }

            match std::fs::File::open(&log_path) {
                Ok(mut file) => {
                    let file_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                    if file_len < offset {
                        offset = 0;
                    }
                    if file_len > offset && file.seek(SeekFrom::Start(offset)).is_ok() {
                        let mut reader = BufReader::new(file);
                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) => break,
                                Ok(_) => {
                                    let trimmed =
                                        line.trim_end_matches('\n').trim_end_matches('\r');
                                    if let Some(message) =
                                        kimi_fatal_provider_error_message(trimmed)
                                    {
                                        mark_kimi_session_fatal(&inner, message).await;
                                        return;
                                    }
                                }
                                Err(err) => {
                                    warn!(error = %err, path = %log_path.display(), "Failed to read Kimi log");
                                    break;
                                }
                            }
                        }
                        offset = reader.stream_position().unwrap_or(file_len);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    debug!(error = %err, path = %log_path.display(), "Kimi log watcher could not open log file");
                }
            }

            tokio::time::sleep(KIMI_LOG_POLL_INTERVAL).await;
        }
    })
}

async fn mark_kimi_session_fatal(inner: &Arc<KimiSessionInner>, message: String) {
    {
        let mut fatal = inner.fatal_error.lock().await;
        if fatal.is_some() {
            return;
        }
        *fatal = Some(message.clone());
    }
    inner.alive.store(false, Ordering::SeqCst);
    inner.pending_requests.lock().await.clear();
    let active_prompt_count = {
        let mut active_prompt_ids = inner.active_prompt_ids.lock().await;
        let count = active_prompt_ids.len();
        active_prompt_ids.clear();
        count
    };
    inner.prompt_wait_notify.notify_waiters();
    schedule_clear_session_snapshot(inner.session_id.as_str().to_string());

    emit_adapter_event(
        inner,
        AdapterEvent::Progress {
            message: message.clone(),
            percent: None,
        },
    )
    .await;
    if active_prompt_count > 0 {
        emit_adapter_event(
            inner,
            AdapterEvent::OperationCompleted {
                operation: "turn".to_string(),
                success: false,
            },
        )
        .await;
    }
    emit_adapter_event(
        inner,
        AdapterEvent::OperationCompleted {
            operation: "kimi provider/runtime".to_string(),
            success: false,
        },
    )
    .await;
    warn!(session_id = %inner.session_id, error = %message, "Kimi session marked fatal");
}

#[derive(Debug, Clone)]
enum KimiParsedMessage {
    Typed(JsonRpcMessage),
    RawServerRequest(KimiRawServerRequest),
}

#[derive(Debug, Clone)]
struct KimiRawServerRequest {
    id: serde_json::Value,
    method: String,
    params: Option<serde_json::Value>,
}

fn parse_kimi_message(line: &str) -> Result<KimiParsedMessage, JsonRpcError> {
    if let Some(request) = parse_raw_kimi_server_request(line) {
        return Ok(KimiParsedMessage::RawServerRequest(request));
    }
    match parse_message(line) {
        Ok(message) => Ok(KimiParsedMessage::Typed(message)),
        Err(err) => Err(err),
    }
}

fn parse_raw_kimi_server_request(line: &str) -> Option<KimiRawServerRequest> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let object = value.as_object()?;
    let method = object.get("method")?.as_str()?.to_string();
    let id = object.get("id")?.clone();
    if !(id.is_string() || id.is_number()) {
        return None;
    }
    Some(KimiRawServerRequest {
        id,
        method,
        params: object.get("params").cloned(),
    })
}

fn serialize_response(response: &JsonRpcResponse) -> Result<String, JsonRpcError> {
    serde_json::to_string(response).map_err(|err| JsonRpcError {
        code: -32603,
        message: err.to_string(),
        data: None,
    })
}

fn serialize_raw_response(response: &serde_json::Value) -> Result<String, JsonRpcError> {
    serde_json::to_string(response).map_err(|err| JsonRpcError {
        code: -32603,
        message: err.to_string(),
        data: None,
    })
}

async fn respond_to_kimi_server_request(inner: &Arc<KimiSessionInner>, request: JsonRpcRequest) {
    let response = response_for_kimi_server_request(&request);
    let line = match serialize_response(&response) {
        Ok(line) => line,
        Err(err) => {
            warn!(error = %err.message, "Failed to serialize Kimi server request response");
            return;
        }
    };

    let process = inner.process.lock().await;
    let Some(process) = process.as_ref() else {
        warn!(method = %request.method, request_id = %request.id, "Kimi process unavailable while answering server request");
        return;
    };

    if let Err(err) = process.send_line(&line).await {
        warn!(error = %err, method = %request.method, request_id = %request.id, "Failed to answer Kimi server request");
    }
}

async fn respond_to_raw_kimi_server_request(
    inner: &Arc<KimiSessionInner>,
    request: KimiRawServerRequest,
) {
    let response = response_for_raw_kimi_server_request(&request);
    let line = match serialize_raw_response(&response) {
        Ok(line) => line,
        Err(err) => {
            warn!(error = %err.message, "Failed to serialize raw Kimi server request response");
            return;
        }
    };

    let process = inner.process.lock().await;
    let Some(process) = process.as_ref() else {
        warn!(method = %request.method, request_id = ?request.id, "Kimi process unavailable while answering raw server request");
        return;
    };

    if let Err(err) = process.send_line(&line).await {
        warn!(error = %err, method = %request.method, request_id = ?request.id, "Failed to answer raw Kimi server request");
    }
}

fn response_for_kimi_server_request(request: &JsonRpcRequest) -> JsonRpcResponse {
    if is_kimi_permission_request(request) {
        debug!(method = %request.method, request_id = %request.id, "Auto-approving Kimi permission request");
        return JsonRpcResponse::success(
            request.id.clone(),
            kimi_permission_response_for_approved(request.params.as_ref()),
        );
    }

    debug!(method = %request.method, request_id = %request.id, "Rejecting unsupported Kimi server request");
    JsonRpcResponse::error(request.id.clone(), JsonRpcError::method_not_found())
}

fn response_for_raw_kimi_server_request(request: &KimiRawServerRequest) -> serde_json::Value {
    if is_kimi_permission_method(&request.method) {
        debug!(method = %request.method, request_id = ?request.id, "Auto-approving raw Kimi permission request");
        return serde_json::json!({
            "jsonrpc": "2.0",
            "id": request.id.clone(),
            "result": kimi_permission_response_for_approved(request.params.as_ref()),
        });
    }

    debug!(method = %request.method, request_id = ?request.id, "Rejecting unsupported raw Kimi server request");
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.id.clone(),
        "error": {
            "code": -32601,
            "message": "Method not found",
        },
    })
}

fn is_kimi_permission_request(request: &JsonRpcRequest) -> bool {
    is_kimi_permission_method(&request.method)
}

fn is_kimi_permission_method(method: &str) -> bool {
    matches!(method, "requestPermission" | "session/request_permission")
}

fn kimi_permission_response_for_approved(params: Option<&serde_json::Value>) -> serde_json::Value {
    let selected = params
        .and_then(|params| params.get("options"))
        .and_then(serde_json::Value::as_array)
        .and_then(|options| select_kimi_approval_option(options));

    match selected {
        Some(option_id) => serde_json::json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
            }
        }),
        None => serde_json::json!({
            "outcome": {
                "outcome": "cancelled",
            }
        }),
    }
}

fn select_kimi_approval_option(options: &[serde_json::Value]) -> Option<String> {
    options
        .iter()
        .find_map(|option| {
            let kind = option.get("kind").and_then(serde_json::Value::as_str);
            let name = option.get("name").and_then(serde_json::Value::as_str);
            let is_approval = kind
                .map(|kind| kind.starts_with("allow_") || kind == "allow" || kind == "approve")
                .unwrap_or(false)
                || name
                    .map(|name| {
                        let name = name.to_ascii_lowercase();
                        name.contains("approve") || name.contains("allow")
                    })
                    .unwrap_or(false);
            if is_approval {
                option
                    .get("optionId")
                    .or_else(|| option.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            } else {
                None
            }
        })
        .or_else(|| {
            options.iter().find_map(|option| {
                option
                    .get("optionId")
                    .or_else(|| option.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
        })
}

async fn forward_kimi_notification(
    inner: &Arc<KimiSessionInner>,
    method: &str,
    params: Option<&serde_json::Value>,
) {
    if method != "session/update" {
        return;
    }
    let Some(update) = params.and_then(|params| params.get("update")) else {
        return;
    };

    let event = match normalize_session_update_value(&inner.session_id, update) {
        Ok(Some(event)) => event,
        Ok(None) => return,
        Err(err) => {
            warn!(error = %err, "Failed to normalize Kimi session update");
            return;
        }
    };

    let adapter_event =
        session_event_to_adapter_event(event).unwrap_or_else(|| AdapterEvent::Progress {
            message: "permission resolved".to_string(),
            percent: None,
        });
    let event_tx = inner.event_tx.lock().await.clone();
    if let Some(tx) = event_tx {
        let _ = tx.send(adapter_event).await;
    }
}

fn acp_capabilities(result: Option<&serde_json::Value>) -> AgentCapabilities {
    let prompt_capabilities = result
        .and_then(|result| result.get("agentCapabilities"))
        .and_then(|caps| caps.get("promptCapabilities"));

    let mut content_block_types = vec!["text".to_string()];
    if prompt_capabilities
        .and_then(|caps| caps.get("image"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        content_block_types.push("image".to_string());
    }
    if prompt_capabilities
        .and_then(|caps| caps.get("audio"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        content_block_types.push("audio".to_string());
    }

    AgentCapabilities {
        content_block_types,
        session_config_options: vec!["mode".to_string(), "model".to_string()],
        permission_support: true,
        terminal_support: false,
        tool_call_streaming: ToolCallStreaming::Basic,
    }
}

fn describe_rpc_error(error: &brehon_adapter_sdk::protocol::JsonRpcError) -> String {
    match &error.data {
        Some(data) => format!("{}: {}", error.message, data),
        None => error.message.clone(),
    }
}

fn acp_prompt_result_to_sdk(result: AcpPromptResult) -> PromptResult {
    let mut pr = PromptResult::default();
    pr.response = result.response;
    pr.tokens_used = result.tokens_used;
    pr.stop_reason = result.stop_reason;
    pr
}

fn kimi_error_to_adapter_error(err: KimiError) -> AdapterError {
    match err {
        KimiError::Spawn(msg) => AdapterError::spawn_failed(msg),
        KimiError::Timeout(msg) => AdapterError::timed_out(msg),
        KimiError::Protocol(msg) => AdapterError::send_failed(msg),
        KimiError::NotRunning => AdapterError::transport_closed("session not running"),
    }
}

// =============================================================================
// KimiAdapter — AgentAdapter implementation
// =============================================================================

/// Configuration for spawning a Kimi adapter session.
#[derive(Clone, Debug)]
pub struct KimiConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Adapter implementation for the Kimi CLI.
pub struct KimiAdapter {
    config: KimiConfig,
    session: RwLock<Option<Arc<KimiSession>>>,
    event_broadcast: tokio::sync::broadcast::Sender<AdapterEvent>,
}

impl KimiAdapter {
    pub fn new(config: KimiConfig) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(256);
        Self {
            config,
            session: RwLock::new(None),
            event_broadcast: tx,
        }
    }
}

#[async_trait]
impl AgentAdapter for KimiAdapter {
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId> {
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let broadcast_tx = self.event_broadcast.clone();

        tokio::spawn(async move {
            while let Some(adapter_event) = event_rx.recv().await {
                let _ = broadcast_tx.send(adapter_event);
            }
        });

        let session = KimiSession::spawn_with_env(
            spec,
            &self.config.command,
            &self.config.args,
            &self.config.env,
            Some(event_tx),
        )
        .await
        .map_err(kimi_error_to_adapter_error)?;

        let session = Arc::new(session);
        let session_id = session.session_id().clone();
        *self.session.write().await = Some(session);
        Ok(session_id)
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        session
            .send_prompt(prompt)
            .await
            .map_err(kimi_error_to_adapter_error)
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        let result = session
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(kimi_error_to_adapter_error)?;
        Ok(acp_prompt_result_to_sdk(result))
    }

    fn events(&self) -> mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = mpsc::channel(256);
        let mut broadcast_rx = self.event_broadcast.subscribe();
        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        let session = {
            let mut guard = self.session.write().await;
            guard.take()
        };
        if let Some(session) = session {
            session.kill().await.map_err(kimi_error_to_adapter_error)?;
        }
        Ok(())
    }

    fn kind(&self) -> brehon_types::AdapterKind {
        brehon_types::AdapterKind::Acp
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        Ok(session.capabilities())
    }

    async fn session_id(&self) -> SessionId {
        let session = self.session.read().await;
        session
            .as_ref()
            .map(|s| s.session_id().clone())
            .unwrap_or_else(|| SessionId::new("kimi-unknown"))
    }

    async fn session_info(&self) -> SessionInfo {
        let session = self.session.read().await;
        session
            .as_ref()
            .map(|s| s.session_info())
            .unwrap_or_else(|| SessionInfo {
                session_id: SessionId::new("kimi-unknown"),
                agent_id: brehon_types::AgentId::new("kimi"),
                role: "worker".to_string(),
                health: HealthStatus::Unknown,
                created_at: chrono::Utc::now(),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec![],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: ToolCallStreaming::None,
                },
            })
    }

    async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        let session = {
            let guard = self.session.read().await;
            guard.as_ref().cloned()
        };
        if let Some(session) = session {
            session.stability_counters().await
        } else {
            brehon_types::StabilityCounters::default()
        }
    }

    async fn set_config(&self, option: &str, value: &str) -> AdapterResult<()> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        session
            .set_config(option, value)
            .await
            .map_err(kimi_error_to_adapter_error)
    }

    async fn cancel_prompt(&self, prompt: &PromptId) -> AdapterResult<()> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        session
            .cancel_prompt(prompt)
            .await
            .map_err(kimi_error_to_adapter_error)
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        let session = {
            let guard = self.session.read().await;
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| AdapterError::transport_closed("session not spawned"))?
        };
        session
            .health_check()
            .await
            .map_err(kimi_error_to_adapter_error)
    }

    async fn attach_terminal(&self, _cols: u16, _rows: u16) -> AdapterResult<Option<TerminalId>> {
        Ok(None)
    }

    async fn send_terminal_input(
        &self,
        _terminal: &TerminalId,
        _input: Vec<u8>,
    ) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Terminal input is not supported for Kimi sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for Kimi sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_adapter_sdk::AdapterErrorKind;
    use tokio::time::timeout;

    #[test]
    fn test_acp_capabilities_from_prompt_capabilities() {
        let result = serde_json::json!({
            "agentCapabilities": {
                "promptCapabilities": {
                    "image": true,
                    "audio": true
                }
            }
        });

        let caps = acp_capabilities(Some(&result));
        assert_eq!(caps.content_block_types, vec!["text", "image", "audio"]);
        assert!(caps.permission_support);
        assert!(!caps.terminal_support);
    }

    #[test]
    fn test_build_minimal_kimi_config() {
        let config =
            build_minimal_kimi_config("kimi-code/kimi-for-coding", "kimi-for-coding", true, false);
        assert!(config.contains("default_model = \"kimi-code/kimi-for-coding\""));
        assert!(config.contains("default_thinking = true"));
        assert!(config.contains("default_yolo = false"));
        assert!(config.contains(&format!(
            "max_context_size = {KIMI_DEFAULT_MAX_CONTEXT_SIZE}"
        )));
        assert!(config.contains(&format!(
            "reserved_context_size = {KIMI_RESERVED_CONTEXT_SIZE}"
        )));
        assert!(config.contains(&format!(
            "compaction_trigger_ratio = {KIMI_COMPACTION_TRIGGER_RATIO}"
        )));
        assert!(config.contains(&format!(
            "tool_call_timeout_ms = {KIMI_TOOL_CALL_TIMEOUT_MS}"
        )));

        let config =
            build_minimal_kimi_config("kimi-code/kimi-for-coding", "kimi-for-coding", true, true);
        assert!(config.contains("default_yolo = true"));
    }

    #[test]
    fn test_kimi_mcp_env_inherits_worktree_root_for_cleanup() {
        let mut env = Vec::new();
        push_inherited_worktree_root_env(
            &mut env,
            Some("  /Volumes/PortableSSD/brehon/worktrees/lorecourt  "),
        );

        assert_eq!(
            env,
            vec![(
                "BREHON_WORKTREE_ROOT".to_string(),
                "/Volumes/PortableSSD/brehon/worktrees/lorecourt".to_string()
            )]
        );
    }

    #[test]
    fn test_kimi_fatal_provider_error_detects_missing_tool_response() {
        let line = "2026-05-31 16:54:10.792 | ERROR | kimi_cli.soul.kimisoul:_agent_loop:926 | session - Agent step 17 failed: APIStatusError: Error code: 400 - {'error': {'message': \"an assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id'. The following tool_call_ids did not have response messages: Shell:25\", 'type': 'invalid_request_error'}}";

        let message = kimi_fatal_provider_error_message(line).expect("fatal message");

        assert!(message.contains("Kimi provider/runtime failure"));
        assert!(message.contains("tool_calls"));
        assert!(message.contains("Shell:25"));
    }

    #[test]
    fn test_kimi_fatal_provider_error_detects_context_limit_rejection() {
        let line = "2026-05-31 18:54:10.792 | ERROR | kimi_cli.soul.kimisoul:_agent_loop:926 | session - Agent step 19 failed: APIStatusError: Error code: 400 - {'error': {'message': 'This model maximum context length is 262144 tokens. However, your messages resulted in 266120 tokens. Please reduce the length of the messages.', 'type': 'invalid_request_error', 'code': 'context_length_exceeded'}}";

        let message = kimi_fatal_provider_error_message(line).expect("fatal message");

        assert!(message.contains("Kimi provider/runtime failure"));
        assert!(message.contains("maximum context length"));
        assert!(message.contains("266120"));
    }

    #[test]
    fn test_kimi_fatal_provider_error_ignores_nonfatal_log_line() {
        assert!(kimi_fatal_provider_error_message(
            "2026-05-31 16:54:10.792 | INFO | kimi_cli - normal progress"
        )
        .is_none());
    }

    #[tokio::test]
    async fn test_kimi_turn_events_complete_only_matching_active_prompt() {
        let (tx, mut rx) = mpsc::channel(4);
        let inner = Arc::new(KimiSessionInner {
            session_id: SessionId::new("test-session"),
            spec: SessionSpec::new(
                brehon_types::AgentId::new("kimi-test"),
                "worker".to_string(),
                "/tmp".to_string(),
            ),
            capabilities: AgentCapabilities {
                content_block_types: vec!["text".to_string()],
                session_config_options: vec![],
                permission_support: false,
                terminal_support: false,
                tool_call_streaming: ToolCallStreaming::None,
            },
            process: Arc::new(Mutex::new(None)),
            event_tx: Mutex::new(Some(tx)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            prompt_results: Arc::new(Mutex::new(HashMap::new())),
            prompt_wait_notify: Notify::new(),
            active_prompt_ids: Mutex::new(HashSet::new()),
            remote_session_id: Mutex::new("remote-session-id".to_string()),
            fatal_error: Mutex::new(None),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
            log_watcher_handle: Mutex::new(None),
        });
        inner
            .active_prompt_ids
            .lock()
            .await
            .insert("prompt-1".to_string());

        emit_kimi_turn_started(&inner).await;
        assert!(emit_kimi_turn_completed_if_active(&inner, "prompt-1", true).await);
        assert!(!emit_kimi_turn_completed_if_active(&inner, "prompt-1", true).await);

        assert_eq!(
            rx.recv().await,
            Some(AdapterEvent::OperationStarted {
                operation: "turn".to_string()
            })
        );
        assert_eq!(
            rx.recv().await,
            Some(AdapterEvent::OperationCompleted {
                operation: "turn".to_string(),
                success: true
            })
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_upsert_toml_assignment() {
        let content = "foo = \"bar\"\n";
        let updated = upsert_toml_assignment(content, "foo", "\"baz\"");
        assert!(updated.contains("foo = \"baz\""));

        let updated = upsert_toml_assignment(content, "qux", "\"quux\"");
        assert!(updated.contains("qux = \"quux\""));
        assert!(updated.contains("foo = \"bar\""));
    }

    #[test]
    fn test_patch_existing_kimi_config_lowers_selected_model_context_ceiling() {
        let content = r#"
default_model = "kimi-code/kimi-for-coding"
default_thinking = false
default_yolo = false

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
max_context_size = 262144
capabilities = ["thinking"]

[models."other/model"]
provider = "managed:kimi-code"
model = "other"
max_context_size = 262144
"#;

        let patched =
            patch_existing_kimi_config(content, "kimi-code/kimi-for-coding", Some("high"), false);

        assert!(patched.contains(&format!(
            "[models.\"kimi-code/kimi-for-coding\"]\nprovider = \"managed:kimi-code\"\nmodel = \"kimi-for-coding\"\nmax_context_size = {KIMI_DEFAULT_MAX_CONTEXT_SIZE}"
        )));
        assert!(patched.contains(&format!(
            "[mcp.client]\ntool_call_timeout_ms = {KIMI_TOOL_CALL_TIMEOUT_MS}"
        )));
        assert!(patched.contains(&format!(
            "[loop_control]\nreserved_context_size = {KIMI_RESERVED_CONTEXT_SIZE}\ncompaction_trigger_ratio = {KIMI_COMPACTION_TRIGGER_RATIO}"
        )));
        assert!(patched.contains("[models.\"other/model\"]\nprovider = \"managed:kimi-code\"\nmodel = \"other\"\nmax_context_size = 262144"));
    }

    #[test]
    fn test_kimi_adapter_kind_is_acp() {
        let adapter = KimiAdapter::new(KimiConfig {
            command: "kimi".to_string(),
            args: vec![],
            env: vec![],
        });
        assert_eq!(adapter.kind(), brehon_types::AdapterKind::Acp);
    }

    #[test]
    fn test_parse_kimi_acp_mcp_servers_reads_env_payload() {
        let payload = serde_json::to_string(&vec![serde_json::json!({
            "name": "brehon",
            "command": "/tmp/brehon",
            "args": ["serve"]
        })])
        .unwrap();
        let servers =
            parse_kimi_acp_mcp_servers(&[("BREHON_ACP_MCP_SERVERS_JSON".to_string(), payload)]);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "brehon");
        assert_eq!(servers[0]["command"], "/tmp/brehon");
    }

    #[test]
    fn test_kimi_permission_response_prefers_allow_option() {
        let request = JsonRpcRequest::new_with_id(
            "perm-1",
            "session/request_permission",
            Some(serde_json::json!({
                "options": [
                    {"kind": "reject_once", "name": "Reject", "optionId": "reject"},
                    {"kind": "allow_once", "name": "Approve once", "optionId": "approve"},
                    {"kind": "allow_always", "name": "Approve for this session", "optionId": "approve_for_session"}
                ]
            })),
        );

        let response = response_for_kimi_server_request(&request);

        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["outcome"]["optionId"], "approve");
    }

    #[test]
    fn test_kimi_permission_response_falls_back_to_first_option_id() {
        let response = kimi_permission_response_for_approved(Some(&serde_json::json!({
            "options": [
                {"kind": "custom", "optionId": "custom-first"},
                {"kind": "custom", "optionId": "custom-second"}
            ]
        })));

        assert_eq!(response["outcome"]["optionId"], "custom-first");
    }

    #[test]
    fn test_kimi_raw_permission_response_preserves_numeric_request_id() {
        let parsed = parse_kimi_message(
            r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission","params":{"options":[{"kind":"allow_once","optionId":"approve"}]}}"#,
        )
        .unwrap();

        let KimiParsedMessage::RawServerRequest(request) = parsed else {
            panic!("expected raw server request");
        };
        let response = response_for_raw_kimi_server_request(&request);

        assert_eq!(response["id"], 0);
        assert_eq!(response["result"]["outcome"]["optionId"], "approve");
    }

    #[test]
    fn test_kimi_unknown_server_request_returns_method_not_found() {
        let request = JsonRpcRequest::new_with_id("req-1", "unknown/method", None);

        let response = response_for_kimi_server_request(&request);

        assert!(response.result.is_none());
        let err = response.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn test_kimi_error_to_adapter_error_marks_timeouts() {
        let adapter_error = kimi_error_to_adapter_error(KimiError::Timeout(
            "timeout waiting for response".to_string(),
        ));
        assert_eq!(
            adapter_error,
            AdapterError {
                kind: AdapterErrorKind::TimedOut,
                message: "timeout waiting for response".to_string(),
            }
        );
    }

    fn build_dummy_kimi_session() -> KimiSession {
        KimiSession {
            inner: Arc::new(KimiSessionInner {
                session_id: SessionId::new("dummy-session"),
                spec: brehon_types::SessionSpec::new(
                    brehon_types::AgentId::new("kimi"),
                    "worker".to_string(),
                    "/tmp".to_string(),
                ),
                capabilities: AgentCapabilities {
                    content_block_types: vec!["text".to_string()],
                    session_config_options: vec![],
                    permission_support: false,
                    terminal_support: false,
                    tool_call_streaming: ToolCallStreaming::None,
                },
                process: Arc::new(Mutex::new(None)),
                event_tx: Mutex::new(None),
                pending_requests: Arc::new(Mutex::new(HashMap::new())),
                prompt_results: Arc::new(Mutex::new(HashMap::new())),
                prompt_wait_notify: Notify::new(),
                active_prompt_ids: Mutex::new(HashSet::new()),
                remote_session_id: Mutex::new("remote-session-id".to_string()),
                fatal_error: Mutex::new(None),
                alive: AtomicBool::new(true),
                shutdown: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
                log_watcher_handle: Mutex::new(None),
            }),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_wait_for_response_timeout_is_typed() {
        let session = build_dummy_kimi_session();
        let prompt_id = PromptId::new("prompt-timeout");

        let result = timeout(
            Duration::from_millis(50),
            session.wait_for_response(&prompt_id, 20),
        )
        .await;
        let err = result
            .expect("wait_for_response timed out while test timer was active")
            .unwrap_err();
        assert!(matches!(err, KimiError::Timeout(_)));
    }

    #[tokio::test]
    async fn test_wait_for_response_is_notified_by_reader_signal() {
        let session = build_dummy_kimi_session();
        let ready = Arc::new(Notify::new());
        let mut waiter = tokio::spawn({
            let session = session.clone();
            let prompt_id = PromptId::new("prompt-notify");
            let ready = ready.clone();
            async move {
                session
                    .wait_for_response_with_ready_signal(&prompt_id, 1000, ready)
                    .await
                    .map(|_| ())
            }
        });

        ready.notified().await;
        {
            let mut guard = session.inner.prompt_results.lock().await;
            guard.insert(
                "prompt-notify".to_string(),
                Ok(AcpPromptResult {
                    response: Some("hi".to_string()),
                    tokens_used: None,
                    stop_reason: None,
                }),
            );
        }
        session.inner.prompt_wait_notify.notify_waiters();

        timeout(Duration::from_millis(20), &mut waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
