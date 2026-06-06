use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
const AGY_BREHON_MCP_CLIENT_PATH: &str = ".antigravitycli/brehon_mcp_client.py";
const AGY_PREFLIGHT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const AGY_PREFLIGHT_HELPER_TIMEOUT: Duration = Duration::from_secs(45);

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
    /// If true, the spawned Agy CLI will run with `--dangerously-skip-permissions`.
    /// Brehon uses this for unattended panes and relies on Brehon's worktree
    /// isolation and guards instead of Agy's interactive permission UI.
    pub allow_privileged_mode: bool,
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
        push_process_brehon_env_defaults(&mut env);

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
        let brehon_exe = current_brehon_exe();
        configure_mcp_in_workspace(&params.cwd, &brehon_exe, &env);
        write_brehon_mcp_client_helper(&params.cwd, &brehon_exe, &env);

        let mut args = Vec::new();
        if params.allow_privileged_mode {
            args.push("--dangerously-skip-permissions".to_string());
        }

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

fn desired_agy_mcp_config_for_workspace(
    exe: &str,
    workspace: &Path,
    env: &[(String, String)],
) -> serde_json::Value {
    let mut config = desired_agy_mcp_config(exe);
    if let Some(obj) = config.as_object_mut() {
        obj.insert(
            "cwd".to_string(),
            serde_json::Value::String(workspace.to_string_lossy().to_string()),
        );
        obj.insert(
            "env".to_string(),
            serde_json::Value::Object(brehon_mcp_env_map(env)),
        );
    }
    config
}

fn brehon_mcp_env_map(env: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (key, value) in std::env::vars().filter(|(key, _)| key.starts_with("BREHON_")) {
        map.insert(key, serde_json::Value::String(value));
    }
    for (key, value) in env.iter().filter(|(key, _)| key.starts_with("BREHON_")) {
        map.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    map
}

fn push_process_brehon_env_defaults(env: &mut Vec<(String, String)>) {
    for key in ["BREHON_SESSION_NAME", "BREHON_WORKTREE_ROOT"] {
        if env.iter().any(|(existing, _)| existing == key) {
            continue;
        }
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        env.push((key.to_string(), value.to_string()));
    }
}

fn with_agy_mcp_usage_guidance(startup_prompt: String) -> String {
    format!(
        "Antigravity MCP usage for this Brehon session:\n\
         - Brehon is available through the local helper \
           `.antigravitycli/brehon_mcp_client.py`.\n\
         - Antigravity CLI currently rejects guessed native MCP tool-call names such as `task`, \
           `brehon:task`, `brehon__task`, `mcp_brehon_task`, and `mcp__brehon__task`. Do not \
           attempt those tool names.\n\
         - Use shell commands that invoke the helper with `python3`. For example: \
           `python3 .antigravitycli/brehon_mcp_client.py agent \
           '{{\"action\":\"message\",\"target\":\"<supervisor>\",\"message\":\"ready\"}}'` or \
           `python3 .antigravitycli/brehon_mcp_client.py task '{{\"action\":\"mine\"}}'`.\n\
         - If the helper fails, report that as an infrastructure error to the supervisor instead \
           of exploring Antigravity internals.\n\
         - Do not inspect `~/.gemini/antigravity-cli/mcp/` JSON descriptor files or `.mcp.json` \
           while trying to discover Brehon tools; those descriptors are only Antigravity's MCP cache.\n\n\
         - Do not inspect `~/.gemini/antigravity-cli/scratch/`, \
           `~/.gemini/antigravity-cli/worktrees/`, or Antigravity helper scripts such as \
           `mcp_client.py`; they are CLI internals, not the Brehon control plane.\n\n\
         {startup_prompt}"
    )
}

fn configure_mcp_in_workspace(workspace: &Path, exe: &str, env: &[(String, String)]) {
    if cfg!(test) {
        return;
    }
    configure_project_mcp_config(workspace, exe, env);
}

fn configure_project_mcp_config(workspace: &Path, exe: &str, env: &[(String, String)]) {
    // Keep MCP discovery project-local using Antigravity CLI's workspace
    // config path, so one project's Brehon server does not leak into
    // unrelated Antigravity sessions.
    let path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
    merge_brehon_mcp_server(
        &path,
        desired_agy_mcp_config_for_workspace(exe, workspace, env),
    );
}

fn write_brehon_mcp_client_helper(workspace: &Path, exe: &str, env: &[(String, String)]) {
    if cfg!(test) {
        return;
    }
    let path = workspace.join(AGY_BREHON_MCP_CLIENT_PATH);
    if let Err(err) = write_brehon_mcp_client_helper_at(&path, workspace, exe, env) {
        warn!(
            path = %path.display(),
            "failed to write Agy Brehon MCP helper: {err}"
        );
    }
}

fn write_brehon_mcp_client_helper_at(
    path: &Path,
    workspace: &Path,
    exe: &str,
    env: &[(String, String)],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let helper = render_brehon_mcp_client_helper(exe, workspace, env);
    std::fs::write(path, helper)?;
    Ok(())
}

fn render_brehon_mcp_client_helper(
    exe: &str,
    workspace: &Path,
    env: &[(String, String)],
) -> String {
    let env_json = serde_json::Value::Object(brehon_mcp_env_map(env));
    format!(
        r#"#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import select
import time

BREHON_COMMAND = {command}
BREHON_ARGS = ["serve"]
BREHON_CWD = {cwd}
BREHON_ENV = {env}

INITIALIZE_TIMEOUT = 10.0
CALL_TIMEOUT = 30.0
SHUTDOWN_TIMEOUT = 5.0

def usage():
    print("Usage: brehon_mcp_client.py <tool_name> [arguments_json]", file=sys.stderr)
    sys.exit(2)

def send(proc, message):
    proc.stdin.write(json.dumps(message) + "\n")
    proc.stdin.flush()

def read_available_stderr(proc):
    try:
        r, _, _ = select.select([proc.stderr], [], [], 0.5)
        if not r:
            return ""
        return os.read(proc.stderr.fileno(), 4096).decode(errors="replace")
    except Exception:
        return ""

def recv(proc, timeout_sec):
    rlist, _, _ = select.select([proc.stdout], [], [], timeout_sec)
    if not rlist:
        proc.poll()
        stderr = read_available_stderr(proc)
        raise RuntimeError(f"Timeout waiting for response (timeout={{timeout_sec}}s). Exit code: {{proc.returncode}}. Stderr: {{stderr}}")
    line = proc.stdout.readline()
    if not line:
        stderr = read_available_stderr(proc)
        raise RuntimeError(f"Brehon MCP server closed stdout before replying. Exit code: {{proc.returncode}}. Stderr: {{stderr}}")
    try:
        return json.loads(line)
    except Exception as err:
        raise RuntimeError(f"Malformed JSON from server: {{err}}. Line: {{line}}")

def tool_result_is_error(message):
    result = message.get("result") if isinstance(message, dict) else None
    return isinstance(result, dict) and result.get("isError") is True

def safe_file_name(value):
    return "".join(ch if ch.isalnum() or ch in "-_" else "_" for ch in value)

def marker_path(kind):
    root = BREHON_ENV.get("BREHON_ROOT") or os.environ.get("BREHON_ROOT")
    agent = BREHON_ENV.get("BREHON_AGENT_NAME") or os.environ.get("BREHON_AGENT_NAME")
    if not root or not agent:
        return None
    return os.path.join(root, "runtime", kind, safe_file_name(agent))

def write_inflight_marker(tool_name):
    path = marker_path("mcp-helper-inflight")
    if not path:
        return
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        payload = {{
            "agent": BREHON_ENV.get("BREHON_AGENT_NAME"),
            "tool": tool_name,
            "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }}
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
    except Exception:
        pass

def clear_inflight_marker():
    path = marker_path("mcp-helper-inflight")
    if not path:
        return
    try:
        os.remove(path)
    except FileNotFoundError:
        pass
    except Exception:
        pass

def main():
    if len(sys.argv) < 2:
        usage()
    tool_name = sys.argv[1]
    raw_args = " ".join(sys.argv[2:]).strip() if len(sys.argv) > 2 else "{{}}"
    try:
        arguments = json.loads(raw_args or "{{}}")
    except Exception as err:
        print(f"invalid arguments JSON: {{err}}", file=sys.stderr)
        sys.exit(2)

    child_env = os.environ.copy()
    child_env.update(BREHON_ENV)
    write_inflight_marker(tool_name)
    try:
        proc = subprocess.Popen(
            [BREHON_COMMAND] + BREHON_ARGS,
            cwd=BREHON_CWD,
            env=child_env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
    except Exception as e:
        clear_inflight_marker()
        print(f"Failed to spawn Brehon MCP process: {{e}}", file=sys.stderr)
        sys.exit(1)

    try:
        send(proc, {{
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {{
                "protocolVersion": "2024-11-05",
                "capabilities": {{}},
                "clientInfo": {{"name": "brehon-agy-helper", "version": "1.0"}},
            }},
        }})
        init_res = recv(proc, INITIALIZE_TIMEOUT)
        if "error" in init_res:
            err = init_res["error"]
            print(f"MCP server initialization failed: {{err.get('message', err)}}", file=sys.stderr)
            sys.exit(1)

        send(proc, {{"jsonrpc": "2.0", "method": "notifications/initialized"}})
        send(proc, {{
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {{"name": tool_name, "arguments": arguments}},
        }})
        result = recv(proc, CALL_TIMEOUT)
        print(json.dumps(result, indent=2))
        if "error" in result:
            err = result["error"]
            print(f"MCP server error: {{err.get('message', err)}}", file=sys.stderr)
            sys.exit(1)
        if tool_result_is_error(result):
            print("MCP tool call returned error in result", file=sys.stderr)
            sys.exit(1)
    except Exception as err:
        print(f"Error: {{err}}", file=sys.stderr)
        sys.exit(1)
    finally:
        clear_inflight_marker()
        proc.terminate()
        try:
            proc.wait(timeout=SHUTDOWN_TIMEOUT)
        except subprocess.TimeoutExpired:
            proc.kill()

if __name__ == "__main__":
    main()
"#,
        command = serde_json::to_string(exe).unwrap_or_else(|_| "\"brehon\"".to_string()),
        cwd = serde_json::to_string(&workspace.to_string_lossy().to_string())
            .unwrap_or_else(|_| "\".\"".to_string()),
        env = serde_json::to_string_pretty(&env_json).unwrap_or_else(|_| "{}".to_string()),
    )
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

    remove_legacy_agora_servers(servers);
    let brehon_server = merge_existing_brehon_env(servers.get("brehon"), brehon_server);
    servers.insert("brehon".to_string(), brehon_server);
    write_json_pretty(path, &config);
}

fn merge_existing_brehon_env(
    existing_server: Option<&serde_json::Value>,
    mut brehon_server: serde_json::Value,
) -> serde_json::Value {
    let Some(existing_env) = existing_server
        .and_then(|server| server.get("env"))
        .and_then(|env| env.as_object())
    else {
        return brehon_server;
    };
    let Some(new_env) = brehon_server.get_mut("env") else {
        return brehon_server;
    };
    if !new_env.is_object() {
        *new_env = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(new_env) = new_env.as_object_mut() else {
        return brehon_server;
    };
    for (key, value) in existing_env {
        new_env.entry(key.clone()).or_insert_with(|| value.clone());
    }
    brehon_server
}

fn remove_legacy_agora_servers(servers: &mut serde_json::Map<String, serde_json::Value>) {
    let keys = servers
        .keys()
        .filter(|key| key.eq_ignore_ascii_case("agora"))
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        servers.remove(&key);
    }
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

/// Run preflight checks before considering Agy ready.
pub fn run_preflight_checks(
    workspace: &Path,
    command: &str,
    brehon_root: Option<&PathBuf>,
) -> Result<(), String> {
    if (cfg!(test) && std::env::var("BREHON_FORCE_PREFLIGHT").is_err())
        || std::env::var("BREHON_SKIP_PREFLIGHT").is_ok()
    {
        return Ok(());
    }

    // 1. Verify agy command is resolvable
    let mut command_check = Command::new(command);
    command_check.arg("--help");
    run_command_with_timeout(
        &format!("{command} --help"),
        command_check,
        AGY_PREFLIGHT_COMMAND_TIMEOUT,
    )
    .map_err(|err| format!("Command '{command}' is not resolvable: {err}"))?;

    // 2. Verify workspace `.agents/mcp_config.json` contains Brehon server
    let mcp_config_path = workspace.join(AGY_PROJECT_MCP_CONFIG_PATH);
    if !mcp_config_path.exists() {
        return Err(format!(
            "MCP config file '{}' does not exist",
            mcp_config_path.display()
        ));
    }
    let mcp_config_file = std::fs::File::open(&mcp_config_path)
        .map_err(|e| format!("Failed to open MCP config file: {}", e))?;
    let mcp_config: serde_json::Value = serde_json::from_reader(mcp_config_file)
        .map_err(|e| format!("Failed to parse MCP config JSON: {}", e))?;
    if mcp_config
        .get("mcpServers")
        .and_then(|s| s.get("brehon"))
        .is_none()
    {
        return Err("MCP config does not contain 'brehon' server".to_string());
    }

    // 3. Verify helper file exists
    let helper_path = workspace.join(AGY_BREHON_MCP_CLIENT_PATH);
    if !helper_path.exists() {
        return Err(format!(
            "MCP helper file '{}' does not exist",
            helper_path.display()
        ));
    }

    // 4. Verify helper can call a cheap Brehon tool successfully
    let mut helper_check = Command::new("python3");
    helper_check
        .arg(&helper_path)
        .arg("health")
        .current_dir(workspace);
    let output = run_command_with_timeout(
        "python3 Agy Brehon MCP helper health",
        helper_check,
        AGY_PREFLIGHT_HELPER_TIMEOUT,
    )
    .map_err(|err| format!("Failed to execute python3 helper: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "MCP helper call to 'health' failed with exit code {:?}.\nStdout: {}\nStderr: {}",
            output.status.code(),
            stdout.trim(),
            stderr.trim()
        ));
    }

    // 5. Verify trust-folder config was written or failure is surfaced clearly
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME environment variable not set".to_string())?;
    let paths_to_check = trusted_workspace_paths(workspace, brehon_root);
    if paths_to_check.is_empty() {
        return Err("No workspace paths to trust".to_string());
    }
    let trusted_folders_files = [
        home.join(".gemini/trustedFolders.json"),
        home.join(".gemini/config/trustedFolders.json"),
        home.join(".gemini/antigravity-cli/trustedFolders.json"),
    ];
    let settings_file = home.join(".gemini/antigravity-cli/settings.json");

    for p in &paths_to_check {
        let p_str = p.to_string_lossy().to_string();
        let mut found = false;
        for config_path in &trusted_folders_files {
            if config_path.exists() {
                if let Ok(file) = std::fs::File::open(config_path) {
                    if let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(file) {
                        if value.get(&p_str).is_some() {
                            found = true;
                            break;
                        }
                    }
                }
            }
        }
        if !found && settings_file.exists() {
            if let Ok(file) = std::fs::File::open(&settings_file) {
                if let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(file) {
                    if let Some(arr) = value.get("trustedWorkspaces").and_then(|v| v.as_array()) {
                        if arr.iter().any(|item| item.as_str() == Some(&p_str)) {
                            found = true;
                        }
                    }
                }
            }
        }
        if !found {
            return Err(format!(
                "Path '{}' is not present in global trust folder configurations. Preflight trust check failed.",
                p_str
            ));
        }
    }

    Ok(())
}

fn run_command_with_timeout(
    label: &str,
    mut command: Command,
    timeout: Duration,
) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().map_err(|err| err.to_string());
            }
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err.to_string());
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output().ok();
            let stderr = output
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stderr).trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_default();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(" stderr: {stderr}")
            };
            return Err(format!(
                "{label} timed out after {}s.{detail}",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[async_trait]
impl AgentAdapter for AgyAdapter {
    async fn spawn(&self, spec: SessionSpec) -> AdapterResult<SessionId> {
        let worktree = Path::new(&spec.worktree_path);
        let brehon_root = std::env::var("BREHON_ROOT").ok().map(PathBuf::from);
        if let Err(err) = run_preflight_checks(worktree, &self.config.command, brehon_root.as_ref())
        {
            return Err(AdapterError::spawn_failed(format!(
                "Agy preflight checks failed: {}",
                err
            )));
        }

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
mod agy_tests;
