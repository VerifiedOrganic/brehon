use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use brehon_adapter_sdk::process::ProcessError;
use brehon_adapter_sdk::{
    AdapterError, AdapterEvent, AdapterResult, AgentAdapter, AgentProcess, PromptResult,
};
use brehon_types::{
    build_reviewer_startup_prompt, build_supervisor_startup_prompt, build_worker_startup_prompt,
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

const PROMPT_RESULT_POLL_MS: u64 = 50;
const AGY_SESSION_COMPLETE_KEY: &str = "_session_complete";
const AGY_PROJECT_MCP_CONFIG_PATH: &str = ".agents/mcp_config.json";

/// Error type for Agy adapter operations.
#[derive(Debug, thiserror::Error)]
pub enum AgyError {
    #[error("failed to spawn agy process: {0}")]
    Spawn(String),
    #[error("session not running")]
    NotRunning,
    #[error("timeout: {0}")]
    TimedOut(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for the Agy adapter.
#[derive(Debug, Clone)]
pub struct AgyConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Parameters for building a Agy session configuration.
#[derive(Debug, Clone)]
pub struct AgySpawnParams {
    pub name: String,
    pub role: String,
    pub cwd: PathBuf,
    pub brehon_root: Option<PathBuf>,
    pub supervisor_name: Option<String>,
    pub factory_worker_cli: Option<String>,
    pub model: Option<String>,
}

/// Session configuration for the Agy CLI.
#[derive(Debug, Clone)]
pub struct AgySessionConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

impl AgySessionConfig {
    /// Build the standard Agy CLI argument list and environment from
    /// [`AgySpawnParams`].
    #[allow(clippy::too_many_arguments)]
    pub fn from_params(params: &AgySpawnParams) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), params.name.clone()),
            ("BREHON_AGENT_ROLE".to_string(), params.role.clone()),
            ("BREHON_AGENT_TYPE".to_string(), "agy".to_string()),
            ("BREHON_SESSION_ID".to_string(), session_id),
            (
                "BREHON_CLONE_PATH".to_string(),
                params.cwd.to_string_lossy().to_string(),
            ),
        ];
        brehon_adapter_sdk::prepend_current_exe_dir_to_path(&mut env);
        brehon_adapter_sdk::push_workspace_root_env(&mut env, &params.cwd);

        if let Some(ref root) = params.brehon_root {
            brehon_adapter_sdk::push_brehon_root_env(&mut env, root);
        }

        if let Some(ref sup) = params.supervisor_name {
            env.push(("BREHON_SUPERVISOR_NAME".to_string(), sup.clone()));
        }
        if let Some(ref worker_cli) = params.factory_worker_cli {
            env.push((
                "BREHON_FACTORY_WORKER_CLI".to_string(),
                worker_cli.to_string(),
            ));
        }

        // Trust the dynamic sandbox/worktree path globally so that agy doesn't
        // prompt for trust, while letting agy run with the user's authentic
        // global HOME environment where keychains and auth are fully intact.
        trust_folders_globally(&trusted_workspace_paths(
            &params.cwd,
            params.brehon_root.as_ref(),
        ));
        configure_mcp_in_workspace(&params.cwd, &current_brehon_exe());

        let mut args = vec!["--dangerously-skip-permissions".to_string()];

        if params.role == "worker" {
            let project_policy = project_policy_for_role(params.brehon_root.as_ref(), &params.role);
            let startup_prompt = build_worker_startup_prompt(
                &params.name,
                params.supervisor_name.as_deref().unwrap_or("supervisor"),
                "agent",
                "task",
                project_policy.as_deref(),
            );
            args.push("--prompt-interactive".to_string());
            args.push(with_agy_mcp_usage_guidance(startup_prompt));
        } else if params.role == "supervisor" {
            let project_policy = project_policy_for_role(params.brehon_root.as_ref(), &params.role);
            let startup_prompt = build_supervisor_startup_prompt(
                &params.name,
                "agent",
                "task",
                project_policy.as_deref(),
            );
            args.push("--prompt-interactive".to_string());
            args.push(with_agy_mcp_usage_guidance(startup_prompt));
        } else if params.role == "reviewer" {
            let project_policy = project_policy_for_role(params.brehon_root.as_ref(), &params.role);
            let startup_prompt = build_reviewer_startup_prompt(
                &params.name,
                "agent",
                "verification",
                project_policy.as_deref(),
            );
            args.push("--prompt-interactive".to_string());
            args.push(with_agy_mcp_usage_guidance(startup_prompt));
        }

        Self {
            command: "agy".to_string(),
            args,
            cwd: Some(params.cwd.clone()),
            env,
            rows: 24,
            cols: 80,
        }
    }
}

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

fn project_policy_for_role(brehon_root: Option<&PathBuf>, role: &str) -> Option<String> {
    let project_root = brehon_root?.parent()?;
    let config = brehon_config::load_config(Some(project_root)).ok()?;
    config.project_prompt_for_role_name(role)
}

fn trusted_workspace_paths(cwd: &Path, brehon_root: Option<&PathBuf>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    push_unique_canonical_path(&mut paths, cwd);
    if let Some(project_root) = brehon_root.and_then(|root| root.parent()) {
        push_unique_canonical_path(&mut paths, project_root);
    }
    paths
}

fn push_unique_canonical_path(paths: &mut Vec<PathBuf>, path: &Path) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !paths.iter().any(|existing| existing == &canonical) {
        paths.push(canonical);
    }
}

pub fn desired_agy_mcp_config(exe: &str) -> serde_json::Value {
    serde_json::json!({
        "command": exe,
        "args": ["serve"]
    })
}

fn desired_agy_mcp_config_for_workspace(exe: &str, workspace: &Path) -> serde_json::Value {
    let mut config = desired_agy_mcp_config(exe);
    if let Some(obj) = config.as_object_mut() {
        obj.insert(
            "cwd".to_string(),
            serde_json::Value::String(workspace.to_string_lossy().to_string()),
        );
    }
    config
}

fn with_agy_mcp_usage_guidance(startup_prompt: String) -> String {
    format!(
        "Antigravity MCP usage for this Brehon session:\n\
         - Brehon is an MCP server named `brehon`.\n\
         - Its tools are direct MCP tools named `agent`, `task`, `factory`, `verification`, \
           `search_memories`, `search_rules`, and `search_skills`.\n\
         - Invoke those MCP tools directly with JSON arguments. For example, call MCP tool \
           `agent` with {{\"action\":\"message\",\"target\":\"<supervisor>\",\"message\":\"ready\"}}, \
           or call MCP tool `task` with {{\"action\":\"mine\"}}.\n\
         - Do not run shell commands such as `brehon task ...` to simulate MCP calls.\n\
         - Do not inspect `~/.gemini/antigravity-cli/mcp/` JSON descriptor files or `.mcp.json` \
           while trying to discover Brehon tools; those descriptors are only Antigravity's MCP cache.\n\n\
         {startup_prompt}"
    )
}

fn configure_mcp_in_workspace(workspace: &Path, exe: &str) {
    if cfg!(test) {
        return;
    }
    configure_project_mcp_config(workspace, exe);
}

fn configure_project_mcp_config(workspace: &Path, exe: &str) {
    // Keep MCP discovery project-local using Antigravity CLI's workspace
    // config path, so one project's Brehon server does not leak into
    // unrelated Antigravity sessions.
    let path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
    merge_brehon_mcp_server(&path, desired_agy_mcp_config_for_workspace(exe, workspace));
}

fn merge_brehon_mcp_server(path: &Path, brehon_server: serde_json::Value) {
    let mut config = read_json_or_empty_object(path);
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(servers) = servers.as_object_mut() else {
        return;
    };

    servers.insert("brehon".to_string(), brehon_server);
    write_json_pretty(path, &config);
}

fn trust_folders_globally(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    if cfg!(test) {
        return;
    }
    if let Some(global_home) = std::env::var("HOME").ok().map(PathBuf::from) {
        trust_folders_in_home(&global_home, paths);
    }
}

fn trust_folders_in_home(global_home: &Path, paths: &[PathBuf]) {
    let trusted_folders_paths = [
        global_home.join(".gemini/trustedFolders.json"),
        global_home.join(".gemini/config/trustedFolders.json"),
        global_home.join(".gemini/antigravity-cli/trustedFolders.json"),
    ];

    for path in &trusted_folders_paths {
        update_trusted_folders_file(path, paths);
    }

    update_antigravity_settings_trust(
        &global_home.join(".gemini/antigravity-cli/settings.json"),
        paths,
    );
}

fn update_trusted_folders_file(path: &Path, paths: &[PathBuf]) {
    let mut config = read_json_or_empty_object(path);
    let Some(obj) = config.as_object_mut() else {
        return;
    };

    for trusted_path in paths {
        obj.insert(
            trusted_path.to_string_lossy().to_string(),
            serde_json::Value::String("TRUST_FOLDER".to_string()),
        );
    }
    write_json_pretty(path, &config);
}

fn update_antigravity_settings_trust(path: &Path, paths: &[PathBuf]) {
    let mut settings = read_json_or_empty_object(path);
    let Some(obj) = settings.as_object_mut() else {
        return;
    };
    let entry = obj
        .entry("trustedWorkspaces".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = serde_json::Value::Array(Vec::new());
    }
    let Some(array) = entry.as_array_mut() else {
        return;
    };

    for trusted_path in paths {
        let value = trusted_path.to_string_lossy();
        if !array
            .iter()
            .any(|existing| existing.as_str() == Some(value.as_ref()))
        {
            array.push(serde_json::Value::String(value.to_string()));
        }
    }
    write_json_pretty(path, &settings);
}

fn read_json_or_empty_object(path: &Path) -> serde_json::Value {
    if path.exists() {
        if let Ok(file) = std::fs::File::open(path) {
            if let Ok(value) = serde_json::from_reader(file) {
                return value;
            }
        }
    }
    serde_json::json!({})
}

fn write_json_pretty(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(file) = std::fs::File::create(path) {
        let _ = serde_json::to_writer_pretty(file, value);
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

type PromptResults = Arc<tokio::sync::Mutex<HashMap<String, PromptResult>>>;

/// A single Agy session backed by a subprocess.
pub struct AgySession {
    session_id: SessionId,
    process: Arc<AgentProcess>,
    output: Arc<tokio::sync::Mutex<String>>,
    prompt_results: PromptResults,
    alive: AtomicBool,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl AgySession {
    /// Spawn a new Agy session.
    pub async fn spawn(spec: SessionSpec, config: &AgyConfig) -> Result<Self, AgyError> {
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let created_at = chrono::Utc::now();

        // Standard OS redirected stdio pipes via AgentProcess::spawn_with_env
        let process = AgentProcess::spawn_with_env(
            &config.command,
            &config.args,
            &spec.worktree_path,
            &config.env,
        )
        .await
        .map_err(|e| AgyError::Spawn(e.to_string()))?;

        let process = Arc::new(process);
        let output = Arc::new(tokio::sync::Mutex::new(String::new()));
        let prompt_results: PromptResults = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let alive = AtomicBool::new(true);

        let reader_process = Arc::clone(&process);
        let reader_output = Arc::clone(&output);
        let reader_results = Arc::clone(&prompt_results);
        let reader_alive = Arc::new(AtomicBool::new(alive.load(Ordering::SeqCst)));
        let session_id_clone = session_id.clone();
        tokio::spawn(async move {
            loop {
                if !reader_alive.load(Ordering::SeqCst) {
                    break;
                }
                let line = reader_process.recv_line(100).await;
                match line {
                    Ok(Some(line)) => {
                        debug!(session_id = %session_id_clone, line = %line, "Agy output");
                        let mut buf = reader_output.lock().await;
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                    Ok(None) => break,
                    Err(ProcessError::Timeout) => continue,
                    Err(e) => {
                        warn!(session_id = %session_id_clone, error = %e, "Agy reader error");
                        break;
                    }
                }
            }
            reader_alive.store(false, Ordering::SeqCst);

            let final_output = reader_output.lock().await.clone();
            let mut result = PromptResult::default();
            result.response = if final_output.is_empty() {
                None
            } else {
                Some(final_output)
            };
            result.stop_reason = Some("stop".to_string());
            reader_results
                .lock()
                .await
                .insert(AGY_SESSION_COMPLETE_KEY.to_string(), result);
        });

        Ok(Self {
            session_id,
            process,
            output,
            prompt_results,
            alive,
            created_at,
        })
    }

    /// Send a prompt line to the session's stdin.
    pub async fn send_prompt(&self, prompt: &PromptTurn) -> Result<(), AgyError> {
        self.process
            .send_line(&prompt.content)
            .await
            .map_err(|e| AgyError::Spawn(e.to_string()))
    }

    /// Wait for the session to produce a response.
    pub async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> Result<PromptResult, AgyError> {
        let deadline = Duration::from_millis(timeout_ms);
        let prompt_key = prompt_id.as_str().to_string();

        let wait = async {
            loop {
                {
                    let mut results = self.prompt_results.lock().await;

                    if let Some(result) = results.remove(&prompt_key) {
                        return Ok(result);
                    }
                    if let Some(result) = results.remove(AGY_SESSION_COMPLETE_KEY) {
                        return Ok(result);
                    }
                }

                if !self.alive.load(Ordering::SeqCst) || !self.process.is_alive() {
                    let output = self.output.lock().await.clone();
                    let mut result = PromptResult::default();
                    result.response = if output.is_empty() {
                        None
                    } else {
                        Some(output)
                    };
                    result.stop_reason = Some("stop".to_string());
                    return Ok(result);
                }

                sleep(Duration::from_millis(PROMPT_RESULT_POLL_MS)).await;
            }
        };

        timeout(deadline, wait).await.map_err(|_| {
            AgyError::TimedOut(format!(
                "timeout waiting for Agy response to {}",
                prompt_id.as_str()
            ))
        })?
    }

    /// Terminate the session.
    pub async fn terminate(&self) -> Result<(), AgyError> {
        self.alive.store(false, Ordering::SeqCst);
        self.process
            .kill()
            .await
            .map_err(|e| AgyError::Spawn(e.to_string()))
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec![],
            permission_support: false,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::None,
        }
    }

    pub fn session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.session_id.clone(),
            agent_id: brehon_types::AgentId::new("agy"),
            role: "worker".to_string(),
            health: if self.alive.load(Ordering::SeqCst) {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
            created_at: self.created_at,
            capabilities: self.capabilities(),
        }
    }

    pub async fn stability_counters(&self) -> brehon_types::StabilityCounters {
        brehon_types::StabilityCounters::default()
    }

    pub async fn health_check(&self) -> Result<HealthStatus, AgyError> {
        Ok(
            if self.process.is_alive() && self.alive.load(Ordering::SeqCst) {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Adapter implementation for the Agy CLI.
pub struct AgyAdapter {
    config: AgyConfig,
    session: RwLock<Option<Arc<AgySession>>>,
    event_broadcast: tokio::sync::broadcast::Sender<AdapterEvent>,
}

impl AgyAdapter {
    /// Create a new Agy adapter with the given configuration.
    pub fn new(config: AgyConfig) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(256);
        Self {
            config,
            session: RwLock::new(None),
            event_broadcast: tx,
        }
    }
}

fn agy_error_to_adapter_error(err: AgyError) -> AdapterError {
    match err {
        AgyError::Spawn(msg) => AdapterError::spawn_failed(msg),
        AgyError::NotRunning => AdapterError::transport_closed("session not running"),
        AgyError::TimedOut(msg) => AdapterError::timed_out(msg),
        AgyError::Io(e) => AdapterError::send_failed(e.to_string()),
    }
}

#[async_trait]
impl AgentAdapter for AgyAdapter {
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId> {
        let session = AgySession::spawn(spec, &self.config)
            .await
            .map_err(agy_error_to_adapter_error)?;

        let session_id = session.session_id().clone();
        *self.session.write().await = Some(Arc::new(session));
        Ok(session_id)
    }

    async fn send_prompt(&self, prompt: PromptTurn) -> AdapterResult<PromptHandle> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .send_prompt(&prompt)
            .await
            .map_err(agy_error_to_adapter_error)?;

        Ok(PromptHandle {
            prompt_id: prompt.prompt_id,
            session_id: session.session_id().as_str().to_string(),
            created_at: prompt.sent_at,
        })
    }

    async fn wait_for_response(
        &self,
        prompt_id: &PromptId,
        timeout_ms: u64,
    ) -> AdapterResult<PromptResult> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .wait_for_response(prompt_id, timeout_ms)
            .await
            .map_err(agy_error_to_adapter_error)
    }

    fn events(&self) -> tokio::sync::mpsc::Receiver<AdapterEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let mut broadcast_rx = self.event_broadcast.subscribe();
        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!("Agy broadcast receiver lagged by {} messages", skipped);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn terminate(&self) -> AdapterResult<()> {
        let session = self.session.write().await.take();
        if let Some(session) = session {
            session
                .terminate()
                .await
                .map_err(agy_error_to_adapter_error)?;
        }
        Ok(())
    }

    fn kind(&self) -> brehon_types::AdapterKind {
        brehon_types::AdapterKind::Agy
    }

    async fn capabilities(&self) -> AdapterResult<AgentCapabilities> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        Ok(session.capabilities())
    }

    async fn session_id(&self) -> SessionId {
        let session = self.session.read().await.as_ref().cloned();
        session
            .map(|s| s.session_id().clone())
            .unwrap_or_else(|| SessionId::new("agy-unknown"))
    }

    async fn session_info(&self) -> SessionInfo {
        let session = self.session.read().await.as_ref().cloned();
        session
            .map(|s| s.session_info())
            .unwrap_or_else(|| SessionInfo {
                session_id: SessionId::new("agy-unknown"),
                agent_id: brehon_types::AgentId::new("agy"),
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
        let session = self.session.read().await.as_ref().cloned();
        if let Some(session) = session {
            session.stability_counters().await
        } else {
            brehon_types::StabilityCounters::default()
        }
    }

    async fn set_config(&self, _option: &str, _value: &str) -> AdapterResult<()> {
        Ok(())
    }

    async fn cancel_prompt(&self, _prompt: &PromptId) -> AdapterResult<()> {
        Ok(())
    }

    async fn health_check(&self) -> AdapterResult<HealthStatus> {
        let session = self.session.read().await.as_ref().cloned();
        let session =
            session.ok_or_else(|| AdapterError::transport_closed("session not spawned"))?;
        session
            .health_check()
            .await
            .map_err(agy_error_to_adapter_error)
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
            "Terminal input is not supported for Agy sessions",
        ))
    }

    async fn resolve_permission(&self, _permission_id: &str, _approved: bool) -> AdapterResult<()> {
        Err(AdapterError::unsupported_operation(
            "Permission resolution is not supported for Agy sessions",
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_session_config_from_params_worker() {
        let params = AgySpawnParams {
            name: "agy-worker".to_string(),
            role: "worker".to_string(),
            cwd: PathBuf::from("/tmp"),
            brehon_root: None,
            supervisor_name: Some("supervisor".to_string()),
            factory_worker_cli: None,
            model: None,
        };
        let config = AgySessionConfig::from_params(&params);
        assert_eq!(config.command, "agy");
        assert_eq!(config.args[0], "--dangerously-skip-permissions");
        assert_eq!(config.args[1], "--prompt-interactive");
        assert!(config.args[2].contains("Brehon worker startup"));
        assert!(config.args[2].contains("Antigravity MCP usage for this Brehon session"));
        assert!(config.args[2].contains("call MCP tool `task` with {\"action\":\"mine\"}"));
        assert!(config.args[2].contains("Do not inspect `~/.gemini/antigravity-cli/mcp/`"));
        assert!(config.args[2].contains("You are worker 'agy-worker'"));
        assert!(config.args[2].contains("target=supervisor"));
        assert!(config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "agy-worker"));
        assert!(config
            .env
            .iter()
            .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker"));
    }

    #[test]
    fn agy_mcp_config_merges_brehon_server_in_workspace_agents_config() {
        let test_root =
            std::env::temp_dir().join(format!("brehon-agy-mcp-test-{}", uuid::Uuid::new_v4()));
        let workspace = test_root.join("workspace");
        let config_path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            r#"{"mcpServers":{"other":{"command":"other","args":["serve"]}}}"#,
        )
        .unwrap();

        configure_project_mcp_config(&workspace, "/tmp/brehon");

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(
            config["mcpServers"]["brehon"],
            serde_json::json!({
                "command": "/tmp/brehon",
                "args": ["serve"],
                "cwd": workspace.to_string_lossy()
            })
        );
        assert_eq!(config["mcpServers"]["other"]["command"], "other");
        assert!(!workspace.join(".mcp.json").exists());

        let _ = std::fs::remove_dir_all(test_root);
    }

    #[test]
    fn agy_trust_updates_legacy_and_cli_settings() {
        let test_root =
            std::env::temp_dir().join(format!("brehon-agy-trust-test-{}", uuid::Uuid::new_v4()));
        let home = test_root.join("home");
        let project = test_root.join("project");
        let worktree = project.join(".brehon/worktrees/runs/test/worker-1");
        std::fs::create_dir_all(&worktree).unwrap();
        let settings_path = home.join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            r#"{"colorScheme":"dark","trustedWorkspaces":["/already/trusted"]}"#,
        )
        .unwrap();

        let paths = trusted_workspace_paths(&worktree, Some(&project.join(".brehon")));
        trust_folders_in_home(&home, &paths);
        let worktree_key = paths[0].to_string_lossy();
        let project_key = paths[1].to_string_lossy();

        let trusted: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".gemini/trustedFolders.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            trusted
                .get(worktree_key.as_ref())
                .and_then(serde_json::Value::as_str),
            Some("TRUST_FOLDER")
        );
        assert_eq!(
            trusted
                .get(project_key.as_ref())
                .and_then(serde_json::Value::as_str),
            Some("TRUST_FOLDER")
        );

        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        let workspaces = settings["trustedWorkspaces"].as_array().unwrap();
        assert!(workspaces
            .iter()
            .any(|value| value.as_str() == Some(worktree_key.as_ref())));
        assert!(workspaces
            .iter()
            .any(|value| value.as_str() == Some(project_key.as_ref())));

        let _ = std::fs::remove_dir_all(test_root);
    }

    #[test]
    fn agy_adapter_kind_is_agy() {
        let adapter = AgyAdapter::new(AgyConfig {
            command: "agy".to_string(),
            args: vec![],
            env: vec![],
        });
        assert_eq!(adapter.kind(), brehon_types::AdapterKind::Agy);
    }
}
