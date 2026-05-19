use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brehon_adapter_sdk::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use brehon_types::{build_native_agent_system_prompt, config::PermissionsConfig};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Notify};

use crate::agent_runtime::message::{trim_message_history, AgentMessage, AgentRole};
use crate::agent_runtime::runner::{AgentTurnConfig, AgentTurnRunner};
use crate::cli::Cli;
use crate::permissions::{new_permission_grant_store, PermissionGrantStore};
use crate::provider::{ChatProvider, FakeProvider, OpenAiCompatibleProvider};
use crate::server::{rpc_error, RpcHandle};
use crate::terminal::NativeTerminalManager;
use crate::tools::{load_brehon_bootstrap_context, load_brehon_turn_context, NativeTools};

const MAX_HISTORY_MESSAGES: usize = 60;
const PROVIDER_STREAM_IDLE_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 8;
const HARD_MAX_PARALLEL_TOOL_CALLS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
}

impl PermissionMode {
    fn parse(input: &str) -> anyhow::Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "accept-edits" | "accept_edits" => Ok(Self::AcceptEdits),
            "plan" => Ok(Self::Plan),
            "bypass" | "yolo" => Ok(Self::Bypass),
            other => Err(anyhow::anyhow!("unsupported permission mode '{other}'")),
        }
    }
}

#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

#[derive(Clone)]
pub struct NativeRuntime {
    inner: Arc<NativeRuntimeInner>,
}

struct NativeRuntimeInner {
    provider: Arc<dyn ChatProvider>,
    model: String,
    reasoning_effort: Option<String>,
    reasoning_effort_param: Option<String>,
    agent_role: String,
    agent_name: String,
    agent_type: String,
    supervisor_name: String,
    sessions: Mutex<HashMap<String, SessionState>>,
    active_prompts: Mutex<HashMap<String, CancellationToken>>,
    env: Vec<(String, String)>,
    tool_prefix: String,
    include_brehon_tools: bool,
    permission_mode: PermissionMode,
    max_parallel_tool_calls: usize,
    assistant_message_passthrough_fields: Vec<String>,
    permission_policy: PermissionsConfig,
    permission_grants: PermissionGrantStore,
    extra_body: Option<Value>,
    provider_idle_timeout: Duration,
    terminals: NativeTerminalManager,
}

struct SessionState {
    cwd: PathBuf,
    messages: Vec<AgentMessage>,
    mode: String,
    model_override: Option<String>,
    context_bootstrapped: bool,
}

impl NativeRuntime {
    pub fn from_cli(cli: &Cli) -> anyhow::Result<Self> {
        let provider_name = cli.provider.trim().to_ascii_lowercase();
        let provider: Arc<dyn ChatProvider> = match provider_name.as_str() {
            "fake" => Arc::new(FakeProvider),
            "openai-compatible" | "openai_compatible" => {
                let base_url = cli
                    .base_url
                    .clone()
                    .or_else(|| std::env::var("BREHON_LLM_BASE_URL").ok())
                    .or_else(|| std::env::var("OPENAI_BASE_URL").ok());
                let api_key_env = cli
                    .api_key_env
                    .clone()
                    .or_else(|| std::env::var("BREHON_LLM_API_KEY_ENV").ok());
                Arc::new(OpenAiCompatibleProvider::new(
                    base_url,
                    api_key_env,
                    &parse_headers(&cli.headers)?,
                )?)
            }
            other => return Err(anyhow::anyhow!("unsupported provider '{other}'")),
        };

        let model = cli
            .model
            .clone()
            .or_else(|| std::env::var("BREHON_AGENT_MODEL").ok())
            .unwrap_or_else(|| "gpt-5.4-mini".to_string());
        let reasoning_effort = cli
            .reasoning_effort
            .clone()
            .or_else(|| std::env::var("BREHON_REASONING_EFFORT").ok());
        let reasoning_effort_param = cli
            .reasoning_effort_param
            .clone()
            .or_else(|| std::env::var("BREHON_LLM_REASONING_EFFORT_PARAM").ok());
        let agent_role = std::env::var("BREHON_AGENT_ROLE").unwrap_or_else(|_| "worker".into());
        let agent_name =
            std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "native-agent".into());
        let agent_type =
            std::env::var("BREHON_AGENT_TYPE").unwrap_or_else(|_| "native-agent".into());
        let supervisor_name =
            std::env::var("BREHON_SUPERVISOR_NAME").unwrap_or_else(|_| "supervisor".into());
        let permission_mode = effective_permission_mode(cli)?;
        let max_parallel_tool_calls = effective_max_parallel_tool_calls(cli);
        let provider_idle_timeout = effective_stream_idle_timeout(cli)?;
        let permission_policy = parse_permission_policy(cli.permission_policy_json.as_deref())?;
        let extra_body = parse_extra_body(cli.extra_body_json.as_deref())?;
        let env = runtime_tool_env(&cli.env_allowlist);

        Ok(Self {
            inner: Arc::new(NativeRuntimeInner {
                provider,
                model,
                reasoning_effort,
                reasoning_effort_param,
                agent_role,
                agent_name,
                agent_type,
                supervisor_name,
                sessions: Mutex::new(HashMap::new()),
                active_prompts: Mutex::new(HashMap::new()),
                env,
                tool_prefix: cli.tool_prefix.clone(),
                include_brehon_tools: !cli.no_brehon_tools,
                permission_mode,
                max_parallel_tool_calls,
                assistant_message_passthrough_fields: cli
                    .assistant_message_passthrough_fields
                    .clone(),
                permission_policy,
                permission_grants: new_permission_grant_store(),
                extra_body,
                provider_idle_timeout,
                terminals: NativeTerminalManager::default(),
            }),
        })
    }

    pub fn configured_model(&self) -> Option<String> {
        Some(self.inner.model.clone())
    }

    pub async fn handle_request(
        &self,
        rpc: &RpcHandle,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = request.id.clone();
        let result = match request.method.as_str() {
            "initialize" => Ok(initialize_result()),
            "session/new" => self.session_new(request.params).await,
            "session/prompt" => self.session_prompt(rpc, id.clone(), request.params).await,
            "session/set_model" => self.session_set_model(request.params).await,
            "session/set_mode" => self.session_set_mode(request.params).await,
            "session/cancel" => {
                self.cancel_from_params(request.params).await;
                Ok(json!({}))
            }
            "terminal_attach" => self.terminal_attach(request.params).await,
            "terminal_input" => self.terminal_input(rpc, request.params).await,
            "shutdown" => Ok(json!({})),
            other => return rpc_error(id, -32601, format!("method not found: {other}")),
        };

        match result {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(message) => rpc_error(id, -32603, message),
        }
    }

    pub async fn handle_notification(&self, _rpc: &RpcHandle, notification: JsonRpcNotification) {
        if notification.method == "session/cancel" {
            self.cancel_from_params(notification.params).await;
        }
    }

    async fn session_new(&self, params: Option<Value>) -> Result<Value, String> {
        let params = params.ok_or_else(|| "session/new missing params".to_string())?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(".")
            .to_string();
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let project_policy = std::env::var("BREHON_ROLE_SYSTEM_PROMPT").ok();
        let mut messages = vec![system_message_for(
            &self.inner.agent_role,
            &self.inner.agent_name,
            &self.inner.agent_type,
            &cwd,
            &self.inner.tool_prefix,
            &self.inner.supervisor_name,
            project_policy.as_deref(),
        )];
        let state = SessionState {
            cwd: PathBuf::from(&cwd),
            messages: std::mem::take(&mut messages),
            mode: "default".to_string(),
            model_override: None,
            context_bootstrapped: false,
        };
        self.inner
            .sessions
            .lock()
            .await
            .insert(session_id.clone(), state);

        Ok(json!({ "sessionId": session_id }))
    }

    async fn session_set_mode(&self, params: Option<Value>) -> Result<Value, String> {
        let params = params.ok_or_else(|| "session/set_mode missing params".to_string())?;
        let session_id = string_param(&params, &["sessionId", "session_id"])?;
        let mode = string_param(&params, &["modeId", "mode_id", "mode"])?;
        let mut sessions = self.inner.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("unknown session {session_id}"))?;
        session.mode = mode;
        Ok(json!({}))
    }

    async fn session_set_model(&self, params: Option<Value>) -> Result<Value, String> {
        let params = params.ok_or_else(|| "session/set_model missing params".to_string())?;
        let session_id = string_param(&params, &["sessionId", "session_id"])?;
        let model = string_param(&params, &["modelId", "model_id", "model"])?;
        let mut sessions = self.inner.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("unknown session {session_id}"))?;
        session.model_override = Some(model);
        Ok(json!({}))
    }

    async fn terminal_attach(&self, params: Option<Value>) -> Result<Value, String> {
        let params = params.ok_or_else(|| "terminal_attach missing params".to_string())?;
        let session_id = string_param(&params, &["sessionId", "session_id"])?;
        {
            let sessions = self.inner.sessions.lock().await;
            if !sessions.contains_key(&session_id) {
                return Err(format!("unknown session {session_id}"));
            }
        }
        let cols = params
            .get("cols")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        let rows = params
            .get("rows")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        Ok(self.inner.terminals.attach(session_id, cols, rows).await)
    }

    async fn terminal_input(
        &self,
        rpc: &RpcHandle,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let params = params.ok_or_else(|| "terminal_input missing params".to_string())?;
        let input = self.inner.terminals.decode_input(&params).await?;
        rpc.send_terminal_event(crate::ui::TerminalEvent::TerminalInput {
            terminal_id: input.terminal_id.clone(),
            data: input.data.clone(),
        });
        self.emit_update(
            rpc,
            json!({
                "sessionUpdate": "progress",
                "message": format!(
                    "terminal input received for {} ({} bytes)",
                    input.terminal_id,
                    input.data.len()
                ),
            }),
        )
        .await;
        Ok(json!({}))
    }

    async fn session_prompt(
        &self,
        rpc: &RpcHandle,
        prompt_id: String,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let params = params.ok_or_else(|| "session/prompt missing params".to_string())?;
        let session_id = string_param(&params, &["sessionId", "session_id"])?;
        let prompt = prompt_text(&params)?;
        let cancel = CancellationToken::new();
        let active_key = active_key(&session_id, &prompt_id);
        {
            let mut active = self.inner.active_prompts.lock().await;
            if active
                .keys()
                .any(|key| active_key_session(key).is_some_and(|active| active == session_id))
            {
                return Err(format!(
                    "session {session_id} already has an active prompt; cancel or wait before sending another"
                ));
            }
            active.insert(active_key.clone(), cancel.clone());
        }

        let result = self
            .run_prompt(rpc, &session_id, prompt, &cancel)
            .await
            .map(|result| {
                json!({
                    "response": result.response,
                    "tokensUsed": result.tokens_used,
                    "stopReason": result.stop_reason,
                })
            });

        self.inner.active_prompts.lock().await.remove(&active_key);
        result
    }

    async fn run_prompt(
        &self,
        rpc: &RpcHandle,
        session_id: &str,
        prompt: String,
        cancel: &CancellationToken,
    ) -> Result<PromptOutcome, String> {
        let nudge_text_only_first_turn = should_nudge_text_only_first_turn(
            self.inner.include_brehon_tools,
            &self.inner.agent_role,
            &prompt,
        );
        let (worktree, model, messages, bootstrap_context) = {
            let mut sessions = self.inner.sessions.lock().await;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| format!("unknown session {session_id}"))?;
            session.messages.push(AgentMessage::user(prompt));
            trim_messages(&mut session.messages);
            let bootstrap_context = !session.context_bootstrapped;
            session.context_bootstrapped = true;
            (
                session.cwd.clone(),
                session
                    .model_override
                    .clone()
                    .unwrap_or_else(|| self.inner.model.clone()),
                session.messages.clone(),
                bootstrap_context,
            )
        };

        self.emit_update(
            rpc,
            json!({
                "sessionUpdate": "operation_started",
                "operation": "native-agent turn"
            }),
        )
        .await;

        let session_env = runtime_session_env(
            &self.inner.env,
            session_id,
            &worktree.to_string_lossy(),
            &self.inner.agent_role,
            &self.inner.agent_name,
            &self.inner.agent_type,
            &model,
            self.inner.reasoning_effort.as_deref(),
        );
        let messages = self
            .prepare_turn_messages(session_id, &worktree, &model, messages, bootstrap_context)
            .await;
        let tools = NativeTools::new_with_tool_env(
            worktree,
            session_env.clone(),
            Some(session_env),
            self.inner.tool_prefix.clone(),
            self.inner.include_brehon_tools,
            self.inner.permission_mode,
            self.inner.permission_policy.clone(),
            self.inner.permission_grants.clone(),
        );

        let runner = AgentTurnRunner::new(
            self.inner.provider.clone(),
            AgentTurnConfig {
                model,
                reasoning_effort: self.inner.reasoning_effort.clone(),
                reasoning_effort_param: self.inner.reasoning_effort_param.clone(),
                extra_body: self.inner.extra_body.clone(),
                max_history_messages: MAX_HISTORY_MESSAGES,
                provider_idle_timeout: self.inner.provider_idle_timeout,
                max_parallel_tool_calls: self.inner.max_parallel_tool_calls,
                assistant_message_passthrough_fields: self
                    .inner
                    .assistant_message_passthrough_fields
                    .clone(),
                role: self.inner.agent_role.clone(),
                agent_name: self.inner.agent_name.clone(),
                agent_type: self.inner.agent_type.clone(),
                nudge_text_only_first_turn,
            },
            tools,
        );

        let outcome = runner.run(rpc, session_id, cancel, messages).await;
        match outcome {
            Ok(outcome) => {
                self.store_messages(session_id, outcome.messages).await;
                self.emit_turn_completed(rpc, outcome.success).await;
                Ok(PromptOutcome {
                    response: outcome.response,
                    tokens_used: outcome.tokens_used,
                    stop_reason: outcome.stop_reason,
                })
            }
            Err(err) => {
                self.store_messages(session_id, err.messages).await;
                self.emit_update(
                    rpc,
                    json!({
                        "sessionUpdate": "progress",
                        "message": format!("native-agent turn failed: {}", err.message),
                    }),
                )
                .await;
                self.emit_turn_completed(rpc, false).await;
                Err(err.message)
            }
        }
    }

    async fn store_messages(&self, session_id: &str, messages: Vec<AgentMessage>) {
        if let Some(session) = self.inner.sessions.lock().await.get_mut(session_id) {
            session.messages = messages;
        }
    }

    async fn prepare_turn_messages(
        &self,
        session_id: &str,
        worktree: &std::path::Path,
        model: &str,
        messages: Vec<AgentMessage>,
        bootstrap_context: bool,
    ) -> Vec<AgentMessage> {
        let project_policy = std::env::var("BREHON_ROLE_SYSTEM_PROMPT").ok();
        let mut turn_messages = vec![system_message_for(
            &self.inner.agent_role,
            &self.inner.agent_name,
            &self.inner.agent_type,
            &worktree.to_string_lossy(),
            &self.inner.tool_prefix,
            &self.inner.supervisor_name,
            project_policy.as_deref(),
        )];

        if self.inner.include_brehon_tools {
            let session_env = runtime_session_env(
                &self.inner.env,
                session_id,
                &worktree.to_string_lossy(),
                &self.inner.agent_role,
                &self.inner.agent_name,
                &self.inner.agent_type,
                model,
                self.inner.reasoning_effort.as_deref(),
            );
            let context = if bootstrap_context {
                load_brehon_bootstrap_context(
                    session_env,
                    &self.inner.tool_prefix,
                    &self.inner.agent_role,
                    &self.inner.agent_name,
                    &self.inner.agent_type,
                )
                .await
            } else {
                load_brehon_turn_context(
                    session_env,
                    &self.inner.tool_prefix,
                    &self.inner.agent_role,
                    &self.inner.agent_name,
                    &self.inner.agent_type,
                )
                .await
            }
            .unwrap_or_else(|err| {
                format!(
                    "Brehon runtime MCP context failed to load before this turn: {err}\n\
Brehon MCP tools may still be callable. If a required coordination tool fails, report the failure explicitly instead of waiting."
                )
            });
            turn_messages.push(AgentMessage::system(context));
        }

        turn_messages.extend(
            messages
                .into_iter()
                .filter(|message| message.role() != AgentRole::System),
        );
        trim_messages(&mut turn_messages);
        turn_messages
    }

    async fn emit_turn_completed(&self, rpc: &RpcHandle, success: bool) {
        self.emit_update(
            rpc,
            json!({
                "sessionUpdate": "operation_completed",
                "operation": "native-agent turn",
                "success": success,
            }),
        )
        .await;
    }

    async fn emit_update(&self, rpc: &RpcHandle, update: Value) {
        let _ = rpc
            .send_notification("session/update", Some(json!({ "update": update })))
            .await;
    }

    async fn cancel_from_params(&self, params: Option<Value>) {
        let Some(params) = params else {
            return;
        };
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str);
        let prompt_id = params
            .get("promptId")
            .or_else(|| params.get("prompt_id"))
            .and_then(Value::as_str);
        let active = self.inner.active_prompts.lock().await;
        for (key, token) in active.iter() {
            if active_key_matches(key, session_id, prompt_id) {
                token.cancel();
            }
        }
    }
}

struct PromptOutcome {
    response: Option<String>,
    tokens_used: Option<u64>,
    stop_reason: Option<String>,
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "content_block_types": ["text"],
            "session_config_options": ["mode", "model"],
            "permission_support": true,
            "terminal_support": true,
            "tool_call_streaming": "full",
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": true
            }
        }
    })
}

fn system_message_for(
    role: &str,
    name: &str,
    agent_type: &str,
    cwd: &str,
    tool_prefix: &str,
    supervisor_name: &str,
    project_policy: Option<&str>,
) -> AgentMessage {
    AgentMessage::system(build_native_agent_system_prompt(
        role,
        name,
        agent_type,
        cwd,
        tool_prefix,
        supervisor_name,
        project_policy,
    ))
}

fn string_param(params: &Value, keys: &[&str]) -> Result<String, String> {
    keys.iter()
        .find_map(|key| params.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field '{}'", keys[0]))
}

fn prompt_text(params: &Value) -> Result<String, String> {
    let prompt = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| "session/prompt missing prompt array".to_string())?;
    let text = prompt
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        Err("session/prompt text content is empty".to_string())
    } else {
        Ok(text)
    }
}

fn trim_messages(messages: &mut Vec<AgentMessage>) {
    trim_message_history(messages, MAX_HISTORY_MESSAGES);
}

fn effective_permission_mode(cli: &Cli) -> anyhow::Result<PermissionMode> {
    match cli.permission_mode.as_deref() {
        Some(mode) => PermissionMode::parse(mode),
        None if cli.worker => Ok(PermissionMode::Bypass),
        None => Ok(PermissionMode::Default),
    }
}

fn effective_max_parallel_tool_calls(cli: &Cli) -> usize {
    cli.max_parallel_tool_calls
        .or_else(|| {
            std::env::var("BREHON_MAX_PARALLEL_TOOL_CALLS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap_or(DEFAULT_MAX_PARALLEL_TOOL_CALLS)
        .clamp(1, HARD_MAX_PARALLEL_TOOL_CALLS)
}

fn effective_stream_idle_timeout(cli: &Cli) -> anyhow::Result<Duration> {
    let value = match cli.stream_idle_timeout_secs {
        Some(value) => Some(value),
        None => std::env::var("BREHON_AGENT_STREAM_IDLE_TIMEOUT_SECS")
            .ok()
            .map(|value| {
                value.parse::<u64>().map_err(|err| {
                    anyhow::anyhow!("invalid BREHON_AGENT_STREAM_IDLE_TIMEOUT_SECS: {err}")
                })
            })
            .transpose()?,
    }
    .unwrap_or(PROVIDER_STREAM_IDLE_TIMEOUT_SECS);

    if value == 0 {
        return Err(anyhow::anyhow!(
            "stream idle timeout must be greater than zero seconds"
        ));
    }
    Ok(Duration::from_secs(value))
}

fn should_nudge_text_only_first_turn(include_brehon_tools: bool, role: &str, prompt: &str) -> bool {
    if !include_brehon_tools {
        return false;
    }

    !is_reviewer_idle_startup_prompt(role, prompt)
}

fn is_reviewer_idle_startup_prompt(role: &str, prompt: &str) -> bool {
    if role.trim().to_ascii_lowercase() != "reviewer" {
        return false;
    }

    let prompt = prompt.trim_start();
    prompt.starts_with("Brehon reviewer startup.")
        || prompt.starts_with("Brehon reviewer session reset.")
        || prompt.contains(
            "Do NOT proactively discover, reconnect, or call Brehon MCP tools during idle startup",
        )
        || prompt.contains("Stay idle until you receive a review request prompt")
}

fn runtime_tool_env(allowlist: &[String]) -> Vec<(String, String)> {
    let allowlist = allowlist
        .iter()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .collect::<HashSet<_>>();
    if allowlist.is_empty() {
        return std::env::vars().collect();
    }

    std::env::vars()
        .filter(|(key, _)| key.starts_with("BREHON_") || allowlist.contains(key.as_str()))
        .collect()
}

fn runtime_session_env(
    base: &[(String, String)],
    session_id: &str,
    cwd: &str,
    role: &str,
    agent_name: &str,
    agent_type: &str,
    model: &str,
    reasoning_effort: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = base.to_vec();
    upsert_env(&mut env, "BREHON_SESSION_ID", session_id);
    upsert_env(&mut env, "BREHON_AGENT_ROLE", role);
    upsert_env(&mut env, "BREHON_AGENT_NAME", agent_name);
    upsert_env(&mut env, "BREHON_AGENT_TYPE", agent_type);
    upsert_env(&mut env, "BREHON_AGENT_MODEL", model);
    let project_root = session_project_root(base).unwrap_or_else(|| cwd.to_string());
    upsert_env(&mut env, "BREHON_PROJECT_ROOT", &project_root);
    upsert_env(&mut env, "BREHON_WORKSPACE_ROOT", cwd);
    if let Some(reasoning_effort) = reasoning_effort.filter(|value| !value.trim().is_empty()) {
        upsert_env(&mut env, "BREHON_REASONING_EFFORT", reasoning_effort);
    }
    env
}

fn session_project_root(base: &[(String, String)]) -> Option<String> {
    if let Some(root) = env_value(base, "BREHON_PROJECT_ROOT") {
        return Some(root.to_string());
    }

    let brehon_root = std::path::PathBuf::from(env_value(base, "BREHON_ROOT")?);
    if brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
        return brehon_root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().to_string());
    }
    Some(brehon_root.to_string_lossy().to_string())
}

fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|(env_key, value)| (env_key == key).then(|| value.trim()))
        .filter(|value| !value.is_empty())
}

fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, stored)) = env.iter_mut().find(|(stored_key, _)| stored_key == key) {
        *stored = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn parse_extra_body(input: Option<&str>) -> anyhow::Result<Option<Value>> {
    let Some(input) = input.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(input)?;
    if !value.is_object() {
        return Err(anyhow::anyhow!(
            "--extra-body-json must decode to a JSON object"
        ));
    }
    Ok(Some(value))
}

fn parse_permission_policy(input: Option<&str>) -> anyhow::Result<PermissionsConfig> {
    let env_input;
    let input = match input {
        Some(input) => Some(input),
        None => {
            env_input = std::env::var("BREHON_NATIVE_AGENT_PERMISSION_POLICY_JSON").ok();
            env_input.as_deref()
        }
    }
    .map(str::trim)
    .filter(|value| !value.is_empty());
    let Some(input) = input else {
        return Ok(PermissionsConfig::default());
    };
    serde_json::from_str(input)
        .map_err(|err| anyhow::anyhow!("invalid permission policy JSON: {err}"))
}

fn parse_headers(headers: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    headers
        .iter()
        .map(|header| {
            let (name, value) = header.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("--header must use KEY=VALUE format, got '{header}'")
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(anyhow::anyhow!("--header name must not be empty"));
            }
            Ok((name.to_string(), value.to_string()))
        })
        .collect()
}

fn active_key(session_id: &str, prompt_id: &str) -> String {
    format!("{session_id}:{prompt_id}")
}

fn active_key_session(key: &str) -> Option<&str> {
    key.split_once(':').map(|(session_id, _)| session_id)
}

fn active_key_matches(key: &str, session_id: Option<&str>, prompt_id: Option<&str>) -> bool {
    let Some((active_session_id, active_prompt_id)) = key.split_once(':') else {
        return false;
    };
    session_id.is_none_or(|session_id| session_id == active_session_id)
        && prompt_id.is_none_or(|prompt_id| prompt_id == active_prompt_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cli(worker: bool, supervised: bool, permission_mode: Option<&str>) -> Cli {
        Cli {
            worker,
            supervised,
            provider: "fake".to_string(),
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: Some("fake-model".to_string()),
            reasoning_effort: None,
            reasoning_effort_param: None,
            extra_body_json: None,
            permission_mode: permission_mode.map(str::to_string),
            max_parallel_tool_calls: None,
            stream_idle_timeout_secs: None,
            assistant_message_passthrough_fields: Vec::new(),
            permission_policy_json: None,
            env_allowlist: Vec::new(),
            tool_prefix: "mcp_brehon_".to_string(),
            no_brehon_tools: true,
            socket_path: None,
            ready_file: None,
        }
    }

    #[test]
    fn worker_without_explicit_permission_mode_defaults_to_bypass() {
        let cli = fake_cli(true, false, None);
        let runtime = NativeRuntime::from_cli(&cli).unwrap();

        assert_eq!(runtime.inner.permission_mode, PermissionMode::Bypass);
    }

    #[test]
    fn supervised_without_explicit_permission_mode_defaults_to_default() {
        let cli = fake_cli(false, true, None);
        let runtime = NativeRuntime::from_cli(&cli).unwrap();

        assert_eq!(runtime.inner.permission_mode, PermissionMode::Default);
    }

    #[test]
    fn explicit_worker_permission_mode_is_respected() {
        let cli = fake_cli(true, false, Some("plan"));
        let runtime = NativeRuntime::from_cli(&cli).unwrap();

        assert_eq!(runtime.inner.permission_mode, PermissionMode::Plan);
    }

    #[test]
    fn reviewer_idle_startup_prompt_does_not_force_tool_nudge() {
        let prompt = "Brehon reviewer startup. You are reviewer 'reviewer-1'.\n\
1) Do NOT proactively discover, reconnect, or call Brehon MCP tools during idle startup. Stay idle until you receive a review request prompt.";

        assert!(!should_nudge_text_only_first_turn(true, "reviewer", prompt));
    }

    #[test]
    fn reviewer_real_review_prompt_can_still_get_tool_nudge() {
        let prompt =
            "Review request for task T-1. Use verification action=submit_review review_id=REV-1.";

        assert!(should_nudge_text_only_first_turn(true, "reviewer", prompt));
    }

    #[test]
    fn worker_startup_prompt_keeps_tool_nudge() {
        let prompt = "Brehon worker startup. You are worker 'worker-1'.";

        assert!(should_nudge_text_only_first_turn(true, "worker", prompt));
    }

    #[test]
    fn max_parallel_tool_calls_is_configurable_and_clamped() {
        let mut cli = fake_cli(true, false, None);
        cli.max_parallel_tool_calls = Some(0);
        let runtime = NativeRuntime::from_cli(&cli).unwrap();
        assert_eq!(runtime.inner.max_parallel_tool_calls, 1);

        cli.max_parallel_tool_calls = Some(4);
        let runtime = NativeRuntime::from_cli(&cli).unwrap();
        assert_eq!(runtime.inner.max_parallel_tool_calls, 4);

        cli.max_parallel_tool_calls = Some(usize::MAX);
        let runtime = NativeRuntime::from_cli(&cli).unwrap();
        assert_eq!(
            runtime.inner.max_parallel_tool_calls,
            HARD_MAX_PARALLEL_TOOL_CALLS
        );
    }

    #[test]
    fn session_env_keeps_project_root_separate_from_worker_workspace() {
        let env = runtime_session_env(
            &[
                ("BREHON_ROOT".to_string(), "/repo/.brehon".to_string()),
                ("BREHON_PROJECT_ROOT".to_string(), "/repo".to_string()),
            ],
            "session-1",
            "/repo/.brehon/worktrees/runs/brehon-1/worker-1",
            "worker",
            "worker-1",
            "native",
            "model",
            None,
        );

        assert_eq!(env_value(&env, "BREHON_PROJECT_ROOT"), Some("/repo"));
        assert_eq!(
            env_value(&env, "BREHON_WORKSPACE_ROOT"),
            Some("/repo/.brehon/worktrees/runs/brehon-1/worker-1")
        );
    }

    #[test]
    fn session_env_derives_project_root_from_brehon_root() {
        let env = runtime_session_env(
            &[("BREHON_ROOT".to_string(), "/repo/.brehon".to_string())],
            "session-1",
            "/repo/.brehon/worktrees/runs/brehon-1/worker-1",
            "worker",
            "worker-1",
            "native",
            "model",
            None,
        );

        assert_eq!(env_value(&env, "BREHON_PROJECT_ROOT"), Some("/repo"));
        assert_eq!(
            env_value(&env, "BREHON_WORKSPACE_ROOT"),
            Some("/repo/.brehon/worktrees/runs/brehon-1/worker-1")
        );
    }

    #[test]
    fn active_prompt_keys_match_exact_session_and_prompt_ids() {
        let key = active_key("session-1", "prompt-1");

        assert_eq!(active_key_session(&key), Some("session-1"));
        assert!(active_key_matches(
            &key,
            Some("session-1"),
            Some("prompt-1")
        ));
        assert!(active_key_matches(&key, Some("session-1"), None));
        assert!(active_key_matches(&key, None, Some("prompt-1")));
        assert!(!active_key_matches(&key, Some("session"), None));
        assert!(!active_key_matches(&key, None, Some("prompt")));
    }

    #[test]
    fn system_message_embeds_first_class_role_contract() {
        let message = system_message_for(
            "reviewer",
            "reviewer-1",
            "native-reviewer",
            "/repo",
            "mcp_brehon_",
            "supervisor",
            Some("Review for architecture."),
        );
        let content = message.text_content();

        assert!(content.contains("native Rust agent runtime"));
        assert!(content.contains("runtime-owned behavior"));
        assert!(content.contains("Brehon reviewer startup"));
        assert!(content.contains("action=submit_review"));
        assert!(content.contains("reviewer=reviewer-1"));
        assert!(content.contains("Project policy:\nReview for architecture."));
        assert!(content.contains("mcp_brehon_* tools"));
    }

    #[tokio::test]
    async fn prepare_turn_messages_rebuilds_runtime_context_without_accumulating_old_systems() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = NativeRuntime::from_cli(&fake_cli(true, false, None)).unwrap();
        let messages = vec![
            AgentMessage::system("stale system"),
            AgentMessage::system("stale context"),
            AgentMessage::user("do work"),
            AgentMessage::assistant(Some("previous".to_string()), Vec::new()),
        ];

        let prepared = runtime
            .prepare_turn_messages("session-1", temp.path(), "fake-model", messages, false)
            .await;

        assert_eq!(prepared[0].role(), AgentRole::System);
        assert!(prepared[0]
            .text_content()
            .contains("native Rust agent runtime"));
        assert_eq!(
            prepared
                .iter()
                .filter(|message| message.role() == AgentRole::System)
                .count(),
            1
        );
        assert!(prepared.iter().any(
            |message| message.role() == AgentRole::User && message.text_content() == "do work"
        ));
        assert!(!prepared
            .iter()
            .any(|message| message.text_content().contains("stale system")));
    }

    #[test]
    fn runtime_tool_env_honors_allowlist_and_preserves_brehon_runtime_vars() {
        std::env::set_var("BREHON_TEST_KEEP", "1");
        std::env::set_var("NATIVE_AGENT_TEST_KEEP", "2");
        std::env::set_var("NATIVE_AGENT_TEST_DROP", "3");

        let env = runtime_tool_env(&["NATIVE_AGENT_TEST_KEEP".to_string()]);
        let lookup = env.into_iter().collect::<HashMap<_, _>>();

        assert_eq!(lookup.get("BREHON_TEST_KEEP").map(String::as_str), Some("1"));
        assert_eq!(
            lookup.get("NATIVE_AGENT_TEST_KEEP").map(String::as_str),
            Some("2")
        );
        assert!(!lookup.contains_key("NATIVE_AGENT_TEST_DROP"));

        std::env::remove_var("BREHON_TEST_KEEP");
        std::env::remove_var("NATIVE_AGENT_TEST_KEEP");
        std::env::remove_var("NATIVE_AGENT_TEST_DROP");
    }
}
