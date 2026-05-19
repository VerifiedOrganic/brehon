use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use brehon_adapter_sdk::direct_tools::{CodingToolBridge, CompositeToolBridge, DirectToolBridge};
use brehon_mcp::server::{ContentBlock, ToolResult};
use brehon_mcp::McpServer;
use brehon_types::config::PermissionsConfig;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::agent_runtime::executor::{
    truncate_tool_output, ToolCall, ToolError, ToolExecutionContext, ToolExecutor, ToolOutput,
};
use crate::permissions::{
    permission_grant_key, permission_kind, permission_level, permission_prompt_decision,
    PermissionAction, PermissionGrantStore, PermissionLevel, PermissionPolicy, PolicyDecision,
};
use crate::runtime::{CancellationToken, PermissionMode};
use crate::server::RpcHandle;
use crate::shell::run_shell_command;

static TOOL_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const MAX_COMMAND_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;
const MAX_COMMAND_TIMEOUT_SECS: u64 = 600;
const BREHON_RAW_TOOL_NAMES: &[&str] = &[
    "health",
    "search_memories",
    "create_memory",
    "get_memories",
    "list_memories",
    "delete_memory",
    "search_rules",
    "create_rule",
    "search_skills",
    "get_task_context",
    "list_tasks",
    "get_task",
    "task",
    "agent",
    "factory",
    "verification",
    "task_actions",
];

pub struct ToolInvocationContext<'a> {
    pub rpc: &'a RpcHandle,
    pub session_id: &'a str,
    pub cancel: &'a CancellationToken,
}

#[derive(Clone)]
pub struct NativeTools {
    inner: Arc<dyn DirectToolBridge>,
    permission_mode: PermissionMode,
    permission_policy: PermissionPolicy,
    worktree_root: PathBuf,
    grants: PermissionGrantStore,
    brehon_tool_prefix: Option<String>,
    tool_env: Option<Vec<(String, String)>>,
}

impl NativeTools {
    #[cfg(test)]
    pub fn new(
        worktree_root: PathBuf,
        env: Vec<(String, String)>,
        tool_prefix: String,
        include_brehon_tools: bool,
        permission_mode: PermissionMode,
        permission_policy: PermissionsConfig,
        grants: PermissionGrantStore,
    ) -> Self {
        Self::new_with_tool_env(
            worktree_root,
            env,
            None,
            tool_prefix,
            include_brehon_tools,
            permission_mode,
            permission_policy,
            grants,
        )
    }

    pub fn new_with_tool_env(
        worktree_root: PathBuf,
        env: Vec<(String, String)>,
        tool_env: Option<Vec<(String, String)>>,
        tool_prefix: String,
        include_brehon_tools: bool,
        permission_mode: PermissionMode,
        permission_policy: PermissionsConfig,
        grants: PermissionGrantStore,
    ) -> Self {
        let mut bridges = Vec::new();
        let brehon_tool_prefix = include_brehon_tools.then(|| tool_prefix.clone());
        if include_brehon_tools {
            bridges.push(BrehonMcpToolBridge::new(env, &tool_prefix));
        }
        bridges.push(CodingToolBridge::new(worktree_root.clone()));
        Self {
            inner: CompositeToolBridge::new(bridges),
            permission_mode,
            permission_policy: PermissionPolicy::from_config(&permission_policy),
            worktree_root,
            grants,
            brehon_tool_prefix,
            tool_env,
        }
    }

    pub fn tool_definitions(&self) -> Vec<Value> {
        self.inner.tool_definitions()
    }

    fn supports_tool(&self, name: &str) -> bool {
        let canonical = self.canonical_tool_name(name);
        self.inner.tool_definitions().iter().any(|definition| {
            definition
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                == Some(canonical.as_str())
        })
    }

    pub async fn invoke(
        &self,
        ctx: ToolInvocationContext<'_>,
        name: &str,
        args: Value,
    ) -> Result<String, String> {
        let name = self.canonical_tool_name(name);
        let name = name.as_str();
        let args = normalize_tool_args(name, args);
        let level = permission_level(name);
        self.ensure_allowed(&ctx, name, level, &args).await?;

        if name == "bash" {
            return run_cancellable_bash(&self.worktree_root, ctx.cancel, args, &self.tool_env)
                .await;
        }

        tokio::select! {
            result = self.inner.invoke(name, args) => result,
            _ = ctx.cancel.cancelled() => Err("tool invocation cancelled".to_string()),
        }
    }

    async fn ensure_allowed(
        &self,
        ctx: &ToolInvocationContext<'_>,
        name: &str,
        level: PermissionLevel,
        args: &Value,
    ) -> Result<(), String> {
        if matches!(self.permission_mode, PermissionMode::Plan)
            && matches!(level, PermissionLevel::Write | PermissionLevel::Execute)
        {
            return Err(format!(
                "permission mode plan blocked {name}; switch modes or ask a supervisor"
            ));
        }
        if self.is_brehon_coordination_tool(name) {
            return Ok(());
        }

        let action = PermissionAction::new(name, level, args);
        if let Some(reason) = action.hard_deny_reason() {
            return Err(format!("permission safety blocked {name}: {reason}"));
        }

        let mut policy_evaluation = self.permission_policy.evaluate(&action);
        if matches!(policy_evaluation.decision, PolicyDecision::Deny) {
            return Err(format!(
                "permission policy denied {name}: {}",
                action.subject()
            ));
        }
        if matches!(self.permission_mode, PermissionMode::Bypass) {
            return Ok(());
        }

        let forced_prompt_reason = action.forced_prompt_reason();
        if forced_prompt_reason.is_some()
            && matches!(policy_evaluation.decision, PolicyDecision::Allow)
        {
            policy_evaluation.decision = PolicyDecision::Ask;
        }

        match policy_evaluation.decision {
            PolicyDecision::Allow => return Ok(()),
            PolicyDecision::Deny => unreachable!("deny is handled before bypass semantics"),
            PolicyDecision::Ask => {}
            PolicyDecision::Unspecified => match (self.permission_mode, level) {
                (_, PermissionLevel::ReadOnly) => return Ok(()),
                (PermissionMode::AcceptEdits, PermissionLevel::Write) => return Ok(()),
                _ => {}
            },
        }

        let grant_key = permission_grant_key(ctx.session_id, &action);
        if self.grants.lock().await.contains(&grant_key) {
            return Ok(());
        }

        let params = json!({
            "sessionId": ctx.session_id,
            "action": name,
            "category": action.category(),
            "operation": permission_kind(level),
            "subject": action.subject(),
            "risk": action.risk().as_str(),
            "riskReasons": action.risk_reasons(),
            "kind": permission_kind(level),
            "policy": {
                "source": policy_evaluation
                    .matched_rule
                    .as_ref()
                    .map(|rule| rule.source.as_str())
                    .unwrap_or("mode_default"),
                "matchedRule": policy_evaluation
                    .matched_rule
                    .as_ref()
                    .map(|rule| rule.to_json()),
            },
            "details": {
                "tool": name,
                "arguments": args,
                "subject": action.subject(),
                "scope": action.scope(),
                "forcedPromptReason": forced_prompt_reason,
                "shell": action.shell().map(|shell| shell.to_json()),
            },
            "options": [
                {
                    "optionId": "allow-once",
                    "kind": "allow_once",
                    "label": "Allow once"
                },
                {
                    "optionId": "allow-session",
                    "kind": "allow_session",
                    "label": "Allow this session"
                },
                {
                    "optionId": "deny",
                    "kind": "deny",
                    "label": "Deny"
                }
            ]
        });
        let response = ctx
            .rpc
            .request_with_cancel("session/request_permission", Some(params), ctx.cancel)
            .await?;
        if response.error.is_some() {
            return Err("permission request was rejected by the gateway".to_string());
        }
        let Some(result) = response.result else {
            return Err("permission response missing result".to_string());
        };
        let decision = permission_prompt_decision(&result);
        if !decision.allow {
            return Err(format!("permission denied for {name}"));
        }
        if decision.remember {
            self.grants.lock().await.insert(grant_key);
        }
        Ok(())
    }

    fn is_brehon_coordination_tool(&self, name: &str) -> bool {
        self.brehon_tool_prefix
            .as_deref()
            .is_some_and(|prefix| name.starts_with(prefix) || is_raw_brehon_tool_name(name))
    }

    fn canonical_tool_name(&self, name: &str) -> String {
        self.brehon_tool_prefix
            .as_deref()
            .map(|prefix| canonical_brehon_tool_name(name, prefix))
            .unwrap_or_else(|| name.to_string())
    }
}

#[async_trait]
impl ToolExecutor for NativeTools {
    fn tool_definitions(&self) -> Vec<Value> {
        NativeTools::tool_definitions(self)
    }

    async fn execute_tool_call(
        &self,
        ctx: ToolExecutionContext<'_>,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        if !self.supports_tool(&call.tool_id) {
            return Ok(None);
        }
        let output = self
            .invoke(
                ToolInvocationContext {
                    rpc: ctx.rpc,
                    session_id: ctx.session_id,
                    cancel: ctx.cancel,
                },
                &call.tool_id,
                call.params_value(),
            )
            .await
            .map_err(classify_tool_error)?;
        Ok(Some(ToolOutput {
            tool_name: self.canonical_tool_name(&call.tool_id),
            summary: truncate_tool_output(&output),
            raw_response: None,
            streamed: false,
            terminal_id: None,
        }))
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        let name = self.canonical_tool_name(tool_id);
        matches!(name.as_str(), "search_text" | "list_files")
            || name.ends_with("search_memories")
            || name.ends_with("search_rules")
            || name.ends_with("search_skills")
    }
}

fn classify_tool_error(error: String) -> ToolError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("cancelled") {
        ToolError::Cancelled
    } else if lower.contains("permission denied") || lower.contains("permission policy denied") {
        ToolError::PermissionDenied(error)
    } else if lower.contains("missing required") || lower.contains("invalid") {
        ToolError::InvalidParams(error)
    } else {
        ToolError::Execution(error)
    }
}

fn normalize_tool_args(name: &str, args: Value) -> Value {
    let Value::Object(mut map) = args else {
        return args;
    };

    match name {
        "bash" => {
            copy_string_alias(&mut map, "command", &["cmd"]);
        }
        "read_file" => {
            copy_string_alias(
                &mut map,
                "path",
                &["file_path", "filepath", "file", "filename"],
            );
            copy_value_alias(&mut map, "start_line", &["start", "line_start"]);
            copy_value_alias(&mut map, "end_line", &["end", "line_end"]);
        }
        "write_file" => {
            copy_string_alias(
                &mut map,
                "path",
                &["file_path", "filepath", "file", "filename"],
            );
        }
        "replace_in_file" => {
            copy_string_alias(
                &mut map,
                "path",
                &["file_path", "filepath", "file", "filename"],
            );
            copy_string_alias(&mut map, "old", &["old_text", "target", "search", "find"]);
            copy_string_alias(&mut map, "new", &["new_text", "replacement", "replace"]);
            copy_value_alias(&mut map, "replace_all", &["replaceAll"]);
        }
        "search_text" => {
            copy_string_alias(&mut map, "pattern", &["query", "regex", "text"]);
            copy_string_alias(&mut map, "path", &["dir", "directory"]);
        }
        "list_files" => {
            copy_string_alias(&mut map, "path", &["dir", "directory"]);
        }
        _ => {}
    }

    Value::Object(map)
}

fn copy_string_alias(map: &mut Map<String, Value>, target: &str, aliases: &[&str]) {
    if map
        .get(target)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        return;
    }
    if let Some(value) = aliases.iter().find_map(|alias| {
        map.get(*alias)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }) {
        map.insert(target.to_string(), Value::String(value));
    }
}

fn copy_value_alias(map: &mut Map<String, Value>, target: &str, aliases: &[&str]) {
    if map.contains_key(target) {
        return;
    }
    if let Some(value) = aliases.iter().find_map(|alias| map.get(*alias).cloned()) {
        map.insert(target.to_string(), value);
    }
}

pub(crate) fn canonical_brehon_tool_name(name: &str, tool_prefix: &str) -> String {
    if !tool_prefix.is_empty() && name.starts_with(tool_prefix) {
        return name.to_string();
    }
    if is_raw_brehon_tool_name(name) {
        return format!("{tool_prefix}{name}");
    }
    name.to_string()
}

pub(crate) fn is_raw_brehon_tool_name(name: &str) -> bool {
    BREHON_RAW_TOOL_NAMES.contains(&name)
}

pub(crate) async fn load_brehon_bootstrap_context(
    env: Vec<(String, String)>,
    tool_prefix: &str,
    agent_role: &str,
    agent_name: &str,
    agent_type: &str,
) -> Result<String, String> {
    load_brehon_runtime_context(env, tool_prefix, agent_role, agent_name, agent_type, true).await
}

pub(crate) async fn load_brehon_turn_context(
    env: Vec<(String, String)>,
    tool_prefix: &str,
    agent_role: &str,
    agent_name: &str,
    agent_type: &str,
) -> Result<String, String> {
    load_brehon_runtime_context(env, tool_prefix, agent_role, agent_name, agent_type, false).await
}

async fn load_brehon_runtime_context(
    env: Vec<(String, String)>,
    tool_prefix: &str,
    agent_role: &str,
    agent_name: &str,
    agent_type: &str,
    bootstrap: bool,
) -> Result<String, String> {
    let bridge = BrehonMcpToolBridge::new(env, tool_prefix);
    let agent_tool = format!("{tool_prefix}agent");
    let skills_tool = format!("{tool_prefix}search_skills");
    let rules_tool = format!("{tool_prefix}search_rules");
    let memories_tool = format!("{tool_prefix}search_memories");

    let session_start = if bootstrap {
        Some(
            bridge
                .invoke(
                    &agent_tool,
                    json!({
                        "action": "session_start",
                        "name": agent_name,
                        "agent_type": agent_role,
                    }),
                )
                .await?,
        )
    } else {
        None
    };
    let whoami = bridge
        .invoke(&agent_tool, json!({"action": "whoami"}))
        .await?;
    let skills = bridge
        .invoke(&skills_tool, json!({"query": "", "limit": 20}))
        .await
        .unwrap_or_else(|err| format!("ERROR: search_skills failed: {err}"));
    let rules = bridge
        .invoke(&rules_tool, json!({"query": "", "limit": 20}))
        .await
        .unwrap_or_else(|err| format!("ERROR: search_rules failed: {err}"));
    let memories_query = format!("brehon {agent_role} {agent_type}");
    let memories = bridge
        .invoke(&memories_tool, json!({"query": memories_query, "limit": 5}))
        .await
        .unwrap_or_else(|err| format!("ERROR: search_memories failed: {err}"));

    let bootstrap_line = if bootstrap {
        "The runtime completed `agent action=session_start` and `agent action=whoami` before this turn; do not repeat bootstrap calls unless a user prompt explicitly asks."
            .to_string()
    } else {
        "The runtime refreshed `agent action=whoami`, skills, rules, and relevant memories before this turn; do not repeat context-refresh calls unless a user prompt explicitly asks."
            .to_string()
    };
    let session_start_block = session_start
        .map(|value| format!("== session_start ==\n{value}\n\n"))
        .unwrap_or_default();

    Ok(format!(
        "Brehon runtime MCP context loaded before the model turn.\n\
{bootstrap_line}\n\
Use these native function names when Brehon text shows raw MCP examples:\n\
- agent -> {prefix}agent\n\
- task -> {prefix}task\n\
- factory -> {prefix}factory\n\
- verification -> {prefix}verification\n\
- search_skills -> {prefix}search_skills\n\
- search_rules -> {prefix}search_rules\n\
- search_memories -> {prefix}search_memories\n\
- get_memories -> {prefix}get_memories\n\
When a prompt shows raw MCP syntax such as `task action=ready`, call the matching `{prefix}*` function with JSON arguments, not a shell command and not a plain-text answer.\n\n\
{session_start_block}\
== whoami ==\n{whoami}\n\n\
== role skills ==\n{skills}\n\n\
== project rules ==\n{rules}\n\n\
== relevant memories ==\n{memories}",
        prefix = tool_prefix,
    ))
}

async fn run_cancellable_bash(
    worktree_root: &Path,
    cancel: &CancellationToken,
    args: Value,
    tool_env: &Option<Vec<(String, String)>>,
) -> Result<String, String> {
    let command = required_string(&args, "command")?;
    let timeout_secs = optional_u64(&args, "timeout_secs")
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS)
        .clamp(1, MAX_COMMAND_TIMEOUT_SECS);
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    let output = run_shell_command(
        worktree_root,
        cancel,
        shell,
        command,
        timeout_secs,
        tool_env,
    )
    .await?;
    Ok(truncate_bytes(&output, MAX_COMMAND_OUTPUT_BYTES))
}

fn required_string(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field '{field}'"))
}

fn optional_u64(args: &Value, field: &str) -> Option<u64> {
    args.get(field).and_then(Value::as_u64)
}

fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &value[..end])
}

struct BrehonMcpToolBridge {
    server: McpServer,
    env: Vec<(String, String)>,
    tool_prefix: String,
}

impl BrehonMcpToolBridge {
    fn new(env: Vec<(String, String)>, tool_prefix: &str) -> Arc<dyn DirectToolBridge> {
        let mut server = McpServer::new("brehon-native-agent-tools", env!("CARGO_PKG_VERSION"));
        server.register_builtin_tools();

        Arc::new(Self {
            server,
            env: with_derived_env(env),
            tool_prefix: tool_prefix.to_string(),
        })
    }
}

#[async_trait]
impl DirectToolBridge for BrehonMcpToolBridge {
    fn tool_definitions(&self) -> Vec<Value> {
        self.server
            .tool_definitions()
            .into_iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": format!("{}{}", self.tool_prefix, tool.name),
                        "description": format!("Brehon coordination tool: {}", tool.description),
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect()
    }

    async fn invoke(&self, name: &str, args: Value) -> Result<String, String> {
        let Some(tool_name) = name.strip_prefix(&self.tool_prefix) else {
            return Err(format!("unsupported tool: {name}"));
        };

        let _guard = TOOL_ENV_LOCK.lock().await;
        let mut previous = Vec::with_capacity(self.env.len());
        for (key, value) in &self.env {
            previous.push((key.clone(), std::env::var(key).ok()));
            std::env::set_var(key, value);
        }

        let result = self.server.call_tool(tool_name, args).await;

        for (key, value) in previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        result
            .map(tool_result_to_string)
            .map_err(|err| err.to_string())
    }
}

fn with_derived_env(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    let brehon_root = env
        .iter()
        .find_map(|(key, value)| (key == "BREHON_ROOT").then(|| PathBuf::from(value)));
    let has_project_root = env.iter().any(|(key, _)| key == "BREHON_PROJECT_ROOT");
    let has_workspace_root = env.iter().any(|(key, _)| key == "BREHON_WORKSPACE_ROOT");

    if let Some(root) = brehon_root {
        let project_root = if root.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            root.parent().map(Path::to_path_buf)
        } else {
            Some(root.clone())
        };

        if !has_project_root {
            if let Some(project_root) = project_root.as_ref() {
                env.push((
                    "BREHON_PROJECT_ROOT".to_string(),
                    project_root.to_string_lossy().to_string(),
                ));
            }
        }
        if !has_workspace_root {
            if let Some(project_root) = project_root {
                env.push((
                    "BREHON_WORKSPACE_ROOT".to_string(),
                    project_root.to_string_lossy().to_string(),
                ));
            }
        }
    }

    env
}

fn tool_result_to_string(result: ToolResult) -> String {
    let mut blocks = Vec::new();
    for block in result.content {
        match block {
            ContentBlock::Text { text } => blocks.push(text),
            ContentBlock::Image { mime_type, .. } => {
                blocks.push(format!("[image output omitted: {mime_type}]"))
            }
        }
    }
    let mut text = blocks.join("\n\n");
    if result.is_error == Some(true) && !text.starts_with("ERROR:") {
        text = format!("ERROR: {text}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{
        new_permission_grant_store, permission_prompt_decision as permission_decision,
        PermissionPromptDecision as PermissionDecision,
    };
    use brehon_adapter_sdk::JsonRpcResponse;
    use brehon_types::{PermissionCategory, PermissionValue};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn permission_result_requires_allow_option() {
        assert_eq!(
            permission_decision(&json!({
                "outcome": {"outcome": "selected", "optionId": "allow-once"}
            })),
            PermissionDecision {
                allow: true,
                remember: false
            }
        );
        assert_eq!(
            permission_decision(&json!({
                "outcome": {"outcome": "selected", "optionId": "allow-session"}
            })),
            PermissionDecision {
                allow: true,
                remember: true
            }
        );
        assert_eq!(
            permission_decision(&json!({
                "outcome": {"outcome": "selected", "optionId": "deny"}
            })),
            PermissionDecision {
                allow: false,
                remember: false
            }
        );
    }

    #[test]
    fn normalize_replace_in_file_accepts_common_argument_aliases() {
        let args = normalize_tool_args(
            "replace_in_file",
            json!({
                "file_path": ".planning/brehon-native-agent-spec.md",
                "old_text": "before",
                "new_text": "after",
                "replaceAll": true
            }),
        );

        assert_eq!(args["path"], ".planning/brehon-native-agent-spec.md");
        assert_eq!(args["old"], "before");
        assert_eq!(args["new"], "after");
        assert_eq!(args["replace_all"], true);
    }

    #[test]
    fn normalize_read_file_accepts_path_and_range_aliases() {
        let args = normalize_tool_args(
            "read_file",
            json!({
                "filename": "src/lib.rs",
                "start": "10",
                "line_end": 25
            }),
        );

        assert_eq!(args["path"], "src/lib.rs");
        assert_eq!(args["start_line"], "10");
        assert_eq!(args["end_line"], 25);
    }

    #[tokio::test]
    async fn write_tool_waits_for_acp_permission_before_invoking() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("blocked.txt");
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();
        let invoke_tools = tools.clone();
        let invoke_rpc = rpc.clone();
        let invoke_cancel = cancel.clone();

        let invoke = tokio::spawn(async move {
            invoke_tools
                .invoke(
                    ToolInvocationContext {
                        rpc: &invoke_rpc,
                        session_id: "session-1",
                        cancel: &invoke_cancel,
                    },
                    "write_file",
                    json!({"path": "blocked.txt", "content": "should not write"}),
                )
                .await
        });

        let mut lines = BufReader::new(reader_side).lines();
        let request_line = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_str(&request_line).unwrap();
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(request["params"]["action"], "write_file");
        assert!(
            !target.exists(),
            "file was written before permission resolved"
        );

        rpc.inject_response(JsonRpcResponse::success(
            request["id"].as_str().unwrap().to_string(),
            json!({"outcome": {"outcome": "selected", "optionId": "deny"}}),
        ))
        .await;

        let result = invoke.await.unwrap();
        assert!(result.unwrap_err().contains("permission denied"));
        assert!(!target.exists(), "denied write still created the file");
    }

    #[tokio::test]
    async fn allow_session_permission_skips_matching_later_request() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let first_tools = tools.clone();
        let first_rpc = rpc.clone();
        let first_cancel = cancel.clone();
        let first = tokio::spawn(async move {
            first_tools
                .invoke(
                    ToolInvocationContext {
                        rpc: &first_rpc,
                        session_id: "session-1",
                        cancel: &first_cancel,
                    },
                    "write_file",
                    json!({"path": "remembered.txt", "content": "first"}),
                )
                .await
        });

        let mut lines = BufReader::new(reader_side).lines();
        let request_line = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_str(&request_line).unwrap();
        rpc.inject_response(JsonRpcResponse::success(
            request["id"].as_str().unwrap().to_string(),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-session"}}),
        ))
        .await;
        first.await.unwrap().expect("first write allowed");

        let second = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "write_file",
                json!({"path": "remembered.txt", "content": "second"}),
            )
            .await
            .expect("second write allowed by session grant");
        assert!(second.contains("Wrote"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("remembered.txt")).unwrap(),
            "second"
        );

        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "matching session grant still emitted a permission request"
        );
    }

    #[tokio::test]
    async fn permission_policy_can_deny_read_tools() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "read".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "secret.txt".to_string(),
                        PermissionValue::Deny,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, _reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "read_file",
                json!({"path": "secret.txt"}),
            )
            .await;
        assert!(result
            .unwrap_err()
            .contains("permission policy denied read_file"));
    }

    #[tokio::test]
    async fn permission_policy_ask_can_mediate_read_tools() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("notes.txt"), "visible").unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "read".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "notes.txt".to_string(),
                        PermissionValue::Ask,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();
        let invoke_tools = tools.clone();
        let invoke_rpc = rpc.clone();
        let invoke_cancel = cancel.clone();

        let invoke = tokio::spawn(async move {
            invoke_tools
                .invoke(
                    ToolInvocationContext {
                        rpc: &invoke_rpc,
                        session_id: "session-1",
                        cancel: &invoke_cancel,
                    },
                    "read_file",
                    json!({"path": "notes.txt"}),
                )
                .await
        });

        let mut lines = BufReader::new(reader_side).lines();
        let request_line = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_str(&request_line).unwrap();
        assert_eq!(request["params"]["kind"], "read");
        rpc.inject_response(JsonRpcResponse::success(
            request["id"].as_str().unwrap().to_string(),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
        ))
        .await;

        let result = invoke.await.unwrap().expect("read allowed");
        assert!(result.contains("visible"));
    }

    #[tokio::test]
    async fn brehon_coordination_tools_do_not_prompt_under_global_ask_policy() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            true,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "*".to_string(),
                    PermissionCategory::Simple(PermissionValue::Ask),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "mcp_brehon_health",
                json!({}),
            )
            .await
            .expect("Brehon coordination tool should not ask for native permission");
        assert!(result.contains("\"status\":\"ok\"") || result.contains("\"status\": \"ok\""));

        let mut lines = BufReader::new(reader_side).lines();
        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "Brehon coordination tool emitted a permission request"
        );
    }

    #[tokio::test]
    async fn raw_brehon_tool_aliases_route_to_prefixed_functions() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            vec![(
                "BREHON_ROOT".to_string(),
                temp.path().join(".brehon").display().to_string(),
            )],
            "mcp_brehon_".to_string(),
            true,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "*".to_string(),
                    PermissionCategory::Simple(PermissionValue::Ask),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "health",
                json!({}),
            )
            .await
            .expect("raw Brehon MCP alias should route to prefixed bridge");
        assert!(result.contains("\"status\":\"ok\"") || result.contains("\"status\": \"ok\""));

        let mut lines = BufReader::new(reader_side).lines();
        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "raw Brehon coordination alias emitted a permission request"
        );
    }

    #[tokio::test]
    async fn bootstrap_context_loads_session_skills_rules_and_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let context = load_brehon_bootstrap_context(
            vec![
                (
                    "BREHON_ROOT".to_string(),
                    temp.path().join(".brehon").display().to_string(),
                ),
                ("BREHON_SESSION_ID".to_string(), "session-1".to_string()),
                ("BREHON_AGENT_ROLE".to_string(), "worker".to_string()),
                ("BREHON_AGENT_NAME".to_string(), "worker-1".to_string()),
                ("BREHON_AGENT_TYPE".to_string(), "native-worker".to_string()),
                ("BREHON_AGENT_MODEL".to_string(), "fake-model".to_string()),
            ],
            "mcp_brehon_",
            "worker",
            "worker-1",
            "native-worker",
        )
        .await
        .expect("bootstrap context should load through Brehon MCP");

        assert!(context.contains("session_start"));
        assert!(context.contains("== role skills =="));
        assert!(context.contains("== project rules =="));
        assert!(context.contains("verification -> mcp_brehon_verification"));
        assert!(context.contains("mcp_brehon_verification"));
        assert!(context.contains("brehon-worker") || context.contains("Worker"));
    }

    #[tokio::test]
    async fn turn_context_refreshes_skills_rules_without_restarting_session() {
        let temp = tempfile::tempdir().unwrap();
        let env = vec![
            (
                "BREHON_ROOT".to_string(),
                temp.path().join(".brehon").display().to_string(),
            ),
            ("BREHON_SESSION_ID".to_string(), "session-1".to_string()),
            ("BREHON_AGENT_ROLE".to_string(), "worker".to_string()),
            ("BREHON_AGENT_NAME".to_string(), "worker-1".to_string()),
            ("BREHON_AGENT_TYPE".to_string(), "native-worker".to_string()),
            ("BREHON_AGENT_MODEL".to_string(), "fake-model".to_string()),
        ];
        load_brehon_bootstrap_context(
            env.clone(),
            "mcp_brehon_",
            "worker",
            "worker-1",
            "native-worker",
        )
        .await
        .expect("bootstrap context should initialize session");

        let context =
            load_brehon_turn_context(env, "mcp_brehon_", "worker", "worker-1", "native-worker")
                .await
                .expect("turn context should refresh through Brehon MCP");

        assert!(context.contains("refreshed `agent action=whoami`"));
        assert!(!context.contains("== session_start =="));
        assert!(context.contains("== role skills =="));
        assert!(context.contains("== project rules =="));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permission_policy_allow_skips_gateway_permission() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "bash".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "printf policy-ok".to_string(),
                        PermissionValue::Allow,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            tools.invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "bash",
                json!({"command": "printf policy-ok", "timeout_secs": 5}),
            ),
        )
        .await
        .expect("policy-allowed bash should not wait for permission")
        .expect("policy-allowed bash should run");
        assert!(result.contains("policy-ok"));

        let mut lines = BufReader::new(reader_side).lines();
        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "policy-allowed command still emitted a permission request"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bypass_mode_allows_ask_policy_without_prompting() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "bash".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "*".to_string(),
                        PermissionValue::Ask,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "bash",
                json!({"command": "printf bypass-ok", "timeout_secs": 5}),
            )
            .await
            .expect("bypass mode should allow ask policy without prompting");
        assert!(result.contains("bypass-ok"));

        let mut lines = BufReader::new(reader_side).lines();
        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "bypass mode unexpectedly emitted a permission request for ask policy"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bypass_mode_still_denies_policy_denied_shell_component() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "bash".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "rm -rf *".to_string(),
                        PermissionValue::Deny,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "bash",
                json!({"command": "printf safe && rm -rf .", "timeout_secs": 5}),
            )
            .await;
        assert!(result
            .unwrap_err()
            .contains("permission policy denied bash"));

        let mut lines = BufReader::new(reader_side).lines();
        let extra_request =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(
            extra_request.is_err(),
            "denied bypass command unexpectedly emitted a permission request"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_tool_uses_restricted_environment_when_configured() {
        let temp = tempfile::tempdir().unwrap();
        std::env::set_var("NATIVE_AGENT_TEST_KEEP", "keep");
        std::env::set_var("NATIVE_AGENT_TEST_DROP", "drop");

        let tools = NativeTools::new_with_tool_env(
            temp.path().to_path_buf(),
            Vec::new(),
            Some(vec![(
                "NATIVE_AGENT_TEST_KEEP".to_string(),
                "keep".to_string(),
            )]),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let (writer_side, _reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();

        let result = tools
            .invoke(
                ToolInvocationContext {
                    rpc: &rpc,
                    session_id: "session-1",
                    cancel: &cancel,
                },
                "bash",
                json!({"command": "printf '%s:%s' \"$NATIVE_AGENT_TEST_KEEP\" \"$NATIVE_AGENT_TEST_DROP\""}),
            )
            .await
            .expect("bash should run");

        assert!(result.contains("keep:"));
        assert!(!result.contains("drop"));

        std::env::remove_var("NATIVE_AGENT_TEST_KEEP");
        std::env::remove_var("NATIVE_AGENT_TEST_DROP");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_substitution_forces_prompt_even_when_policy_allows() {
        let temp = tempfile::tempdir().unwrap();
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Default,
            PermissionsConfig {
                categories: std::collections::HashMap::from([(
                    "bash".to_string(),
                    PermissionCategory::Nested(std::collections::HashMap::from([(
                        "printf *".to_string(),
                        PermissionValue::Allow,
                    )])),
                )]),
            },
            new_permission_grant_store(),
        );
        let (writer_side, reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();
        let invoke_tools = tools.clone();
        let invoke_rpc = rpc.clone();
        let invoke_cancel = cancel.clone();

        let invoke = tokio::spawn(async move {
            invoke_tools
                .invoke(
                    ToolInvocationContext {
                        rpc: &invoke_rpc,
                        session_id: "session-1",
                        cancel: &invoke_cancel,
                    },
                    "bash",
                    json!({"command": "printf $(whoami)", "timeout_secs": 5}),
                )
                .await
        });

        let mut lines = BufReader::new(reader_side).lines();
        let request_line = tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("policy-allowed command substitution should still prompt")
            .unwrap()
            .unwrap();
        let request: Value = serde_json::from_str(&request_line).unwrap();
        assert_eq!(
            request["params"]["details"]["forcedPromptReason"],
            "command substitution requires explicit approval"
        );
        assert_eq!(
            request["params"]["details"]["shell"]["hasCommandSubstitution"],
            true
        );

        rpc.inject_response(JsonRpcResponse::success(
            request["id"].as_str().unwrap().to_string(),
            json!({"outcome": {"outcome": "selected", "optionId": "deny"}}),
        ))
        .await;

        let result = invoke.await.unwrap();
        assert!(result.unwrap_err().contains("permission denied"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_bash_kills_child_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("should-not-exist");
        let tools = NativeTools::new(
            temp.path().to_path_buf(),
            Vec::new(),
            "mcp_brehon_".to_string(),
            false,
            PermissionMode::Bypass,
            PermissionsConfig::default(),
            new_permission_grant_store(),
        );
        let (writer_side, _reader_side) = tokio::io::duplex(16 * 1024);
        let rpc = RpcHandle::new(writer_side);
        let cancel = CancellationToken::new();
        let invoke_tools = tools.clone();
        let invoke_rpc = rpc.clone();
        let invoke_cancel = cancel.clone();
        let command = format!("sleep 5; touch {}", marker.to_string_lossy());

        let invoke = tokio::spawn(async move {
            invoke_tools
                .invoke(
                    ToolInvocationContext {
                        rpc: &invoke_rpc,
                        session_id: "session-1",
                        cancel: &invoke_cancel,
                    },
                    "bash",
                    json!({"command": command, "timeout_secs": 30}),
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(2), invoke)
            .await
            .expect("cancelled bash should return promptly")
            .unwrap();
        assert!(result.unwrap_err().contains("cancelled"));

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !marker.exists(),
            "cancelled bash command still completed after cancellation"
        );
    }
}
