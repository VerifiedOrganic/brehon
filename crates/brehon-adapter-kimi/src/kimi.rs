use std::collections::HashMap;
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
        parse_message, serialize_notification, serialize_request, JsonRpcMessage, JsonRpcRequest,
        JsonRpcResponse,
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
const KIMI_DEFAULT_MAX_CONTEXT_SIZE: u32 = 262_144;
const KIMI_DEFAULT_CAPABILITIES: &str = "thinking,image_in,video_in";

const KIMI_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);
const KIMI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const KIMI_PROMPT_ACCEPT_TIMEOUT: Duration = Duration::from_millis(1500);

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

/// Return the local Kimi share directory for a workspace.
pub fn kimi_share_dir(cwd: &Path) -> PathBuf {
    cwd.join(".brehon/factory-runtime/kimi/share")
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
        [mcp.client]\n\
        tool_call_timeout_ms = 60000\n",
        default_model = quote_toml_string(model_key),
        default_thinking = if thinking_enabled { "true" } else { "false" },
        default_yolo = if allow_yolo { "true" } else { "false" },
        model_key = quote_toml_string(model_key),
        raw_model_name = quote_toml_string(raw_kimi_model_name(raw_model_name)),
        max_context_size = KIMI_DEFAULT_MAX_CONTEXT_SIZE,
        base_url = KIMI_DEFAULT_BASE_URL,
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
    remote_session_id: Mutex<String>,
    alive: AtomicBool,
    shutdown: AtomicBool,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
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
            create_new_session_request(&spec.worktree_path, vec![]),
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
            remote_session_id: Mutex::new(remote_session_id),
            alive: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            reader_handle: Mutex::new(None),
        });

        let reader_handle = spawn_reader(Arc::clone(&inner));
        *inner.reader_handle.lock().await = Some(reader_handle);
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
        SessionInfo {
            session_id: self.inner.session_id.clone(),
            agent_id: self.inner.spec.agent_id.clone(),
            role: self.inner.spec.role.clone(),
            health: if self.inner.alive.load(Ordering::SeqCst) {
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
        let remote_session_id = self.inner.remote_session_id.lock().await.clone();
        let prompt_id = prompt.prompt_id.as_str().to_string();
        self.inner.prompt_results.lock().await.remove(&prompt_id);
        self.persist_runtime_stability();
        let request = create_prompt_request(&prompt_id, &remote_session_id, &prompt.content);
        if let Some(response) = self
            .send_request_with_short_acceptance(request, KIMI_PROMPT_ACCEPT_TIMEOUT)
            .await?
        {
            if let Some(error) = response.error {
                return Err(KimiError::Protocol(describe_rpc_error(&error)));
            }
            let prompt_result = parse_prompt_result(&response).map_err(KimiError::Protocol)?;
            self.inner
                .prompt_results
                .lock()
                .await
                .insert(prompt_id, Ok(prompt_result));
            self.inner.prompt_wait_notify.notify_waiters();
            self.persist_runtime_stability();
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
                let notified = self.inner.prompt_wait_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                ready.notify_waiters();
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

        clear_session_snapshot(self.inner.session_id.as_str());
        result
    }

    pub async fn health_check(&self) -> Result<HealthStatus, KimiError> {
        let process = self.inner.process.lock().await;
        Ok(
            if process.as_ref().map(|p| p.is_alive()).unwrap_or(false)
                && self.inner.alive.load(Ordering::SeqCst)
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
                let notified = self.inner.prompt_wait_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

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
        match parse_message(&line) {
            Ok(JsonRpcMessage::Response(response)) if response.id == request_id => {
                return Ok(response);
            }
            Ok(JsonRpcMessage::Notification(_)) => continue,
            Ok(JsonRpcMessage::Response(_)) => continue,
            Ok(JsonRpcMessage::Request(request)) => {
                debug!(method = %request.method, "Ignoring Kimi server request during bootstrap");
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

            match parse_message(&line) {
                Ok(JsonRpcMessage::Response(response)) => {
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
                                inner
                                    .prompt_results
                                    .lock()
                                    .await
                                    .insert(request_id.clone(), prompt_result);
                                inner.prompt_wait_notify.notify_waiters();
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
                        inner
                            .prompt_results
                            .lock()
                            .await
                            .insert(request_id, prompt_result);
                        inner.prompt_wait_notify.notify_waiters();
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
                Ok(JsonRpcMessage::Notification(notification)) => {
                    forward_kimi_notification(
                        &inner,
                        &notification.method,
                        notification.params.as_ref(),
                    )
                    .await;
                }
                Ok(JsonRpcMessage::Request(request)) => {
                    debug!(method = %request.method, "Ignoring Kimi server request");
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

        let config =
            build_minimal_kimi_config("kimi-code/kimi-for-coding", "kimi-for-coding", true, true);
        assert!(config.contains("default_yolo = true"));
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
    fn test_kimi_adapter_kind_is_acp() {
        let adapter = KimiAdapter::new(KimiConfig {
            command: "kimi".to_string(),
            args: vec![],
            env: vec![],
        });
        assert_eq!(adapter.kind(), brehon_types::AdapterKind::Acp);
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
                remote_session_id: Mutex::new("remote-session-id".to_string()),
                alive: AtomicBool::new(true),
                shutdown: AtomicBool::new(false),
                reader_handle: Mutex::new(None),
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
