use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use brehon_adapter_sdk::process::ProcessError;
use brehon_adapter_sdk::{
    AdapterError, AdapterEvent, AdapterResult, AgentAdapter, AgentProcess, PromptResult,
};
use brehon_types::{
    AgentCapabilities, HealthStatus, PromptHandle, PromptId, PromptTurn, SessionId, SessionInfo,
    SessionSpec, TerminalId, ToolCallStreaming,
};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

const PROMPT_RESULT_POLL_MS: u64 = 50;
const AGY_SESSION_COMPLETE_KEY: &str = "_session_complete";

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
            env.push((
                "BREHON_ROOT".to_string(),
                root.to_string_lossy().to_string(),
            ));
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

        // Set up custom local home directory to bypass the trust folder prompt
        if let Ok(home_root) = prepare_local_agy_home(&params.cwd) {
            env.push(("HOME".to_string(), home_root.to_string_lossy().to_string()));
        }

        let mut args = vec![
            "--dangerously-skip-permissions".to_string(),
        ];

        let project_policy = project_policy_for_role(params.brehon_root.as_ref(), &params.role);

        let startup_prompt = if params.role == "worker" {
            Some(brehon_types::build_worker_startup_prompt(
                &params.name,
                params.supervisor_name.as_deref().unwrap_or("supervisor"),
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            ))
        } else if params.role == "supervisor" {
            Some(brehon_types::build_supervisor_startup_prompt(
                &params.name,
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            ))
        } else if params.role == "reviewer" {
            Some(brehon_types::build_reviewer_startup_prompt(
                &params.name,
                "mcp_brehon_agent",
                "mcp_brehon_verification",
                project_policy.as_deref(),
            ))
        } else {
            None
        };

        if let Some(prompt) = startup_prompt {
            args.push("--prompt-interactive".to_string());
            args.push(prompt);
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

fn project_policy_for_role(brehon_root: Option<&PathBuf>, role: &str) -> Option<String> {
    let project_root = brehon_root?.parent()?;
    let config = brehon_config::load_config(Some(project_root)).ok()?;
    config.project_prompt_for_role_name(role)
}

fn prepare_local_agy_home(cwd: &std::path::Path) -> std::result::Result<PathBuf, String> {
    let home_root = cwd.join(".brehon/factory-runtime/agy/home");
    let gemini_dir = home_root.join(".gemini");
    let agy_dir = home_root.join(".gemini/antigravity-cli");
    let config_dir = home_root.join(".gemini/config");

    std::fs::create_dir_all(&gemini_dir)
        .map_err(|e| format!("Failed to create local agy GeminiDir: {}", e))?;
    std::fs::create_dir_all(&agy_dir)
        .map_err(|e| format!("Failed to create local agy AppDataDir: {}", e))?;
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create local agy ConfigDir: {}", e))?;

    if let Some(global_home) = std::env::var("HOME").ok().map(PathBuf::from) {
        let global_gemini = global_home.join(".gemini");
        let global_agy = global_home.join(".gemini/antigravity-cli");
        let global_config = global_home.join(".gemini/config");

        let files_to_copy = [
            "gemini-credentials.json",
            "google_accounts.json",
            "oauth_creds.json",
            "installation_id",
            "state.json",
            "projects.json",
            "settings.json",
        ];

        for name in &files_to_copy {
            let src = if global_config.join(name).exists() {
                Some(global_config.join(name))
            } else if global_agy.join(name).exists() {
                Some(global_agy.join(name))
            } else if global_gemini.join(name).exists() {
                Some(global_gemini.join(name))
            } else {
                None
            };

            if let Some(src_path) = src {
                let _ = std::fs::copy(&src_path, gemini_dir.join(name));
                let _ = std::fs::copy(&src_path, agy_dir.join(name));
                let _ = std::fs::copy(&src_path, config_dir.join(name));
            }
        }
    }

    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let content = serde_json::json!({
        canonical_cwd.to_string_lossy(): "TRUST_FOLDER"
    });

    for dir in &[&gemini_dir, &agy_dir, &config_dir] {
        let trusted_folders_path = dir.join("trustedFolders.json");
        if let Ok(file) = std::fs::File::create(&trusted_folders_path) {
            let _ = serde_json::to_writer_pretty(file, &content);
        }
    }

    Ok(home_root)
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
        assert_eq!(config.args.len(), 3);
        assert_eq!(config.args[0], "--dangerously-skip-permissions");
        assert_eq!(config.args[1], "--prompt-interactive");
        assert!(config.args[2].contains("Brehon worker startup. You are worker 'agy-worker'"));
        assert!(config
            .env
            .iter()
            .any(|(k, v)| k == "HOME" && v.contains(".brehon/factory-runtime/agy/home")));
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
    fn agy_adapter_kind_is_agy() {
        let adapter = AgyAdapter::new(AgyConfig {
            command: "agy".to_string(),
            args: vec![],
            env: vec![],
        });
        assert_eq!(adapter.kind(), brehon_types::AdapterKind::Agy);
    }
}
