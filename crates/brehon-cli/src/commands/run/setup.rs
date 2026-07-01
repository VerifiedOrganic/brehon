use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use brehon_ports::GitOperations;
use brehon_types::{is_terminal_task_status, normalize_task_status, BrehonConfig};

use crate::commands::serve::{BREHON_MCP_BACKING_ENV, MCP_BACKING_RUNTIME_FILES};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod adapter_selection;
mod agent_git_guard;
#[cfg(test)]
mod protected_branch_tests;

pub(crate) use agent_git_guard::ensure_agent_git_worktree_guard;

const BREHON_PROTECTED_BRANCH_GUARD_BEGIN: &str = "# BEGIN BREHON PROTECTED BRANCH GUARD";
const BREHON_PROTECTED_BRANCH_GUARD_END: &str = "# END BREHON PROTECTED BRANCH GUARD";
const BREHON_PROTECTED_BRANCH_GUARD_MARKER: &str = "protected-branch-guard-active";
const BREHON_PROTECTED_BRANCH_BYPASS_DIR: &str = "protected-branch-bypass";
const BREHON_PROTECTED_BRANCH_HOOKS: &[&str] = &[
    "pre-commit",
    "pre-merge-commit",
    "commit-msg",
    "reference-transaction",
];

/// Ensure MCP discovery files exist at the project root so agents can discover
/// the Brehon MCP server. Also update `.claude/settings.local.json` to allow
/// `mcp__brehon__*` tool calls for Claude Code agents.
///
/// Both files are machine-local (absolute brehon binary path in the
/// former, per-developer permissions in the latter). The helper also
/// calls [`ensure_brehon_ignored_in_repo`] so the generated files are
/// immediately added to `.git/info/exclude` on the first run — no
/// teammate ever sees them as uncommitted work they need to reason
/// about.
pub(crate) fn ensure_mcp_config(cwd: &Path) -> Result<()> {
    let brehon_exe = current_brehon_exe();

    // ── .mcp.json ────────────────────────────────────────────────────────
    let mcp_path = cwd.join(".mcp.json");
    let brehon_server =
        brehon_mcp_server_config(&brehon_exe, cwd, cwd, None, None, None, None, None);
    let mcp_action = upsert_brehon_mcp_server(&mcp_path, brehon_server)?;
    if let Some(action) = mcp_action {
        tracing::info!("{action} .mcp.json with brehon server");
    }

    // ── opencode.json ────────────────────────────────────────────────────
    let opencode_path = cwd.join("opencode.json");
    let brehon_server =
        brehon_mcp_server_config(&brehon_exe, cwd, cwd, None, None, None, None, None);
    let opencode_action = upsert_brehon_opencode_mcp_server(&opencode_path, brehon_server)?;
    if let Some(action) = opencode_action {
        tracing::info!("{action} opencode.json with brehon MCP server");
    }

    // ── .claude/settings.local.json — add mcp__brehon__* permission ───────
    let claude_dir = cwd.join(".claude");
    if !claude_dir.exists() {
        std::fs::create_dir_all(&claude_dir)?;
    }
    let settings_path = claude_dir.join("settings.local.json");
    if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path).unwrap_or_default();
        let mut doc: serde_json::Value =
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

        let perms = doc
            .as_object_mut()
            .unwrap()
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}));
        let allow = perms
            .as_object_mut()
            .unwrap()
            .entry("allow")
            .or_insert_with(|| serde_json::json!([]));

        if let Some(arr) = allow.as_array_mut() {
            let needle = "mcp__brehon__*";
            if !arr.iter().any(|v| v.as_str() == Some(needle)) {
                arr.push(serde_json::Value::String(needle.to_string()));
                std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
                tracing::info!("Added mcp__brehon__* permission to .claude/settings.local.json");
            }
        }
    } else {
        let doc = serde_json::json!({
            "permissions": {
                "allow": ["mcp__brehon__*"]
            }
        });
        std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?)?;
        tracing::info!("Created .claude/settings.local.json with mcp__brehon__* permission");
    }

    // The two files above are machine-local by design (absolute binary
    // path, per-developer permissions). Ensure they're git-ignored at
    // the same moment we write them so they never surface as
    // "uncommitted work" on teammate checkouts.
    if let Err(err) = ensure_brehon_ignored_in_repo(cwd) {
        // Not fatal — MCP config is written, agents will still work.
        // Just warn so the operator knows the gitignore step failed.
        tracing::warn!(
            path = %cwd.display(),
            error = %err,
            "Failed to update .git/info/exclude; local MCP and agent settings files may show up as uncommitted"
        );
    }

    Ok(())
}

/// Copy machine-local agent bootstrap files into isolated worktrees.
///
/// These files are deliberately excluded from git because they contain
/// developer-local paths and permissions, but agents launched from a worktree
/// still need them for MCP discovery and tool authorization.
#[allow(clippy::too_many_arguments)]
fn sync_local_agent_scaffolding_to_worktree(
    project_root: &Path,
    worktree_path: &Path,
    worktrees_root: &Path,
    session_scope: Option<&str>,
    scope: Option<&str>,
    name: &str,
    supervisor_name: &str,
) -> Result<()> {
    let role = scope.unwrap_or("worker");
    let brehon_server = brehon_mcp_server_config(
        &current_brehon_exe(),
        project_root,
        worktree_path,
        Some(worktrees_root),
        session_scope,
        Some(name),
        Some(role),
        Some(supervisor_name),
    );

    sync_brehon_mcp_config_to_worktree(
        project_root,
        worktree_path,
        Path::new(".mcp.json"),
        &brehon_server,
        "MCP discovery config",
    )?;
    sync_brehon_mcp_config_to_worktree(
        project_root,
        worktree_path,
        Path::new(".agents/mcp_config.json"),
        &brehon_server,
        "Antigravity CLI MCP discovery config",
    )?;
    sync_brehon_opencode_config_to_worktree(
        project_root,
        worktree_path,
        Path::new("opencode.json"),
        &brehon_server,
        "OpenCode MCP discovery config",
    )?;
    sync_project_local_file_to_worktree(
        project_root,
        worktree_path,
        Path::new(".claude/settings.local.json"),
        "Claude local settings",
    )?;
    Ok(())
}

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

#[allow(clippy::too_many_arguments)]
fn brehon_mcp_server_config(
    brehon_exe: &str,
    project_root: &Path,
    workspace_root: &Path,
    worktrees_root: Option<&Path>,
    session_scope: Option<&str>,
    agent_name: Option<&str>,
    agent_role: Option<&str>,
    supervisor_name: Option<&str>,
) -> serde_json::Value {
    let mut env = serde_json::Map::new();
    insert_env_path(&mut env, "BREHON_ROOT", &project_root.join(".brehon"));
    insert_env_path(&mut env, "BREHON_PROJECT_ROOT", project_root);
    insert_env_path(&mut env, "BREHON_WORKSPACE_ROOT", workspace_root);
    insert_env_path(&mut env, "BREHON_CLONE_PATH", workspace_root);
    env.insert(
        BREHON_MCP_BACKING_ENV.to_string(),
        serde_json::Value::String(MCP_BACKING_RUNTIME_FILES.to_string()),
    );
    if let Some(worktrees_root) = worktrees_root {
        insert_env_path(&mut env, "BREHON_WORKTREE_ROOT", worktrees_root);
    }
    if let Some(session_scope) = session_scope
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        env.insert(
            "BREHON_SESSION_NAME".to_string(),
            serde_json::Value::String(session_scope.to_string()),
        );
    }
    if let Some(agent_name) = agent_name.map(str::trim).filter(|value| !value.is_empty()) {
        env.insert(
            "BREHON_AGENT_NAME".to_string(),
            serde_json::Value::String(agent_name.to_string()),
        );
    }
    if let Some(agent_role) = agent_role.map(str::trim).filter(|value| !value.is_empty()) {
        env.insert(
            "BREHON_AGENT_ROLE".to_string(),
            serde_json::Value::String(agent_role.to_string()),
        );
    }
    if let Some(supervisor_name) = supervisor_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        env.insert(
            "BREHON_SUPERVISOR_NAME".to_string(),
            serde_json::Value::String(supervisor_name.to_string()),
        );
    }

    serde_json::json!({
        "command": brehon_exe,
        "args": ["serve"],
        "cwd": workspace_root,
        "env": env,
    })
}

fn insert_env_path(env: &mut serde_json::Map<String, serde_json::Value>, key: &str, path: &Path) {
    env.insert(
        key.to_string(),
        serde_json::Value::String(path.to_string_lossy().to_string()),
    );
}

fn upsert_brehon_mcp_server(
    path: &Path,
    brehon_server: serde_json::Value,
) -> Result<Option<&'static str>> {
    let existed = path.exists();
    let mut doc = read_json_object_or_empty(path);
    let obj = doc
        .as_object_mut()
        .expect("read_json_object_or_empty always returns object");
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers = servers
        .as_object_mut()
        .expect("mcpServers normalized to object");
    let removed_legacy = remove_legacy_agora_servers(servers);
    let needs_brehon_update = servers.get("brehon") != Some(&brehon_server);
    if !needs_brehon_update && !removed_legacy && existed {
        return Ok(None);
    }

    servers.insert("brehon".to_string(), brehon_server);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(Some(if existed { "Updated" } else { "Created" }))
}

fn opencode_brehon_mcp_server_config(brehon_server: &serde_json::Value) -> serde_json::Value {
    let mut command = Vec::new();
    if let Some(cmd) = brehon_server
        .get("command")
        .and_then(|value| value.as_str())
    {
        command.push(serde_json::Value::String(cmd.to_string()));
    }
    if let Some(args) = brehon_server.get("args").and_then(|value| value.as_array()) {
        command.extend(args.iter().filter_map(|arg| {
            arg.as_str()
                .map(|value| serde_json::Value::String(value.to_string()))
        }));
    }

    serde_json::json!({
        "type": "local",
        "command": command,
        "environment": brehon_server
            .get("env")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        "enabled": true,
        "timeout": 10000,
    })
}

fn upsert_brehon_opencode_mcp_server(
    path: &Path,
    brehon_server: serde_json::Value,
) -> Result<Option<&'static str>> {
    let existed = path.exists();
    let mut doc = read_json_object_or_empty(path);
    let obj = doc
        .as_object_mut()
        .expect("read_json_object_or_empty always returns object");
    obj.entry("$schema".to_string())
        .or_insert_with(|| serde_json::Value::String("https://opencode.ai/config.json".into()));
    let servers = obj
        .entry("mcp".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers = servers.as_object_mut().expect("mcp normalized to object");
    let desired = opencode_brehon_mcp_server_config(&brehon_server);
    let needs_brehon_update = servers.get("brehon") != Some(&desired);
    if !needs_brehon_update && existed {
        return Ok(None);
    }

    servers.insert("brehon".to_string(), desired);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(Some(if existed { "Updated" } else { "Created" }))
}

fn read_json_object_or_empty(path: &Path) -> serde_json::Value {
    let Ok(content) = std::fs::read_to_string(path) else {
        return serde_json::json!({});
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return serde_json::json!({});
    };
    if value.is_object() {
        value
    } else {
        serde_json::json!({})
    }
}

fn remove_legacy_agora_servers(servers: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let keys = servers
        .keys()
        .filter(|key| key.eq_ignore_ascii_case("agora"))
        .cloned()
        .collect::<Vec<_>>();
    let removed = !keys.is_empty();
    for key in keys {
        servers.remove(&key);
    }
    removed
}

fn sync_brehon_mcp_config_to_worktree(
    project_root: &Path,
    worktree_path: &Path,
    relative_path: &Path,
    brehon_server: &serde_json::Value,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative_path);
    let target = worktree_path.join(relative_path);
    let mut doc = read_json_object_or_empty(&source);
    let obj = doc
        .as_object_mut()
        .expect("read_json_object_or_empty always returns object");
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers = servers
        .as_object_mut()
        .expect("mcpServers normalized to object");
    remove_legacy_agora_servers(servers);
    servers.insert("brehon".to_string(), brehon_server.clone());

    if let Some(target_dir) = target.parent() {
        std::fs::create_dir_all(target_dir).with_context(|| {
            format!(
                "Failed to create {label} directory '{}' in isolated worktree",
                target_dir.display()
            )
        })?;
    }
    std::fs::write(&target, serde_json::to_string_pretty(&doc)?).with_context(|| {
        format!(
            "Failed to sync {label} to isolated worktree '{}'",
            target.display()
        )
    })?;
    Ok(())
}

fn sync_brehon_opencode_config_to_worktree(
    project_root: &Path,
    worktree_path: &Path,
    relative_path: &Path,
    brehon_server: &serde_json::Value,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative_path);
    let target = worktree_path.join(relative_path);
    let mut doc = read_json_object_or_empty(&source);
    let obj = doc
        .as_object_mut()
        .expect("read_json_object_or_empty always returns object");
    obj.entry("$schema".to_string())
        .or_insert_with(|| serde_json::Value::String("https://opencode.ai/config.json".into()));
    let servers = obj
        .entry("mcp".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers = servers.as_object_mut().expect("mcp normalized to object");
    servers.insert(
        "brehon".to_string(),
        opencode_brehon_mcp_server_config(brehon_server),
    );

    if let Some(target_dir) = target.parent() {
        std::fs::create_dir_all(target_dir).with_context(|| {
            format!(
                "Failed to create {label} directory '{}' in isolated worktree",
                target_dir.display()
            )
        })?;
    }
    std::fs::write(&target, serde_json::to_string_pretty(&doc)?).with_context(|| {
        format!(
            "Failed to sync {label} to isolated worktree '{}'",
            target.display()
        )
    })?;
    Ok(())
}

fn sync_project_local_file_to_worktree(
    project_root: &Path,
    worktree_path: &Path,
    relative_path: &Path,
    label: &str,
) -> Result<()> {
    let source = project_root.join(relative_path);
    if !source.exists() {
        return Ok(());
    }

    let target = worktree_path.join(relative_path);
    if let Some(target_dir) = target.parent() {
        std::fs::create_dir_all(target_dir).with_context(|| {
            format!(
                "Failed to create {label} directory '{}' in isolated worktree",
                target_dir.display()
            )
        })?;
    }
    std::fs::copy(&source, &target).with_context(|| {
        format!(
            "Failed to sync {label} from '{}' to '{}'",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

/// Ensure Codex instruction files exist under `.brehon/instructions` so
/// Codex app-server agents can load them through `model_instructions_file`.
pub(crate) fn ensure_codex_instruction_files(cwd: &Path, config: &BrehonConfig) -> Result<()> {
    let instructions_dir = cwd.join(".brehon").join("instructions");
    std::fs::create_dir_all(&instructions_dir).with_context(|| {
        format!(
            "Failed to create Codex instructions directory '{}'",
            instructions_dir.display()
        )
    })?;

    let worker_body = format!(
        "Use your actual Brehon agent name wherever this prompt says '<your agent name>'.\n\n{}",
        brehon_pty::build_worker_startup_prompt(
            "<your agent name>",
            &config.roles.supervisor.name,
            "mcp__brehon__agent",
            "mcp__brehon__task",
            config.project_prompt_for_role_name("worker").as_deref(),
        )
    );
    let reviewer_body = format!(
        "Use your actual Brehon agent name wherever this prompt says '<your agent name>'.\n\n{}",
        brehon_pty::build_reviewer_startup_prompt(
            "<your agent name>",
            "mcp__brehon__agent",
            "mcp__brehon__verification",
            config.project_prompt_for_role_name("reviewer").as_deref(),
        )
    );
    let supervisor_body = brehon_pty::build_supervisor_startup_prompt(
        &config.roles.supervisor.name,
        "mcp__brehon__agent",
        "mcp__brehon__task",
        config.project_prompt_for_role_name("supervisor").as_deref(),
    );
    let advisor_body = format!(
        "Use your actual Brehon agent name wherever this prompt says '<your agent name>'.\n\n{}",
        brehon_pty::build_advisor_startup_prompt(
            "<your agent name>",
            "mcp__brehon__agent",
            "mcp__brehon__advisor",
            config.project_prompt_for_role_name("advisor").as_deref(),
        )
    );
    let research_body = format!(
        "Use your actual Brehon agent name wherever this prompt says '<your agent name>'.\n\n{}",
        brehon_pty::build_research_startup_prompt(
            "<your agent name>",
            "mcp__brehon__agent",
            "mcp__brehon__research",
            None,
            config.project_prompt_for_role_name("research").as_deref(),
        )
    );

    for (name, body) in [
        ("codex-worker-instructions.md", worker_body),
        ("codex-reviewer-instructions.md", reviewer_body),
        ("codex-supervisor-instructions.md", supervisor_body),
        ("codex-advisor-instructions.md", advisor_body),
        ("codex-research-instructions.md", research_body),
    ] {
        let path = instructions_dir.join(name);
        std::fs::write(&path, body).with_context(|| {
            format!(
                "Failed to write Codex instructions file '{}'",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn launcher_has_capability_overrides(agent_config: &brehon_types::AgentConnectionConfig) -> bool {
    agent_config
        .transport_str()
        .is_some_and(|value| !value.trim().is_empty())
        || agent_config
            .control_plane_str()
            .is_some_and(|value| !value.trim().is_empty())
}

fn builtin_cli_alias(agent_name: &str) -> Option<brehon_mux::SupervisorCli> {
    use brehon_mux::SupervisorCli;

    match agent_name {
        "claude-code" | "claude" => Some(SupervisorCli::Claude),
        "codex" => Some(SupervisorCli::Codex),
        "gemini" => Some(SupervisorCli::Gemini),
        "kimi" => Some(SupervisorCli::Kimi),
        "opencode" => Some(SupervisorCli::OpenCode),
        "junie" => Some(SupervisorCli::Junie),
        "copilot" => Some(SupervisorCli::Copilot),
        "agy" => Some(SupervisorCli::Agy),
        _ => None,
    }
}

fn builtin_cli_from_launcher_config(
    agent_config: &brehon_types::AgentConnectionConfig,
) -> Option<brehon_mux::SupervisorCli> {
    brehon_mux::builtin_cli_from_launcher_shape(
        agent_config.adapter,
        agent_config.command_str(),
        &agent_config.args,
    )
}

fn resolved_builtin_cli(
    agent_name: &str,
    config: &BrehonConfig,
) -> Option<(brehon_mux::SupervisorCli, bool)> {
    if let Some(agent_config) = config.lane_launcher(agent_name) {
        return builtin_cli_from_launcher_config(agent_config)
            .map(|cli| (cli, launcher_has_capability_overrides(agent_config)));
    }

    builtin_cli_alias(agent_name).map(|cli| (cli, false))
}

fn launches_codex_app_server(command: &str, args: &[String]) -> bool {
    command == "codex" && args.iter().any(|arg| arg == "app-server")
}

fn command_basename(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

fn launches_grok_agent_stdio(command: &str, args: &[String]) -> bool {
    command_basename(command) == "grok"
        && args.iter().any(|arg| arg == "agent")
        && args.iter().any(|arg| arg == "stdio")
}

fn native_agent_command(configured: Option<&str>) -> String {
    if let Some(command) = configured
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        if command_basename(command) != "agora-native-agent" {
            return command.to_string();
        }
    }

    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("brehon-native-agent"))
        })
        .filter(|path| path.exists())
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "brehon-native-agent".to_string())
}

fn native_agent_args(
    agent_config: &brehon_types::AgentConnectionConfig,
    permissions: &brehon_types::config::PermissionsConfig,
    security: &brehon_types::config::SecurityConfig,
) -> Vec<String> {
    let mut args = agent_config.args.clone();
    if !args
        .iter()
        .any(|arg| arg == "--worker" || arg == "--supervised")
    {
        let mode = if agent_config
            .control_plane_str()
            .map(|value| value.eq_ignore_ascii_case("acp_sidecar"))
            .unwrap_or(false)
        {
            "--supervised"
        } else {
            "--worker"
        };
        args.insert(0, mode.to_string());
    }

    push_option_arg(
        &mut args,
        "--provider",
        agent_config.provider_str().unwrap_or("openai-compatible"),
    );
    set_option_arg(&mut args, "--tool-prefix", "mcp_brehon_");
    if let Some(base_url) = agent_config.base_url_str() {
        push_option_arg(&mut args, "--base-url", base_url);
    }
    if let Some(api_key_env) = agent_config.api_key_env_str() {
        push_option_arg(&mut args, "--api-key-env", api_key_env);
    }
    let is_worker = args.iter().any(|arg| arg == "--worker");
    if let Some(permission_mode) = agent_config.permission_mode_str() {
        let effective_permission_mode =
            if is_worker && permission_mode.eq_ignore_ascii_case("default") {
                "bypass"
            } else {
                permission_mode
            };
        push_option_arg(&mut args, "--permission-mode", effective_permission_mode);
    } else if is_worker {
        push_option_arg(&mut args, "--permission-mode", "bypass");
    }
    if let Some(max_parallel_tool_calls) = agent_config.max_parallel_tool_calls() {
        push_option_arg(
            &mut args,
            "--max-parallel-tool-calls",
            &max_parallel_tool_calls.to_string(),
        );
    }
    if let Some(context_window) = agent_config.context_window() {
        push_option_arg(&mut args, "--context-window", &context_window.to_string());
    }
    for field in agent_config.assistant_message_passthrough_fields() {
        push_repeated_option_arg(&mut args, "--assistant-message-passthrough-field", field);
    }
    if !permissions.categories.is_empty() {
        if let Ok(policy_json) = serde_json::to_string(permissions) {
            push_option_arg(&mut args, "--permission-policy-json", &policy_json);
        }
    }
    for env_name in &security.env_allowlist {
        push_repeated_option_arg(&mut args, "--env-allowlist", env_name);
    }
    if let Some(path) = agent_config.reasoning_effort_param_str() {
        push_option_arg(&mut args, "--reasoning-effort-param", path);
    }
    if let Some(extra_body) = agent_config.extra_body.as_ref() {
        push_option_arg(&mut args, "--extra-body-json", &extra_body.to_string());
    }
    for (name, value) in &agent_config.headers {
        push_option_arg(&mut args, "--header", &format!("{name}={value}"));
    }

    args
}

fn push_option_arg(args: &mut Vec<String>, option: &str, value: &str) {
    if value.trim().is_empty() || args.iter().any(|arg| arg == option) {
        return;
    }
    args.push(option.to_string());
    args.push(value.to_string());
}

fn set_option_arg(args: &mut Vec<String>, option: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    let inline_prefix = format!("{option}=");
    if let Some(idx) = args.iter().position(|arg| arg.starts_with(&inline_prefix)) {
        args[idx] = format!("{option}={value}");
        return;
    }
    if let Some(idx) = args.iter().position(|arg| arg == option) {
        if let Some(existing) = args.get_mut(idx + 1) {
            *existing = value.to_string();
        } else {
            args.push(value.to_string());
        }
        return;
    }
    args.push(option.to_string());
    args.push(value.to_string());
}

fn push_repeated_option_arg(args: &mut Vec<String>, option: &str, value: &str) {
    if value.trim().is_empty()
        || args
            .windows(2)
            .any(|window| window[0] == option && window[1] == value)
    {
        return;
    }
    args.push(option.to_string());
    args.push(value.to_string());
}

fn apply_capability_overrides(
    capabilities: &mut brehon_mux::HarnessCapabilities,
    agent_config: &brehon_types::AgentConnectionConfig,
) {
    use brehon_mux::{HarnessControlPlane, HarnessTransport};

    let transport_override = agent_config
        .transport_str()
        .and_then(|value| value.parse::<HarnessTransport>().ok());
    let control_plane_override = agent_config
        .control_plane_str()
        .and_then(|value| value.parse::<HarnessControlPlane>().ok());

    if let Some(control_plane) = control_plane_override {
        capabilities.preferred_control_plane = control_plane;
        if let Some(transport) = transport_override {
            if transport.supports_control_plane(control_plane) {
                capabilities.transport = transport;
            } else {
                capabilities.transport = control_plane.canonical_transport();
            }
        } else {
            capabilities.transport = control_plane.canonical_transport();
        }
    } else if let Some(transport) = transport_override {
        if transport.supports_control_plane(capabilities.preferred_control_plane) {
            capabilities.transport = transport;
        }
    }

    capabilities.one_shot =
        capabilities.transport.is_one_shot() || capabilities.preferred_control_plane.is_one_shot();
}

pub(crate) fn agent_to_adapter(name: &str, config: &BrehonConfig) -> brehon_mux::AgentAdapter {
    use brehon_mux::{
        AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane,
        HarnessTransport, PromptInjectionStrategy, SupervisorCli,
    };
    use brehon_types::agent::AdapterKind;

    if let Some((cli, has_capability_overrides)) = resolved_builtin_cli(name, config) {
        if let Some(adapter) = adapter_selection::opencode_supervisor_adapter(name, config, cli) {
            return adapter;
        }

        if !has_capability_overrides {
            return AgentAdapter::BuiltIn(cli);
        }

        if let Some(agent_config) = config.lane_launcher(name) {
            let mut capabilities = cli.capabilities();
            apply_capability_overrides(&mut capabilities, agent_config);
            return AgentAdapter::built_in_with_capabilities(cli, capabilities);
        }
    }

    if let Some(agent_config) = config.lane_launcher(name) {
        let mut capabilities = match agent_config.adapter {
            AdapterKind::Acp
                if launches_codex_app_server(
                    agent_config.command_str().unwrap_or_default(),
                    &agent_config.args,
                ) =>
            {
                HarnessCapabilities {
                    supports_hooks: false,
                    supports_subagents: false,
                    supports_textbox_submit: false,
                    supports_teams: false,
                    one_shot: false,
                    uses_ink_prompt: false,
                    prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                    tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                    transport: HarnessTransport::AppServer,
                    preferred_control_plane: HarnessControlPlane::Acp,
                }
            }
            AdapterKind::Acp
                if launches_grok_agent_stdio(
                    agent_config.command_str().unwrap_or_default(),
                    &agent_config.args,
                ) =>
            {
                HarnessCapabilities {
                    supports_hooks: false,
                    supports_subagents: false,
                    supports_textbox_submit: true,
                    supports_teams: false,
                    one_shot: false,
                    uses_ink_prompt: false,
                    prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                    tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                    transport: HarnessTransport::AppServer,
                    preferred_control_plane: HarnessControlPlane::Acp,
                }
            }
            AdapterKind::Acp => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::OpenAiCompatible => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::ManagedApi,
                preferred_control_plane: HarnessControlPlane::OpenAiCompatible,
            },
            AdapterKind::NativeAgent => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::Codex => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::InkEcho,
                tool_prefix: std::borrow::Cow::Borrowed("mcp__brehon__"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::Mock => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
            AdapterKind::Kimi => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed(""),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::Junie => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: false,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: true,
                prompt_injection_strategy: PromptInjectionStrategy::InkEcho,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::InteractivePty,
                preferred_control_plane: HarnessControlPlane::PtyInjection,
            },
            AdapterKind::Copilot => HarnessCapabilities {
                supports_hooks: false,
                supports_subagents: false,
                supports_textbox_submit: true,
                supports_teams: false,
                one_shot: false,
                uses_ink_prompt: false,
                prompt_injection_strategy: PromptInjectionStrategy::ImmediateSubmit,
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::Agy => SupervisorCli::Agy.capabilities(),
            AdapterKind::PtyHooks => SupervisorCli::Claude.capabilities(),
        };
        apply_capability_overrides(&mut capabilities, agent_config);
        let command = if agent_config.adapter == AdapterKind::NativeAgent {
            Some(native_agent_command(agent_config.command_str()))
        } else {
            agent_config.command.clone()
        };
        let args = if agent_config.adapter == AdapterKind::NativeAgent {
            native_agent_args(agent_config, &config.permissions, &config.security)
        } else {
            agent_config.args.clone()
        };
        AgentAdapter::Custom(CustomAgentConfig {
            name: name.to_string(),
            command,
            args,
            base_url: agent_config.base_url.clone(),
            max_concurrency: agent_config.max_concurrency(),
            api_key_env: agent_config.api_key_env.clone(),
            headers: agent_config
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            capabilities,
        })
    } else {
        AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Claude)
    }
}

pub(crate) fn worker_branch_name(
    prefix: &str,
    session_scope: Option<&str>,
    worker_name: &str,
) -> String {
    let trimmed = prefix.trim();
    if let Some(session_scope) = session_scope.filter(|s| !s.is_empty()) {
        if trimmed.is_empty() {
            format!("runs/{session_scope}/{worker_name}")
        } else if trimmed.ends_with('/') {
            format!("{trimmed}runs/{session_scope}/{worker_name}")
        } else {
            format!("{trimmed}/runs/{session_scope}/{worker_name}")
        }
    } else if trimmed.is_empty() {
        worker_name.to_string()
    } else if trimmed.ends_with('/') {
        format!("{trimmed}{worker_name}")
    } else {
        format!("{trimmed}/{worker_name}")
    }
}

pub(crate) fn role_branch_name(
    prefix: &str,
    session_scope: Option<&str>,
    role_segment: &str,
    agent_name: &str,
) -> String {
    let trimmed = prefix.trim_end_matches('/');
    if let Some(session_scope) = session_scope.filter(|s| !s.is_empty()) {
        if trimmed.is_empty() {
            format!("runs/{session_scope}/{role_segment}/{agent_name}")
        } else {
            format!("{trimmed}/runs/{session_scope}/{role_segment}/{agent_name}")
        }
    } else if trimmed.is_empty() {
        format!("{role_segment}/{agent_name}")
    } else {
        format!("{trimmed}/{role_segment}/{agent_name}")
    }
}

pub(crate) fn slugify_branch_component(title: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in title.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_ascii_whitespace() || matches!(ch, '-' | '_' | '/' | ':') {
            Some('-')
        } else {
            None
        };

        let Some(normalized) = normalized else {
            continue;
        };

        if normalized == '-' {
            if slug.is_empty() || last_was_dash {
                continue;
            }
            last_was_dash = true;
            slug.push('-');
        } else {
            last_was_dash = false;
            slug.push(normalized);
        }
    }

    slug.trim_matches('-').to_string()
}

pub(crate) fn default_initiative_integration_branch(task_id: &str, title: &str) -> String {
    let short_id = task_id
        .strip_prefix("T-")
        .unwrap_or(task_id)
        .to_ascii_lowercase();
    let slug = slugify_branch_component(title);
    if slug.is_empty() {
        format!("initiative/{short_id}")
    } else {
        format!("initiative/{slug}-{short_id}")
    }
}

pub(crate) fn default_initiative_integration_worktree(
    worktrees_root: &Path,
    task_id: &str,
) -> PathBuf {
    worktrees_root.join("initiative").join(task_id)
}

/// Gitignore patterns that cover every file `brehon run` generates in the
/// shared repo root. All of these are machine-local — committing them
/// poisons teammate checkouts:
///
/// * `.brehon/` — runtime/state files and nested worktrees stay fully ignored
///   from the shared checkout. Brehon worktrees are complete repositories with
///   their own `.gitignore` files, so exposing their directories can re-expose
///   files in the shared root.
/// * `.mcp.json` — Claude Code MCP discovery file. Written with an
///   absolute path to the current machine's brehon binary (see
///   [`ensure_mcp_config`]); the path won't resolve on any other host.
/// * `.agents/mcp_config.json` — Antigravity CLI workspace MCP discovery
///   file. Also contains an absolute path to this machine's brehon binary.
/// * `opencode.json` — OpenCode project MCP discovery file. Also contains
///   an absolute path to this machine's brehon binary.
/// * `.antigravitycli` — Antigravity CLI's project-local state/cache.
/// * `.claude/settings.local.json` — Claude Code per-developer
///   permissions file. The `.local.json` suffix already signals
///   machine-local by Claude Code convention, but it's worth making
///   explicit.
///
/// Written to `.git/info/exclude` (the local-only ignore list) rather
/// than the committed `.gitignore` so the rule follows each clone
/// without requiring a team-wide .gitignore update. This is the same
/// pattern most tooling uses for auto-generated dev scaffolding.
const BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS: &[&str] = &[
    ".mcp.json",
    ".agents/mcp_config.json",
    "opencode.json",
    ".antigravitycli",
    ".claude/settings.local.json",
];

/// Stable prefix for the auto-managed header written into `.git/info/exclude`.
///
/// The full header text is "# Brehon local scaffolding (auto-managed; safe to edit)".
/// We intentionally match only on this prefix so that users may edit the
/// parenthetical note without breaking idempotence — reruns will recognise the
/// existing block and skip appending a duplicate header.
const BREHON_GITIGNORE_HEADER_PREFIX: &str = "# Brehon local scaffolding";

/// Ensure all Brehon-generated machine-local files are git-ignored
/// via `.git/info/exclude`.
///
/// No-ops silently when the target directory is not a git repository —
/// we'd rather skip than pollute a non-git dir with a spurious
/// `.git/info/exclude` file.
pub(crate) fn ensure_brehon_ignored_in_repo(repo_root: &Path) -> Result<()> {
    if !repo_root.join(".git").exists() {
        // `brehon run` can be invoked from a non-git directory; that's
        // legal and shouldn't trigger filesystem writes just to set up
        // a gitignore rule that has no home.
        return Ok(());
    }

    let info_dir = brehon_git::resolve_git_info_dir(repo_root)
        .map_err(|err| anyhow::anyhow!("Failed to resolve git info directory: {err}"))?;
    std::fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let (mut updated, removed_legacy) = brehon_git::remove_legacy_brehon_dir_ignores(&existing);

    // Collect the trimmed set of lines already present so we only write
    // patterns that are genuinely missing — and preserve anything else
    // the developer has put in their exclude file (custom tool caches,
    // editor scratch files, etc.).
    let already_present: std::collections::HashSet<&str> =
        updated.lines().map(|line| line.trim()).collect();

    let missing: Vec<&&str> = brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS
        .iter()
        .chain(BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS.iter())
        .filter(|pattern| !already_present.contains(**pattern))
        .collect();

    if missing.is_empty() && !removed_legacy {
        return Ok(());
    }

    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated
        .lines()
        .any(|l| l.trim().starts_with(BREHON_GITIGNORE_HEADER_PREFIX))
    {
        updated.push_str("# Brehon local scaffolding (auto-managed; safe to edit)\n");
    }
    for pattern in missing {
        updated.push_str(pattern);
        updated.push('\n');
    }
    std::fs::write(&exclude_path, updated)?;
    Ok(())
}

pub(crate) fn ensure_branch_checked_out(worktree_path: &Path, branch: &str) -> Result<()> {
    let current = git_current_branch(worktree_path).with_context(|| {
        format!(
            "Failed to inspect branch for worktree '{}'",
            worktree_path.display()
        )
    })?;
    if current != branch {
        anyhow::bail!(
            "Initiative worktree '{}' is on branch '{}' instead of '{}'",
            worktree_path.display(),
            current,
            branch
        );
    }
    Ok(())
}

fn task_has_unconsolidated_review_round(brehon_root: &Path, task_id: &str) -> bool {
    let review_dir = brehon_root.join("runtime").join("reviews").join(task_id);
    let Ok(entries) = std::fs::read_dir(review_dir) else {
        return false;
    };

    let latest_round = entries.flatten().filter_map(|entry| {
        let path = entry.path();
        if !path.is_dir() {
            return None;
        }
        let name = path.file_name().and_then(|name| name.to_str())?;
        let round = name
            .strip_prefix("round-")
            .and_then(|suffix| suffix.parse::<u32>().ok())?;
        Some((round, path))
    });
    let Some((_, path)) = latest_round.max_by_key(|(round, _)| *round) else {
        return false;
    };
    path.join("request.json").exists() && !path.join("consolidated.json").exists()
}

pub(crate) fn reconcile_orphaned_worker_assignments_for_run(
    brehon_root: &Path,
    current_worker_names: &[String],
) -> Result<Vec<String>> {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    if !tasks_dir.exists() {
        return Ok(Vec::new());
    }

    let live_workers: std::collections::HashSet<&str> =
        current_worker_names.iter().map(String::as_str).collect();
    let mut repaired = Vec::new();

    for entry in std::fs::read_dir(&tasks_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let mut task: serde_json::Value = match serde_json::from_str(&content) {
            Ok(task) => task,
            Err(_) => continue,
        };

        let status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending");
        if is_terminal_task_status(status) {
            continue;
        }

        let Some(assignee) = task
            .get("assignee")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
        else {
            continue;
        };

        if live_workers.contains(assignee.as_str()) {
            continue;
        }

        let Some(normalized_status) = normalize_task_status(status) else {
            continue;
        };
        if !matches!(
            normalized_status,
            "assigned" | "in_progress" | "changes_requested" | "blocked"
        ) {
            continue;
        }

        let task_id = task
            .get("task_id")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        if task_has_unconsolidated_review_round(brehon_root, &task_id) {
            continue;
        }

        task["orphaned_assignee"] = serde_json::Value::String(assignee.clone());
        task["orphaned_status"] = serde_json::Value::String(normalized_status.to_string());
        task["assignee"] = serde_json::Value::Null;
        task["inbox_delivered"] = serde_json::Value::Bool(false);
        task.as_object_mut().map(|object| object.remove("activity"));
        if task_has_recoverable_worker_state_blocker_text(&task) {
            task.as_object_mut().map(|object| object.remove("blockers"));
        }

        let has_blocked_by = task
            .get("blocked_by")
            .and_then(|value| value.as_array())
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        let has_manual_blockers = task
            .get("blockers")
            .and_then(|value| value.as_str())
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);

        let next_status = if normalized_status == "changes_requested" {
            "changes_requested"
        } else if has_blocked_by || has_manual_blockers {
            "blocked"
        } else {
            "pending"
        };
        task["status"] = serde_json::Value::String(next_status.to_string());
        task["recovery_note"] = serde_json::Value::String(format!(
            "Recovered orphaned task from {normalized_status}: previous assignee {assignee} was no longer live at startup. Returned to {next_status} for reassignment."
        ));
        task["updated_at"] = serde_json::Value::String(
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        brehon_types::write_json_atomic(&path, &task)?;
        repaired.push(format!(
            "Recovered orphaned task {task_id} from {normalized_status}: previous assignee {assignee} was no longer live. Returned to {next_status}."
        ));
    }

    Ok(repaired)
}

fn task_has_recoverable_worker_state_blocker_text(task: &serde_json::Value) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    blockers_lower.contains("state deadlock")
        || blockers_lower.contains("assignment mismatch")
        || blockers_lower.contains("complete call reports task assigned")
        || blockers_lower.contains("reports task assigned to")
        || blockers_lower.contains("empty string instead")
        || blockers_lower.contains("cannot checkpoint/complete")
        || blockers_lower.contains("checkpoint created during pending state")
        || blockers_lower.contains("could not move it to review_ready")
        || blockers_lower.contains("requires supervisor reassignment")
        || blockers_lower.contains("need reassignment to complete")
        || blockers_lower.contains("not permitted to complete")
        || blockers_lower.contains("ownership drift")
        || blockers_lower.contains("preventing worker completion handoff")
        || (blockers_lower.contains("invalid status transition")
            && blockers_lower.contains("'pending'")
            && blockers_lower.contains("'in_progress'"))
        || (blockers_lower.contains("invalid status transition")
            && blockers_lower.contains("'blocked'")
            && blockers_lower.contains("'in_progress'"))
        || (blockers_lower.contains("invalid transition")
            && blockers_lower.contains("blocked")
            && blockers_lower.contains("in_progress"))
}

pub(crate) fn scoped_worktree_path(
    worktrees_dir: &Path,
    session_scope: Option<&str>,
    scope: Option<&str>,
    name: &str,
) -> PathBuf {
    let mut path = worktrees_dir.to_path_buf();
    if let Some(session_scope) = session_scope.filter(|s| !s.is_empty()) {
        path = path.join("runs").join(session_scope);
    }
    match scope {
        Some(scope) if !scope.is_empty() => path.join(scope).join(name),
        _ => path.join(name),
    }
}

fn open_git_repo(cwd: &Path, operation: &str) -> Result<git2::Repository> {
    git2::Repository::discover(cwd).with_context(|| {
        format!(
            "Failed to open git repository at '{}' while {operation}",
            cwd.display()
        )
    })
}

/// Compute a deterministic repo identity for external worktree root scoping.
///
/// Normalize a potentially `.brehon`-suffixed path back to the actual
/// project root.  Brehon accepts `.brehon` as a project path (e.g.
/// `brehon run --config .brehon`), so callers must normalize before
/// deriving repo identity or resolving relative paths.
pub(crate) fn normalize_project_root(cwd: &Path) -> PathBuf {
    // If the supplied path ends with `.brehon`, its parent is the real root.
    if cwd.file_name() == Some(std::ffi::OsStr::new(".brehon")) {
        return cwd
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
    }
    cwd.to_path_buf()
}

#[cfg_attr(not(test), allow(dead_code))]
const REPO_IDENTITY_CACHE_FILE: &str = "runtime/repo-identity-cache.json";

#[cfg_attr(not(test), allow(dead_code))]
#[derive(serde::Serialize, serde::Deserialize)]
struct RepoIdentityCache {
    repo_name: String,
    identity: String,
}

#[cfg_attr(not(test), allow(dead_code))]
fn read_repo_identity_cache(brehon_root: &Path, repo_name: &str) -> Option<String> {
    let path = brehon_root.join(REPO_IDENTITY_CACHE_FILE);
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: RepoIdentityCache = serde_json::from_str(&content).ok()?;
    if cache.repo_name == repo_name {
        Some(cache.identity)
    } else {
        None
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_repo_identity_cache(brehon_root: &Path, repo_name: &str, identity: &str) {
    if !brehon_root.exists() {
        return;
    }
    let path = brehon_root.join(REPO_IDENTITY_CACHE_FILE);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let doc = RepoIdentityCache {
        repo_name: repo_name.to_string(),
        identity: identity.to_string(),
    };
    if let Ok(body) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(&path, body);
    }
}

/// Returns `{repo_name}-{short_hash}` where `repo_name` is the project
/// directory basename and `short_hash` is the first 8 hex chars of a
/// deterministic hash derived from the repository's origin remote URL.
/// Using the remote URL ensures the identity stays stable across clones
/// (including shallow clones) and does not change as new commits are made.
///
/// Falls back to a hash of the canonical path when git is unavailable,
/// the repository has no origin remote, or the remote has no URL.
/// **Note:** the fallback uses `std::collections::hash_map::DefaultHasher`,
/// whose algorithm is not guaranteed stable across Rust compiler versions.
/// In practice the value is stable for a given toolchain, but pinning the
/// identity explicitly via `orchestration.worktree_root` is recommended for
/// environments where reproducibility across compiler upgrades matters.
///
/// The resolved identity is cached in `.brehon/runtime/repo-identity-cache.json`.
/// Delete that file (or run `brehon clean`) to invalidate the cache.
///
/// **Callers must pass a normalized project root.** This function does
/// NOT call `normalize_project_root` internally; callers are responsible
/// for stripping any trailing `.brehon` segment before passing `cwd`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compute_repo_identity(cwd: &Path) -> String {
    let root = cwd.to_path_buf();
    let repo_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    let brehon_root = root.join(".brehon");
    if let Some(cached) = read_repo_identity_cache(&brehon_root, &repo_name) {
        return cached;
    }

    let hash = git2::Repository::discover(&root)
        .ok()
        .and_then(|repo| {
            repo.find_remote("origin")
                .ok()
                .and_then(|remote| remote.url().map(String::from))
        })
        .map(|url| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            url.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        })
        .unwrap_or_else(|| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            let mut hasher = DefaultHasher::new();
            canonical.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        });
    let hash = hash.get(..8).map(String::from).unwrap_or(hash);

    let identity = format!("{repo_name}-{hash}");
    write_repo_identity_cache(&brehon_root, &repo_name, &identity);
    identity
}

pub(crate) fn effective_worktree_root(project_root: &Path, config: &BrehonConfig) -> PathBuf {
    let project_root = normalize_project_root(project_root);
    let repo_identity = compute_repo_identity(&project_root);
    config
        .orchestration
        .resolve_worktree_root(&project_root, &repo_identity)
}

pub(crate) fn detect_default_branch(cwd: &Path) -> Result<String> {
    let repo = open_git_repo(cwd, "detecting the default branch")?;

    if let Ok(reference) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(stripped) = reference
            .symbolic_target()
            .and_then(|target| target.strip_prefix("refs/remotes/origin/"))
        {
            return Ok(stripped.to_string());
        }
    }

    for candidate in ["main", "master", "develop"] {
        let ref_path = format!("refs/heads/{candidate}");
        if repo.find_reference(&ref_path).is_ok() {
            return Ok(candidate.to_string());
        }
    }

    Ok("main".to_string())
}

pub(crate) fn git_current_branch(cwd: &Path) -> Result<String> {
    let repo = open_git_repo(cwd, "reading the current branch")?;
    let head = match repo.head() {
        Ok(head) => head,
        Err(err) if err.code() == git2::ErrorCode::UnbornBranch => return Ok(String::new()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to read HEAD for git repository at '{}'",
                    cwd.display()
                )
            });
        }
    };

    if !head.is_branch() {
        return Ok(String::new());
    }

    Ok(head.shorthand().unwrap_or_default().to_string())
}

pub(crate) fn git_branch_exists(cwd: &Path, branch: &str) -> bool {
    let Ok(repo) = open_git_repo(cwd, "checking whether a branch exists") else {
        return false;
    };
    let ref_path = format!("refs/heads/{branch}");
    let exists = repo.find_reference(&ref_path).is_ok();
    exists
}

pub(crate) fn git_is_clean(cwd: &Path) -> Result<bool> {
    let repo = open_git_repo(cwd, "checking worktree cleanliness")?;
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo.statuses(Some(&mut options)).with_context(|| {
        format!(
            "Failed to read git status for repository at '{}'",
            cwd.display()
        )
    })?;
    Ok(statuses.is_empty())
}

/// Relative path (under cwd) of the Claude Code settings file Brehon edits.
const CLAUDE_SETTINGS_RELATIVE: &str = ".claude/settings.local.json";

/// Marker token Brehon embeds in the hook command so it can be located and
/// removed cleanly without disturbing user-added hooks.
const CLAUDE_HOOK_MARKER: &str = "brehon claude-hook";

/// Claude tools that can mutate repository files or spawn unmanaged
/// subagent worktrees and must therefore pass through the Brehon guard.
const CLAUDE_HOOK_MATCHERS: &[&str] =
    &["Bash", "Edit", "MultiEdit", "Write", "NotebookEdit", "Task"];

/// Relative path (under cwd) of the runtime marker file the `claude-hook`
/// binary checks before applying its policy. When this file is absent, the
/// hook falls through — that's how we keep the hook a no-op outside an
/// active `brehon run`.
const CLAUDE_HOOK_ACTIVE_MARKER_RELATIVE: &str = ".brehon/runtime/claude-hook-active";

/// Install a Claude Code `PreToolUse` hook pointing at `brehon claude-hook`.
///
/// Idempotent. Preserves any existing `hooks.PreToolUse` entries the user or
/// other tools added; we only touch the entry whose command contains
/// [`CLAUDE_HOOK_MARKER`].
pub(crate) fn ensure_claude_worktree_hook(cwd: &Path) -> Result<()> {
    let brehon_bin = std::env::current_exe().context(
        "Failed to resolve current brehon binary path; Claude worktree hook cannot be installed",
    )?;
    let hook_command = claude_hook_command(cwd, &brehon_bin);

    let settings_path = cwd.join(CLAUDE_SETTINGS_RELATIVE);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create directory '{}' for Claude settings",
                parent.display()
            )
        })?;
    }

    let mut doc: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path).with_context(|| {
            format!(
                "Failed to read Claude settings at '{}'",
                settings_path.display()
            )
        })?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Walk: doc.hooks.PreToolUse — create as needed.
    let root = doc.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "Claude settings at '{}' is not a JSON object",
            settings_path.display()
        )
    })?;
    let hooks_root = root
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude settings `hooks` key is not an object"))?;
    let pretooluse = hooks_root
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude settings `hooks.PreToolUse` is not an array"))?;

    // Remove any prior Brehon-owned entry so we re-install with the current
    // binary path. Prevents drift when the brehon binary is rebuilt to a
    // different location between runs.
    pretooluse.retain(|entry| !entry_contains_brehon_marker(entry));

    for matcher in CLAUDE_HOOK_MATCHERS {
        pretooluse.push(serde_json::json!({
            "matcher": matcher,
            "hooks": [
                { "type": "command", "command": hook_command }
            ]
        }));
    }

    std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?).with_context(|| {
        format!(
            "Failed to write Claude settings at '{}'",
            settings_path.display()
        )
    })?;
    tracing::info!(
        path = %settings_path.display(),
        matchers = ?CLAUDE_HOOK_MATCHERS,
        "Installed Brehon Claude PreToolUse hook"
    );
    Ok(())
}

fn entry_contains_brehon_marker(entry: &serde_json::Value) -> bool {
    let inner = entry.get("hooks").and_then(|h| h.as_array());
    if let Some(arr) = inner {
        for h in arr {
            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                if command_contains_brehon_hook_marker(cmd) {
                    return true;
                }
            }
        }
    }
    false
}

fn command_contains_brehon_hook_marker(cmd: &str) -> bool {
    cmd.contains(CLAUDE_HOOK_MARKER)
        || (cmd.contains("brehon") && cmd.split_whitespace().last() == Some("claude-hook"))
}

fn claude_hook_command(cwd: &Path, brehon_bin: &Path) -> String {
    let brehon_root = cwd.join(".brehon");
    let mut env = vec![
        format!("BREHON_ROOT={}", shell_quote(&brehon_root)),
        format!("BREHON_PROJECT_ROOT={}", shell_quote(cwd)),
        "BREHON_WORKSPACE_ROOT=\"${BREHON_WORKSPACE_ROOT:-$PWD}\"".to_string(),
        "BREHON_AGENT_ROLE=\"${BREHON_AGENT_ROLE:-}\"".to_string(),
        "BREHON_MERGE_TARGET=\"${BREHON_MERGE_TARGET:-}\"".to_string(),
    ];
    if let Some(worktree_root) =
        std::env::var_os("BREHON_WORKTREE_ROOT").filter(|value| !value.is_empty())
    {
        env.push(format!(
            "BREHON_WORKTREE_ROOT={}",
            shell_quote(Path::new(&worktree_root))
        ));
    }
    env.push(shell_quote(brehon_bin));
    env.push("claude-hook".to_string());
    env.join(" ")
}

fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// RAII guard that activates the Claude worktree hook by writing the active
/// marker. Drops the marker on drop so the hook becomes a no-op as soon as
/// `brehon run` exits (graceful or otherwise — Drop runs on panic too).
pub(crate) struct ClaudeWorktreeHookActivation {
    marker_path: PathBuf,
}

impl Drop for ClaudeWorktreeHookActivation {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker_path);
    }
}

pub(crate) fn activate_claude_worktree_hook(cwd: &Path) -> Result<ClaudeWorktreeHookActivation> {
    let marker_path = cwd.join(CLAUDE_HOOK_ACTIVE_MARKER_RELATIVE);
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create Claude hook marker directory '{}'",
                parent.display()
            )
        })?;
    }
    let body = format!(
        "pid={}\nstarted_at={}\n",
        std::process::id(),
        chrono::Utc::now().to_rfc3339()
    );
    std::fs::write(&marker_path, body).with_context(|| {
        format!(
            "Failed to write Claude hook active marker '{}'",
            marker_path.display()
        )
    })?;
    Ok(ClaudeWorktreeHookActivation { marker_path })
}

/// Remove Brehon's Claude `PreToolUse` hook entry from settings and delete
/// the active marker. Called from `brehon clean` / `brehon reset`. Idempotent.
pub(crate) fn remove_claude_worktree_hook(cwd: &Path) -> Result<()> {
    // Marker first — easy, always safe.
    let marker_path = cwd.join(CLAUDE_HOOK_ACTIVE_MARKER_RELATIVE);
    let _ = std::fs::remove_file(&marker_path);

    let settings_path = cwd.join(CLAUDE_SETTINGS_RELATIVE);
    if !settings_path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&settings_path).with_context(|| {
        format!(
            "Failed to read Claude settings at '{}' while removing Brehon hook",
            settings_path.display()
        )
    })?;
    let mut doc: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut changed = false;
    if let Some(pretooluse) = doc
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(|p| p.as_array_mut())
    {
        let before = pretooluse.len();
        pretooluse.retain(|entry| !entry_contains_brehon_marker(entry));
        if pretooluse.len() != before {
            changed = true;
        }
        // Clean up empty containers so we don't leave noise in settings.
        if pretooluse.is_empty() {
            if let Some(hooks_obj) = doc.get_mut("hooks").and_then(|h| h.as_object_mut()) {
                hooks_obj.remove("PreToolUse");
            }
        }
    }
    if let Some(hooks_obj) = doc.get("hooks").and_then(|h| h.as_object()) {
        if hooks_obj.is_empty() {
            if let Some(root) = doc.as_object_mut() {
                root.remove("hooks");
            }
            changed = true;
        }
    }

    if changed {
        std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?).with_context(|| {
            format!(
                "Failed to rewrite Claude settings at '{}' after removing Brehon hook",
                settings_path.display()
            )
        })?;
        tracing::info!(
            path = %settings_path.display(),
            "Removed Brehon Claude PreToolUse hook"
        );
    }
    Ok(())
}

pub(crate) fn ensure_protected_branch_hooks(cwd: &Path, default_branch: &str) -> Result<()> {
    let hooks_dir = git_common_dir(cwd)?.join("hooks");
    std::fs::create_dir_all(&hooks_dir).with_context(|| {
        format!(
            "Failed to create git hooks directory '{}' for protected branch guard",
            hooks_dir.display()
        )
    })?;

    for hook_name in BREHON_PROTECTED_BRANCH_HOOKS {
        install_protected_branch_hook(&hooks_dir, hook_name, default_branch)?;
    }

    Ok(())
}

pub(crate) struct ProtectedBranchGuardActivation {
    marker_path: PathBuf,
}

impl Drop for ProtectedBranchGuardActivation {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker_path);
    }
}

pub(crate) fn activate_protected_branch_guard(
    cwd: &Path,
    session_name: &str,
) -> Result<ProtectedBranchGuardActivation> {
    let marker_path = protected_branch_guard_marker_path(cwd)?;
    if let Some(parent) = marker_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create protected branch guard marker directory '{}'",
                parent.display()
            )
        })?;
    }

    let marker = format!(
        "pid={}\nsession={}\nworkspace={}\n",
        std::process::id(),
        session_name,
        cwd.display()
    );
    std::fs::write(&marker_path, marker).with_context(|| {
        format!(
            "Failed to write protected branch guard marker '{}'",
            marker_path.display()
        )
    })?;

    Ok(ProtectedBranchGuardActivation { marker_path })
}

pub(crate) fn remove_protected_branch_hooks(cwd: &Path) -> Result<Vec<PathBuf>> {
    let git_common_dir = git_common_dir(cwd)?;
    let hooks_dir = git_common_dir.join("hooks");
    let mut removed = Vec::new();

    for hook_name in BREHON_PROTECTED_BRANCH_HOOKS {
        let hook_path = hooks_dir.join(hook_name);
        if !hook_path.exists() {
            continue;
        }

        let existing = std::fs::read_to_string(&hook_path).with_context(|| {
            format!(
                "Failed to read existing git hook '{}' while removing protected branch guard",
                hook_path.display()
            )
        })?;
        let (shebang, original_body) = split_hook_shebang(&existing);
        let stripped = strip_existing_protected_branch_guard(&original_body);
        if stripped == original_body {
            continue;
        }

        if stripped.trim().is_empty() {
            std::fs::remove_file(&hook_path).with_context(|| {
                format!(
                    "Failed to remove empty Brehon protected branch hook '{}'",
                    hook_path.display()
                )
            })?;
        } else {
            std::fs::write(&hook_path, format!("{shebang}\n{stripped}")).with_context(|| {
                format!(
                    "Failed to rewrite git hook '{}' while removing protected branch guard",
                    hook_path.display()
                )
            })?;
            make_hook_executable(&hook_path)?;
        }
        removed.push(hook_path);
    }

    let marker_path = git_common_dir
        .join("brehon")
        .join(BREHON_PROTECTED_BRANCH_GUARD_MARKER);
    if marker_path.exists() {
        let _ = std::fs::remove_file(marker_path);
    }

    Ok(removed)
}

pub(crate) fn protected_branch_hooks_installed(cwd: &Path) -> Result<bool> {
    let git_common_dir = git_common_dir(cwd)?;
    let hooks_dir = git_common_dir.join("hooks");

    for hook_name in BREHON_PROTECTED_BRANCH_HOOKS {
        let hook_path = hooks_dir.join(hook_name);
        let Ok(existing) = std::fs::read_to_string(&hook_path) else {
            continue;
        };
        if existing.contains(BREHON_PROTECTED_BRANCH_GUARD_BEGIN)
            && existing.contains(BREHON_PROTECTED_BRANCH_GUARD_END)
        {
            return Ok(true);
        }
    }

    Ok(git_common_dir
        .join("brehon")
        .join(BREHON_PROTECTED_BRANCH_GUARD_MARKER)
        .exists())
}

fn git_common_dir(cwd: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .with_context(|| {
            format!(
                "Failed to run git rev-parse --git-common-dir for '{}'",
                cwd.display()
            )
        })?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to resolve git common directory for '{}': {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(cwd.join(path))
    }
}

fn protected_branch_guard_marker_path(cwd: &Path) -> Result<PathBuf> {
    Ok(git_common_dir(cwd)?
        .join("brehon")
        .join(BREHON_PROTECTED_BRANCH_GUARD_MARKER))
}

fn install_protected_branch_hook(
    hooks_dir: &Path,
    hook_name: &str,
    default_branch: &str,
) -> Result<()> {
    let hook_path = hooks_dir.join(hook_name);
    let existing = if hook_path.exists() {
        Some(std::fs::read_to_string(&hook_path).with_context(|| {
            format!(
                "Failed to read existing git hook '{}' while installing protected branch guard",
                hook_path.display()
            )
        })?)
    } else {
        None
    };

    let script = build_protected_branch_hook_script(hook_name, existing.as_deref(), default_branch);
    std::fs::write(&hook_path, script).with_context(|| {
        format!(
            "Failed to write protected branch git hook '{}'",
            hook_path.display()
        )
    })?;
    make_hook_executable(&hook_path)?;
    Ok(())
}

fn build_protected_branch_hook_script(
    hook_name: &str,
    existing: Option<&str>,
    default_branch: &str,
) -> String {
    let (shebang, original_body) = existing
        .map(split_hook_shebang)
        .unwrap_or_else(|| ("#!/bin/sh".to_string(), String::new()));
    let original_body = strip_existing_protected_branch_guard(&original_body);
    let guard = protected_branch_guard_body(hook_name, default_branch);

    if original_body.trim().is_empty() {
        format!("{shebang}\n{guard}\n")
    } else {
        format!("{shebang}\n{guard}\n{original_body}")
    }
}

fn split_hook_shebang(content: &str) -> (String, String) {
    if content.starts_with("#!") {
        if let Some((first, rest)) = content.split_once('\n') {
            return (first.to_string(), rest.to_string());
        }
        return (content.to_string(), String::new());
    }

    ("#!/bin/sh".to_string(), content.to_string())
}

fn strip_existing_protected_branch_guard(content: &str) -> String {
    let Some(begin) = content.find(BREHON_PROTECTED_BRANCH_GUARD_BEGIN) else {
        return content.to_string();
    };
    let Some(end_relative) = content[begin..].find(BREHON_PROTECTED_BRANCH_GUARD_END) else {
        return content.to_string();
    };

    let end = begin + end_relative + BREHON_PROTECTED_BRANCH_GUARD_END.len();
    let mut stripped = String::new();
    stripped.push_str(&content[..begin]);
    stripped.push_str(content[end..].trim_start_matches(['\r', '\n']));
    stripped
}

fn protected_branch_guard_body(hook_name: &str, default_branch: &str) -> String {
    let fallback = shell_double_quote_fragment(&protected_branch_fallback(default_branch));
    let active_guard = protected_branch_guard_active_shell();
    let bypass_guard = protected_branch_bypass_shell();
    if hook_name == "reference-transaction" {
        return format!(
            r#"{BREHON_PROTECTED_BRANCH_GUARD_BEGIN}
{active_guard}
{bypass_guard}
brehon_ref_txn_input="$(mktemp "${{TMPDIR:-/tmp}}/brehon-ref-transaction.XXXXXX")" || exit 1
cat > "$brehon_ref_txn_input"
trap 'rm -f "$brehon_ref_txn_input"' EXIT HUP INT TERM
if [ "$brehon_protected_branch_bypass_valid" != "1" ] && [ "$brehon_protected_branch_guard_active" = "1" ] && [ "${{1:-}}" = "prepared" ]; then
    brehon_protected_branches="${{BREHON_PROTECTED_BRANCHES:-}}"
    if [ -z "$brehon_protected_branches" ]; then
        brehon_protected_branches="$(git config --get brehon.protectedBranches 2>/dev/null || true)"
    fi
    if [ -z "$brehon_protected_branches" ]; then
        brehon_protected_branches="{fallback}"
    fi
    while read -r brehon_old_ref brehon_new_ref brehon_ref_name; do
        case "$brehon_ref_name" in
            refs/heads/*)
                brehon_ref_branch="${{brehon_ref_name#refs/heads/}}"
                for brehon_protected_branch in $brehon_protected_branches; do
                    if [ "$brehon_ref_branch" = "$brehon_protected_branch" ]; then
                        echo "Brehon protected branch guard: refusing to update protected branch '$brehon_ref_branch'." >&2
                        echo "Use Brehon's task integration/close flow; generic BREHON_ALLOW_PROTECTED_BRANCH_COMMIT is not enough without a Brehon lease token." >&2
                        exit 1
                    fi
                done
                ;;
        esac
    done < "$brehon_ref_txn_input"
fi
unset brehon_old_ref brehon_new_ref brehon_ref_name brehon_ref_branch brehon_protected_branches brehon_protected_branch
unset brehon_protected_branch_bypass_valid brehon_bypass_token brehon_bypass_dir brehon_bypass_path brehon_bypass_pid
unset brehon_protected_branch_guard_active brehon_git_common_dir brehon_git_root brehon_guard_marker brehon_guard_pid brehon_guard_line
exec < "$brehon_ref_txn_input"
{BREHON_PROTECTED_BRANCH_GUARD_END}"#
        );
    }

    format!(
        r#"{BREHON_PROTECTED_BRANCH_GUARD_BEGIN}
{active_guard}
{bypass_guard}
if [ "$brehon_protected_branch_bypass_valid" != "1" ] && [ "$brehon_protected_branch_guard_active" = "1" ]; then
    brehon_current_branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)"
    if [ -n "$brehon_current_branch" ]; then
        brehon_protected_branches="${{BREHON_PROTECTED_BRANCHES:-}}"
        if [ -z "$brehon_protected_branches" ]; then
            brehon_protected_branches="$(git config --get brehon.protectedBranches 2>/dev/null || true)"
        fi
        if [ -z "$brehon_protected_branches" ]; then
            brehon_protected_branches="{fallback}"
        fi
        for brehon_protected_branch in $brehon_protected_branches; do
            if [ "$brehon_current_branch" = "$brehon_protected_branch" ]; then
                echo "Brehon protected branch guard: refusing to create a commit on '$brehon_current_branch'." >&2
                echo "Use Brehon's task integration/close flow; generic BREHON_ALLOW_PROTECTED_BRANCH_COMMIT is not enough without a Brehon lease token." >&2
                exit 1
            fi
        done
    fi
fi
unset brehon_current_branch brehon_protected_branches brehon_protected_branch
unset brehon_protected_branch_bypass_valid brehon_bypass_token brehon_bypass_dir brehon_bypass_path brehon_bypass_pid
unset brehon_protected_branch_guard_active brehon_git_common_dir brehon_git_root brehon_guard_marker brehon_guard_pid brehon_guard_line
{BREHON_PROTECTED_BRANCH_GUARD_END}"#
    )
}

fn protected_branch_guard_active_shell() -> &'static str {
    r#"brehon_protected_branch_guard_active=0
brehon_bypass_token="${BREHON_PROTECTED_BRANCH_BYPASS_TOKEN:-}"
brehon_bypass_dir="${BREHON_PROTECTED_BRANCH_BYPASS_DIR:-}"
case "$brehon_bypass_token" in *[!A-Za-z0-9_.-]*|"") brehon_bypass_token="" ;; esac
if [ "${BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}" = "1" ] && [ -n "$brehon_bypass_token" ] && [ -n "$brehon_bypass_dir" ] && [ -f "$brehon_bypass_dir/$brehon_bypass_token" ]; then
    brehon_bypass_dir="${brehon_bypass_dir%/}"
    brehon_git_common_dir="${brehon_bypass_dir%/brehon/protected-branch-bypass}"
else
    brehon_git_common_dir="$(git rev-parse --git-common-dir 2>/dev/null || true)"
    case "$brehon_git_common_dir" in ""|/*) ;; *) brehon_git_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"; brehon_git_common_dir="$brehon_git_root/$brehon_git_common_dir" ;; esac
fi
brehon_guard_marker="$brehon_git_common_dir/brehon/protected-branch-guard-active"
if [ -n "$brehon_git_common_dir" ] && [ -f "$brehon_guard_marker" ]; then
    brehon_guard_pid=""
    while IFS= read -r brehon_guard_line; do
        case "$brehon_guard_line" in
            pid=*) brehon_guard_pid="${brehon_guard_line#pid=}"; break ;;
        esac
    done < "$brehon_guard_marker" || true
    case "$brehon_guard_pid" in
        ""|*[!0-9]*) ;;
        *) kill -0 "$brehon_guard_pid" 2>/dev/null && brehon_protected_branch_guard_active=1 ;;
    esac
fi"#
}

fn protected_branch_bypass_shell() -> String {
    format!(
        r#"brehon_protected_branch_bypass_valid=0
if [ "${{BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}}" = "1" ] && [ -n "${{BREHON_PROTECTED_BRANCH_BYPASS_TOKEN:-}}" ]; then
    brehon_bypass_token="$BREHON_PROTECTED_BRANCH_BYPASS_TOKEN"
    case "$brehon_bypass_token" in
        *[!A-Za-z0-9_.-]*|"") ;;
        *)
            brehon_bypass_dir="${{BREHON_PROTECTED_BRANCH_BYPASS_DIR:-}}"
            [ -n "$brehon_bypass_dir" ] || brehon_bypass_dir="${{brehon_git_common_dir:+$brehon_git_common_dir/brehon/{BREHON_PROTECTED_BRANCH_BYPASS_DIR}}}"
            brehon_bypass_path="${{brehon_bypass_dir:+$brehon_bypass_dir/$brehon_bypass_token}}"
            if [ -f "$brehon_bypass_path" ]; then
                brehon_bypass_pid=""
                while IFS= read -r brehon_guard_line; do
                    case "$brehon_guard_line" in
                        pid=*)
                            brehon_bypass_pid="${{brehon_guard_line#pid=}}"
                            break
                            ;;
                    esac
                done < "$brehon_bypass_path" || true
                case "$brehon_bypass_pid" in
                    ""|*[!0-9]*) ;;
                    *) kill -0 "$brehon_bypass_pid" 2>/dev/null && brehon_protected_branch_bypass_valid=1 ;;
                esac
            fi
            ;;
    esac
fi"#
    )
}

fn shell_double_quote_fragment(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn protected_branch_fallback(default_branch: &str) -> String {
    let mut branches = Vec::new();
    for branch in [default_branch, "main", "master", "develop"] {
        if !branch.trim().is_empty() && !branches.contains(&branch) {
            branches.push(branch);
        }
    }
    branches.join(" ")
}

fn make_hook_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(path)
            .with_context(|| format!("Failed to read permissions for '{}'", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("Failed to mark git hook '{}' executable", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

pub(crate) fn switch_branch(cwd: &Path, branch: &str) -> Result<()> {
    let repo = open_git_repo(cwd, "switching branches")?;
    brehon_git::BranchOps::new(&repo)
        .checkout(branch)
        .with_context(|| format!("Failed to switch '{}' to branch '{branch}'", cwd.display()))?;
    Ok(())
}

pub(crate) fn ensure_shared_root_on_default_branch(cwd: &Path) -> Result<String> {
    ensure_brehon_ignored_in_repo(cwd).with_context(|| {
        format!(
            "Failed to prepare Brehon local git excludes before checking shared repo cleanliness at '{}'",
            cwd.display()
        )
    })?;

    let default_branch = detect_default_branch(cwd)?;
    let current_branch = git_current_branch(cwd)?;

    if current_branch == default_branch {
        if !git_is_clean(cwd)? {
            anyhow::bail!(
                "Shared repo root is on default branch '{}' but has local changes. \
                 Refusing to run with worktree isolation because workers must not mutate the shared checkout, \
                 and Brehon needs a clean baseline to detect worktree escapes.",
                default_branch
            );
        }
        return Ok(default_branch);
    }

    if !git_is_clean(cwd)? {
        anyhow::bail!(
            "Shared repo root is on '{}' instead of '{}' and has local changes. \
             Refusing to run because Brehon must not mutate the shared checkout while worktree isolation is enabled.",
            if current_branch.is_empty() {
                "detached HEAD"
            } else {
                &current_branch
            },
            default_branch
        );
    }

    tracing::warn!(
        current_branch = if current_branch.is_empty() { "detached HEAD" } else { &current_branch },
        default_branch = %default_branch,
        "Shared repo root drifted away from the default branch; restoring it before startup"
    );
    switch_branch(cwd, &default_branch)?;
    Ok(default_branch)
}

pub(crate) fn restore_shared_root_branch(cwd: &Path, default_branch: &str) -> Result<()> {
    let current_branch = git_current_branch(cwd)?;
    if current_branch == default_branch {
        if !git_is_clean(cwd)? {
            anyhow::bail!(
                "Shared repo root stayed on default branch '{}' but became dirty during Brehon run. \
                 This indicates an agent likely wrote outside its assigned worktree; refusing to hide the mutation.",
                default_branch
            );
        }
        return Ok(());
    }

    if !git_is_clean(cwd)? {
        anyhow::bail!(
            "Shared repo root drifted to '{}' during Brehon run and is now dirty. \
             Refusing automatic repair because that would hide a real mutation bug.",
            if current_branch.is_empty() {
                "detached HEAD"
            } else {
                &current_branch
            }
        );
    }

    tracing::error!(
        current_branch = if current_branch.is_empty() { "detached HEAD" } else { &current_branch },
        default_branch = %default_branch,
        "Shared repo root drifted during Brehon run; restoring default branch"
    );
    switch_branch(cwd, default_branch)
}

pub(crate) async fn reconcile_initiative_hierarchy_for_run(
    cwd: &Path,
    brehon_root: &Path,
    config: &BrehonConfig,
) -> Result<Vec<String>> {
    let tasks_dir = brehon_root.join("runtime").join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(Vec::new());
    }

    ensure_brehon_ignored_in_repo(cwd)?;
    let git = brehon_git::Git2Operations::open(cwd).map_err(|err| {
        anyhow::anyhow!(
            "Failed to open git repository at '{}' while reconciling initiatives: {err}",
            cwd.display()
        )
    })?;
    let default_branch = detect_default_branch(cwd)?;
    let project_root = normalize_project_root(cwd);
    let worktrees_root = effective_worktree_root(&project_root, config);

    let mut repaired = Vec::new();
    let entries = std::fs::read_dir(&tasks_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }

        let content = std::fs::read_to_string(&path)?;
        let mut task: serde_json::Value = serde_json::from_str(&content)?;
        let is_initiative =
            task.get("task_type").and_then(|value| value.as_str()) == Some("initiative");
        if !is_initiative {
            continue;
        }

        let status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending");
        if matches!(status, "merged" | "closed" | "completed" | "rejected") {
            continue;
        }

        let task_id = match task.get("task_id").and_then(|value| value.as_str()) {
            Some(value) => value.to_string(),
            None => continue,
        };
        let title = task
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("initiative")
            .to_string();

        let branch = task
            .get("integration_branch")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_initiative_integration_branch(&task_id, &title));
        let worktree_path = task
            .get("integration_worktree")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_initiative_integration_worktree(&worktrees_root, &task_id));

        if !git_branch_exists(cwd, &branch) {
            git.create_branch(&branch, Some(&default_branch))
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "Failed to create initiative branch '{}' from '{}' for {}: {err}",
                        branch,
                        default_branch,
                        task_id
                    )
                })?;
        }

        if worktree_path.exists() {
            ensure_branch_checked_out(&worktree_path, &branch)?;
        } else {
            if let Some(parent) = worktree_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            git.create_worktree(&branch, &worktree_path)
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "Failed to create initiative worktree '{}' for branch '{}': {err}",
                        worktree_path.display(),
                        branch
                    )
                })?;
        }

        let mut changed = false;
        if task
            .get("integration_branch")
            .and_then(|value| value.as_str())
            != Some(branch.as_str())
        {
            task["integration_branch"] = serde_json::json!(branch);
            changed = true;
        }
        if task
            .get("integration_worktree")
            .and_then(|value| value.as_str())
            != Some(worktree_path.to_string_lossy().as_ref())
        {
            task["integration_worktree"] =
                serde_json::json!(worktree_path.to_string_lossy().to_string());
            changed = true;
        }

        if changed {
            brehon_types::write_json_atomic(&path, &task)?;
            repaired.push(format!(
                "Backfilled initiative {} onto branch '{}' ({})",
                task_id,
                task["integration_branch"].as_str().unwrap_or(""),
                worktree_path.display()
            ));
        }
    }

    Ok(repaired)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn prepare_scoped_worktrees(
    cwd: &Path,
    config: &BrehonConfig,
    session_scope: Option<&str>,
    scope: Option<&str>,
    names: &[String],
) -> Result<HashMap<String, PathBuf>> {
    prepare_scoped_worktrees_with_progress(cwd, config, session_scope, scope, names, |_| {}).await
}

pub(crate) async fn prepare_scoped_worktrees_with_progress<F>(
    cwd: &Path,
    config: &BrehonConfig,
    session_scope: Option<&str>,
    scope: Option<&str>,
    names: &[String],
    mut report: F,
) -> Result<HashMap<String, PathBuf>>
where
    F: FnMut(String),
{
    if !config.orchestration.worktree_isolation {
        return Ok(HashMap::new());
    }

    let git = brehon_git::Git2Operations::open(cwd).map_err(|err| {
        anyhow::anyhow!(
            "Worktree isolation is enabled, but '{}' is not a usable git repository: {err}",
            cwd.display()
        )
    })?;

    let project_root = normalize_project_root(cwd);
    ensure_brehon_ignored_in_repo(&project_root)?;
    let worktrees_dir = effective_worktree_root(&project_root, config);
    std::fs::create_dir_all(&worktrees_dir).with_context(|| {
        format!(
            "Failed to create worktree directory '{}'",
            worktrees_dir.display()
        )
    })?;

    let mut role_cwds = HashMap::new();
    let scope_label = scope.unwrap_or("worker");
    for name in names {
        report(format!("Preparing {scope_label} workspace for {name}"));
        let branch = match scope {
            Some(role_segment) if !role_segment.is_empty() => role_branch_name(
                &config.orchestration.branch_prefix,
                session_scope,
                role_segment,
                name,
            ),
            _ => worker_branch_name(&config.orchestration.branch_prefix, session_scope, name),
        };
        let worktree_path = scoped_worktree_path(&worktrees_dir, session_scope, scope, name);
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create parent directory '{}' for {scope_label} worktree",
                    parent.display()
                )
            })?;
        }

        if worktree_path.exists() {
            if !config.orchestration.auto_cleanup_worktrees {
                return Err(anyhow::anyhow!(
                    "{} worktree '{}' already exists and auto-cleanup is disabled. \
                     Remove it or run with worktree isolation disabled.",
                    scope_label,
                    worktree_path.display()
                ));
            }

            report(format!(
                "Removing stale {scope_label} worktree {}",
                worktree_path.display()
            ));
            git.remove_worktree(&worktree_path).await.map_err(|err| {
                anyhow::anyhow!(
                    "Failed to remove stale {} worktree '{}' before startup: {err}",
                    scope_label,
                    worktree_path.display()
                )
            })?;
        }

        report(format!(
            "Creating {scope_label} worktree {name} on branch {branch}"
        ));
        git.create_worktree(&branch, &worktree_path)
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "Failed to create isolated {} worktree for '{}' at '{}': {err}. \
                     Brehon will not fall back to the shared repo root while worktree isolation is enabled.",
                    scope_label,
                    name,
                    worktree_path.display()
                )
            })?;
        git.validate_worktree(&worktree_path).map_err(|err| {
            anyhow::anyhow!(
                "Created {} worktree for '{}' at '{}' but validation failed: {err}. \
                 Brehon will not start with an invalid isolated worktree.",
                scope_label,
                name,
                worktree_path.display()
            )
        })?;
        sync_local_agent_scaffolding_to_worktree(
            &project_root,
            &worktree_path,
            &worktrees_dir,
            session_scope,
            scope,
            name,
            &config.roles.supervisor.name,
        )?;
        report(format!(
            "{scope_label} {name} ready at {}",
            worktree_path.display()
        ));

        role_cwds.insert(name.clone(), worktree_path);
    }

    Ok(role_cwds)
}

pub(crate) async fn cleanup_scoped_worktrees(cwd: &Path, role_cwds: &HashMap<String, PathBuf>) {
    if role_cwds.is_empty() {
        return;
    }

    let Ok(git) = brehon_git::Git2Operations::open(cwd) else {
        tracing::warn!(
            "Could not reopen git repository at '{}' to clean role worktrees",
            cwd.display()
        );
        return;
    };

    for (agent_name, worktree_path) in role_cwds {
        if !worktree_path.exists() {
            continue;
        }

        if let Err(err) = git.remove_worktree(worktree_path).await {
            tracing::warn!(
                agent = %agent_name,
                path = %worktree_path.display(),
                "Failed to auto-clean role worktree: {}",
                err
            );
            continue;
        }

        if worktree_path.exists() {
            let cleanup_result = if worktree_path.is_dir() {
                std::fs::remove_dir_all(worktree_path)
            } else {
                std::fs::remove_file(worktree_path)
            };
            if let Err(err) = cleanup_result {
                tracing::warn!(
                    agent = %agent_name,
                    path = %worktree_path.display(),
                    "Worktree metadata was removed, but deleting the path failed: {}",
                    err
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::OrchestrationConfig;

    fn default_security() -> brehon_types::config::SecurityConfig {
        brehon_config::parse_defaults().unwrap().security
    }

    fn isolated_git_command(path: &Path) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        command.current_dir(path);
        for key in [
            "BREHON_ALLOW_PROTECTED_BRANCH_COMMIT",
            "BREHON_PROTECTED_BRANCH_BYPASS_TOKEN",
            "BREHON_PROTECTED_BRANCH_BYPASS_DIR",
            "BREHON_PROTECTED_BRANCHES",
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(key);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = isolated_git_command(path)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-b", "main"]);
        run_git(path, &["config", "user.email", "brehon@example.invalid"]);
        run_git(path, &["config", "user.name", "Brehon Test"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        std::fs::write(
            path.join(".gitignore"),
            ".brehon/\n.claude/settings.local.json\n",
        )
        .unwrap();
        run_git(path, &["add", "README.md", ".gitignore"]);
        run_git(path, &["commit", "-m", "seed"]);
    }

    fn configure_test_external_worktree_root(config: &mut BrehonConfig, root: &Path) -> PathBuf {
        let root = root.join("brehon-worktrees");
        config.orchestration.worktree_root = Some(root.to_string_lossy().to_string());
        root
    }

    #[test]
    fn test_claude_hook_installs_all_mutating_tool_matchers() {
        let temp = tempfile::tempdir().unwrap();
        let settings_path = temp.path().join(CLAUDE_SETTINGS_RELATIVE);
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(
            &settings_path,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": "Bash",
                            "hooks": [
                                { "type": "command", "command": "/old/bin/brehon claude-hook" }
                            ]
                        },
                        {
                            "matcher": "Read",
                            "hooks": [
                                { "type": "command", "command": "/tmp/user-read-hook" }
                            ]
                        }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        ensure_claude_worktree_hook(temp.path()).unwrap();
        ensure_claude_worktree_hook(temp.path()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let entries = settings["hooks"]["PreToolUse"].as_array().unwrap();
        let mut installed: Vec<String> = entries
            .iter()
            .filter(|entry| entry_contains_brehon_marker(entry))
            .filter_map(|entry| entry["matcher"].as_str().map(str::to_string))
            .collect();
        installed.sort();

        let mut expected: Vec<String> = CLAUDE_HOOK_MATCHERS
            .iter()
            .map(|matcher| matcher.to_string())
            .collect();
        expected.sort();

        assert_eq!(installed, expected);
        assert_eq!(entries.len(), CLAUDE_HOOK_MATCHERS.len() + 1);
        assert!(entries
            .iter()
            .any(|entry| entry["matcher"].as_str() == Some("Read")));

        let hook_command = entries
            .iter()
            .filter(|entry| entry_contains_brehon_marker(entry))
            .find_map(|entry| {
                entry
                    .get("hooks")
                    .and_then(|hooks| hooks.as_array())
                    .and_then(|hooks| hooks.first())
                    .and_then(|hook| hook.get("command"))
                    .and_then(|command| command.as_str())
            })
            .expect("Brehon hook command should be present");
        assert!(
            hook_command.contains("BREHON_ROOT="),
            "hook command should carry BREHON_ROOT explicitly: {hook_command}"
        );
        assert!(
            hook_command.contains("BREHON_PROJECT_ROOT="),
            "hook command should carry BREHON_PROJECT_ROOT explicitly: {hook_command}"
        );
        assert!(
            hook_command.contains("BREHON_WORKSPACE_ROOT=\"${BREHON_WORKSPACE_ROOT:-$PWD}\""),
            "hook command should derive missing BREHON_WORKSPACE_ROOT from PWD: {hook_command}"
        );
        assert!(
            command_contains_brehon_hook_marker(hook_command),
            "hook command should still contain the removable Brehon marker: {hook_command}"
        );
    }

    #[test]
    fn test_claude_hook_command_quotes_paths() {
        let command = claude_hook_command(
            Path::new("/tmp/repo with ' quote"),
            Path::new("/tmp/bin/brehon with ' quote"),
        );

        assert!(command.contains("BREHON_ROOT='/tmp/repo with '\\'' quote/.brehon'"));
        assert!(command.contains("BREHON_PROJECT_ROOT='/tmp/repo with '\\'' quote'"));
        assert!(command.contains("'/tmp/bin/brehon with '\\'' quote' claude-hook"));
    }

    #[test]
    fn test_git_current_branch_missing_cwd_reports_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-worktree");

        let err = git_current_branch(&missing).unwrap_err().to_string();

        assert!(err.contains("Failed to open git repository"));
        assert!(err.contains("reading the current branch"));
        assert!(err.contains(missing.to_string_lossy().as_ref()));
    }

    #[test]
    fn test_worker_branch_name_respects_prefix_separator() {
        assert_eq!(
            worker_branch_name("brehon/", None, "worker-1"),
            "brehon/worker-1"
        );
        assert_eq!(
            worker_branch_name("brehon", None, "worker-1"),
            "brehon/worker-1"
        );
        assert_eq!(worker_branch_name("", None, "worker-1"), "worker-1");
        assert_eq!(
            worker_branch_name("brehon/", Some("run-a"), "worker-1"),
            "brehon/runs/run-a/worker-1"
        );
    }

    #[test]
    fn test_role_branch_name_respects_prefix_separator() {
        assert_eq!(
            role_branch_name("brehon/", None, "supervisor", "claude-code"),
            "brehon/supervisor/claude-code"
        );
        assert_eq!(
            role_branch_name("brehon", None, "reviewer", "reviewer-1"),
            "brehon/reviewer/reviewer-1"
        );
        assert_eq!(
            role_branch_name("", None, "reviewer", "reviewer-1"),
            "reviewer/reviewer-1"
        );
        assert_eq!(
            role_branch_name("brehon/", Some("run-a"), "supervisor", "claude-code"),
            "brehon/runs/run-a/supervisor/claude-code"
        );
    }

    #[test]
    fn test_agent_to_adapter_maps_claude_alias_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "claude-reviewer".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("claude".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("claude-reviewer", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Claude)
        );
    }

    #[test]
    fn test_agent_to_adapter_keeps_claude_builtin_when_launcher_sets_env() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "claude-ollama-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("claude".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::from([
                    ("ANTHROPIC_AUTH_TOKEN".to_string(), "ollama".to_string()),
                    (
                        "ANTHROPIC_BASE_URL".to_string(),
                        "http://localhost:11434".to_string(),
                    ),
                ]),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("claude-ollama-worker", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Claude)
        );
    }

    #[test]
    fn test_agent_to_adapter_keeps_true_custom_agents_custom() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "custom-reviewer".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("my-wrapper".to_string()),
                args: vec!["review".to_string()],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("custom-reviewer", &config);
        assert!(adapter.as_builtin().is_none());
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::AppServer
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
    }

    #[test]
    fn test_agent_to_adapter_uses_mcp_tool_prefix_for_grok_acp() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "grok".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("/usr/local/bin/grok".to_string()),
                args: vec![
                    "agent".to_string(),
                    "--always-approve".to_string(),
                    "stdio".to_string(),
                ],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );
        config.lanes.insert(
            "grok-worker".to_string(),
            brehon_types::LaneConfig {
                launcher: "grok".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );

        let adapter = agent_to_adapter("grok-worker", &config);

        assert!(adapter.as_builtin().is_none());
        assert_eq!(adapter.capabilities().tool_prefix.as_ref(), "mcp__brehon__");
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
    }

    #[test]
    fn test_agent_to_adapter_applies_transport_and_control_plane_overrides() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "native-supervisor".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("brehon-native-agent".to_string()),
                args: vec!["--supervised".to_string()],
                provider: None,
                transport: Some("interactive_pty".to_string()),
                control_plane: Some("acp_sidecar".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );
        config.lanes.insert(
            "native-supervisor".to_string(),
            brehon_types::LaneConfig {
                launcher: "native-supervisor".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );

        let adapter = agent_to_adapter("native-supervisor", &config);
        assert!(adapter.as_builtin().is_none());
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::AcpSidecar
        );
    }

    #[test]
    fn test_agent_to_adapter_keeps_builtin_gemini_shape_when_overrides_are_present() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "gemini".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                provider: None,
                transport: Some("interactive_pty".to_string()),
                control_plane: Some("pty_injection".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("gemini", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Gemini)
        );
        assert_eq!(adapter.capabilities().tool_prefix.as_ref(), "mcp_brehon_");
        assert!(adapter.capabilities().supports_hooks);
        assert!(!adapter.capabilities().uses_ink_prompt);
        assert_eq!(
            adapter.capabilities().prompt_injection_strategy,
            brehon_mux::PromptInjectionStrategy::DelayedSubmit
        );
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::PtyInjection
        );
    }

    #[test]
    fn test_agent_to_adapter_keeps_builtin_codex_shape_for_alias_lane_overrides() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "codex".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("codex".to_string()),
                args: vec!["app-server".to_string()],
                provider: None,
                transport: Some("interactive_pty".to_string()),
                control_plane: Some("pty_injection".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );
        config.lanes.insert(
            "codex-reviewer".to_string(),
            brehon_types::LaneConfig {
                launcher: "codex".to_string(),
                model: None,
                reasoning_effort: None,
                system_prompt: None,
                profile: None,
            },
        );

        let adapter = agent_to_adapter("codex-reviewer", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Codex));
        assert_eq!(adapter.capabilities().tool_prefix.as_ref(), "mcp__brehon__");
        assert!(adapter.capabilities().uses_ink_prompt);
        assert_eq!(
            adapter.capabilities().prompt_injection_strategy,
            brehon_mux::PromptInjectionStrategy::InkEcho
        );
        assert!(!adapter.capabilities().supports_hooks);
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::PtyInjection
        );
    }

    #[test]
    fn test_agent_to_adapter_normalizes_builtin_pty_control_plane_to_pty_transport() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "gemini".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                provider: None,
                transport: None,
                control_plane: Some("pty_injection".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("gemini", &config);

        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Gemini)
        );
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::PtyInjection
        );
    }

    #[test]
    fn test_agent_to_adapter_ignores_incompatible_transport_override_for_builtin_gateway_lane() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "gemini".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                provider: None,
                transport: Some("interactive_pty".to_string()),
                control_plane: Some("acp".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("gemini", &config);

        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Gemini)
        );
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::AppServer
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
        assert!(!adapter.capabilities().one_shot);
    }

    #[test]
    fn test_agent_to_adapter_normalizes_incompatible_transport_override_for_builtin_pty_lane() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "gemini".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                provider: None,
                transport: Some("app_server".to_string()),
                control_plane: Some("pty_injection".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("gemini", &config);

        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Gemini)
        );
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::InteractivePty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::PtyInjection
        );
        assert!(!adapter.capabilities().one_shot);
    }

    #[test]
    fn test_agent_to_adapter_normalizes_unsupported_claude_gateway_override_back_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "claude-code".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("claude".to_string()),
                args: vec![],
                provider: None,
                transport: Some("app_server".to_string()),
                control_plane: Some("acp".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("claude-code", &config);

        assert_eq!(
            adapter,
            brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Claude)
        );
        assert_eq!(
            adapter.capabilities(),
            brehon_mux::SupervisorCli::Claude.capabilities()
        );
    }

    #[test]
    fn test_agent_to_adapter_normalizes_unsupported_builtin_managed_api_override_back_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "gemini".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("gemini".to_string()),
                args: vec!["--acp".to_string()],
                provider: None,
                transport: Some("managed_api".to_string()),
                control_plane: Some("openai_compatible".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("gemini", &config);

        assert_eq!(
            adapter,
            brehon_mux::AgentAdapter::BuiltIn(brehon_mux::SupervisorCli::Gemini)
        );
        assert_eq!(
            adapter.capabilities(),
            brehon_mux::SupervisorCli::Gemini.capabilities()
        );
    }

    #[test]
    fn test_agent_to_adapter_maps_plain_opencode_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "opencode-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("opencode".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("opencode-worker", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::OpenCode)
        );
    }

    #[test]
    fn test_agent_to_adapter_maps_plain_kimi_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "kimi-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("kimi".to_string()),
                args: vec!["acp".to_string()],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("kimi-worker", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Kimi));
        assert_eq!(adapter.capabilities().tool_prefix.as_ref(), "");
    }

    #[test]
    fn test_agent_to_adapter_maps_junie_name_to_builtin() {
        let config = brehon_config::parse_defaults().unwrap();

        let adapter = agent_to_adapter("junie", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Junie));
    }

    #[test]
    fn test_agent_to_adapter_maps_plain_junie_launcher_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "junie-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("junie".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("junie-worker", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Junie));
    }

    #[test]
    fn test_agent_to_adapter_maps_plain_copilot_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "copilot-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("copilot".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("copilot-worker", &config);
        assert_eq!(
            adapter.as_builtin(),
            Some(brehon_mux::SupervisorCli::Copilot)
        );
    }

    #[test]
    fn test_agent_to_adapter_maps_plain_agy_launcher_to_builtin() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "agy-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Agy,
                command: Some("agy".to_string()),
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("agy-worker", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Agy));
    }

    #[test]
    fn test_agent_to_adapter_keeps_acp_agy_one_shot_launcher_custom() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "agy-reviewer".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("agy".to_string()),
                args: vec!["--prompt-interactive".to_string()],
                provider: None,
                transport: None,
                control_plane: Some("one_shot".to_string()),
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("agy-reviewer", &config);
        assert!(adapter.as_builtin().is_none());
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::OneShotPty
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::OneShot
        );
        assert!(adapter.capabilities().one_shot);
    }

    #[test]
    fn test_agent_to_adapter_keeps_custom_codex_app_server_custom_but_codex_shaped() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "codex-ollama-worker".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::Acp,
                command: Some("codex".to_string()),
                args: vec![
                    "-c".to_string(),
                    "model_provider=\"ollama_cloud\"".to_string(),
                    "app-server".to_string(),
                ],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: None,
                api_key_env: None,
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("codex-ollama-worker", &config);
        assert!(adapter.as_builtin().is_none());
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::AppServer
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
        assert!(!adapter.capabilities().supports_textbox_submit);
        assert_eq!(adapter.capabilities().tool_prefix.as_ref(), "mcp__brehon__");
    }

    #[test]
    fn test_agent_to_adapter_maps_openai_compatible_launcher_to_managed_api() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "ollama-direct".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::OpenAiCompatible,
                command: None,
                args: vec![],
                provider: None,
                transport: None,
                control_plane: None,
                base_url: Some("https://ollama.example/v1".to_string()),
                api_key_env: Some("OLLAMA_API_KEY".to_string()),
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::from([(
                    "x-provider".to_string(),
                    "ollama-cloud".to_string(),
                )]),
            },
        );

        let adapter = agent_to_adapter("ollama-direct", &config);
        assert!(adapter.as_builtin().is_none());
        assert_eq!(
            adapter.capabilities().transport,
            brehon_mux::HarnessTransport::ManagedApi
        );
        assert_eq!(
            adapter.capabilities().preferred_control_plane,
            brehon_mux::HarnessControlPlane::OpenAiCompatible
        );
    }

    #[test]
    fn test_agent_to_adapter_maps_native_agent_to_first_class_acp_worker() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "native-openai".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::NativeAgent,
                command: None,
                args: vec![],
                provider: Some("openai-compatible".to_string()),
                transport: None,
                control_plane: None,
                base_url: Some("https://api.example.test".to_string()),
                api_key_env: Some("EXAMPLE_API_KEY".to_string()),
                permission_mode: Some("default".to_string()),
                profile: None,
                max_parallel_tool_calls: Some(4),
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: vec!["reasoning_content".to_string()],
                reasoning_effort_param: Some("thinking.reasoning_effort".to_string()),
                extra_body: Some(serde_json::json!({"thinking": {"type": "enabled"}})),
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::from([(
                    "x-provider".to_string(),
                    "example".to_string(),
                )]),
            },
        );
        config.permissions.categories.insert(
            "bash".to_string(),
            brehon_types::config::PermissionCategory::Nested(std::collections::HashMap::from([(
                "git status*".to_string(),
                brehon_types::config::PermissionValue::Allow,
            )])),
        );

        let adapter = agent_to_adapter("native-openai", &config);
        let brehon_mux::AgentAdapter::Custom(custom) = adapter else {
            panic!("native agent should materialize as a custom ACP launcher");
        };

        assert!(custom
            .command
            .as_deref()
            .unwrap()
            .contains("brehon-native-agent"));
        assert_eq!(
            custom.capabilities.preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
        assert_eq!(
            custom.capabilities.transport,
            brehon_mux::HarnessTransport::AppServer
        );
        assert!(custom.args.iter().any(|arg| arg == "--worker"));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--provider", "openai-compatible"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--tool-prefix", "mcp_brehon_"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--permission-mode", "bypass"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--max-parallel-tool-calls", "4"]));
        assert!(
            custom
                .args
                .windows(2)
                .any(|window| window
                    == ["--assistant-message-passthrough-field", "reasoning_content"])
        );
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--base-url", "https://api.example.test"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--reasoning-effort-param", "thinking.reasoning_effort"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window[0] == "--extra-body-json" && window[1].contains("\"thinking\"")));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--header", "x-provider=example"]));
        let policy_json = custom
            .args
            .windows(2)
            .find_map(|window| (window[0] == "--permission-policy-json").then(|| window[1].clone()))
            .expect("native agent should receive Brehon permission policy");
        let parsed: serde_json::Value = serde_json::from_str(&policy_json).unwrap();
        assert_eq!(parsed["bash"]["git status*"], "Allow");
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--env-allowlist", "PATH"]));
    }

    #[test]
    fn test_native_agent_worker_defaults_to_guarded_bypass_permissions() {
        let agent_config = brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::NativeAgent,
            command: None,
            args: vec![],
            provider: Some("openai-compatible".to_string()),
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };

        let args = native_agent_args(
            &agent_config,
            &brehon_types::config::PermissionsConfig::default(),
            &default_security(),
        );

        assert!(args.iter().any(|arg| arg == "--worker"));
        assert!(args
            .windows(2)
            .any(|window| window == ["--tool-prefix", "mcp_brehon_"]));
        assert!(args
            .windows(2)
            .any(|window| window == ["--permission-mode", "bypass"]));
    }

    #[test]
    fn test_native_agent_worker_normalizes_default_permissions_to_bypass() {
        let agent_config = brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::NativeAgent,
            command: None,
            args: vec![],
            provider: Some("openai-compatible".to_string()),
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: Some("default".to_string()),
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };

        let args = native_agent_args(
            &agent_config,
            &brehon_types::config::PermissionsConfig::default(),
            &default_security(),
        );

        assert!(args.iter().any(|arg| arg == "--worker"));
        assert!(args
            .windows(2)
            .any(|window| window == ["--permission-mode", "bypass"]));
    }

    #[test]
    fn test_native_agent_args_overwrite_legacy_agora_tool_prefix() {
        let agent_config = brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::NativeAgent,
            command: None,
            args: vec![
                "--worker".to_string(),
                "--tool-prefix=mcp_agora_".to_string(),
            ],
            provider: Some("openai-compatible".to_string()),
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };

        let args = native_agent_args(
            &agent_config,
            &brehon_types::config::PermissionsConfig::default(),
            &default_security(),
        );

        assert!(args.iter().any(|arg| arg == "--tool-prefix=mcp_brehon_"));
        assert!(!args.iter().any(|arg| arg.contains("mcp_agora_")));
    }

    #[test]
    fn test_native_agent_worker_preserves_non_default_permission_mode() {
        let agent_config = brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::NativeAgent,
            command: None,
            args: vec![],
            provider: Some("openai-compatible".to_string()),
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: Some("accept-edits".to_string()),
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };

        let args = native_agent_args(
            &agent_config,
            &brehon_types::config::PermissionsConfig::default(),
            &default_security(),
        );

        assert!(args
            .windows(2)
            .any(|window| window == ["--permission-mode", "accept-edits"]));
    }

    #[test]
    fn test_agent_to_adapter_maps_native_agent_to_first_class_acp_reviewer() {
        let mut config = brehon_config::parse_defaults().unwrap();
        config.launchers.insert(
            "native-reviewer-launcher".to_string(),
            brehon_types::AgentConnectionConfig {
                adapter: brehon_types::agent::AdapterKind::NativeAgent,
                command: None,
                args: vec![],
                provider: Some("openai-compatible".to_string()),
                transport: None,
                control_plane: None,
                base_url: Some("https://api.example.test".to_string()),
                api_key_env: Some("EXAMPLE_API_KEY".to_string()),
                permission_mode: None,
                profile: None,
                max_parallel_tool_calls: None,
                max_concurrency: None,
                context_window: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );
        config.lanes.insert(
            "native-reviewer".to_string(),
            brehon_types::config::LaneConfig {
                launcher: "native-reviewer-launcher".to_string(),
                model: None,
                reasoning_effort: Some("high".to_string()),
                system_prompt: Some("Review for correctness and maintainability.".to_string()),
                profile: None,
            },
        );
        config.roles.reviewers = vec![brehon_types::config::ReviewerPoolConfig {
            lane: "native-reviewer".to_string(),
            model: None,
            reasoning_effort: Some("high".to_string()),
            system_prompt: None,
            min: 1,
            max: 1,
        }];
        config.review.default_reviewers = vec!["native-reviewer".to_string()];

        let adapter = agent_to_adapter("native-reviewer", &config);
        let brehon_mux::AgentAdapter::Custom(custom) = adapter else {
            panic!("native reviewer should materialize as a custom ACP launcher");
        };

        assert_eq!(
            custom.capabilities.preferred_control_plane,
            brehon_mux::HarnessControlPlane::Acp
        );
        assert_eq!(
            custom.capabilities.transport,
            brehon_mux::HarnessTransport::AppServer
        );
        assert_eq!(custom.capabilities.tool_prefix.as_ref(), "mcp_brehon_");
        assert!(custom.args.iter().any(|arg| arg == "--worker"));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--tool-prefix", "mcp_brehon_"]));
        assert!(custom
            .args
            .windows(2)
            .any(|window| window == ["--permission-mode", "bypass"]));
    }

    #[test]
    fn test_native_agent_legacy_agora_command_maps_to_brehon_binary() {
        let command = native_agent_command(Some("/opt/brehon/bin/agora-native-agent"));

        assert_eq!(command_basename(&command), "brehon-native-agent");
    }

    #[test]
    fn test_native_agent_supervised_does_not_default_to_bypass_permissions() {
        let agent_config = brehon_types::AgentConnectionConfig {
            adapter: brehon_types::agent::AdapterKind::NativeAgent,
            command: None,
            args: vec!["--supervised".to_string()],
            provider: Some("openai-compatible".to_string()),
            transport: None,
            control_plane: None,
            base_url: None,
            api_key_env: None,
            permission_mode: None,
            profile: None,
            max_parallel_tool_calls: None,
            max_concurrency: None,
            context_window: None,
            assistant_message_passthrough_fields: Vec::new(),
            reasoning_effort_param: None,
            extra_body: None,
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
        };

        let args = native_agent_args(
            &agent_config,
            &brehon_types::config::PermissionsConfig::default(),
            &default_security(),
        );

        assert!(args.iter().any(|arg| arg == "--supervised"));
        assert!(!args.iter().any(|arg| arg == "--permission-mode"));
    }

    #[tokio::test]
    async fn test_reconcile_initiative_hierarchy_backfills_missing_initiative_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        let brehon_root = temp.path().join(".brehon");
        std::fs::create_dir_all(brehon_root.join("runtime").join("tasks")).unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-init.json"),
            serde_json::json!({
                "task_id": "T-init",
                "title": "Program Alpha",
                "task_type": "initiative",
                "status": "pending"
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            brehon_root
                .join("runtime")
                .join("tasks")
                .join("T-epic.json"),
            serde_json::json!({
                "task_id": "T-epic",
                "title": "Phase 1",
                "task_type": "epic",
                "status": "pending",
                "parent_id": "T-init",
                "integration_branch": "epic/phase-1"
            })
            .to_string(),
        )
        .unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, temp.path());

        let repaired = reconcile_initiative_hierarchy_for_run(temp.path(), &brehon_root, &config)
            .await
            .unwrap();
        assert_eq!(repaired.len(), 1);

        let initiative: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                brehon_root
                    .join("runtime")
                    .join("tasks")
                    .join("T-init.json"),
            )
            .unwrap(),
        )
        .unwrap();

        let branch = initiative["integration_branch"].as_str().unwrap();
        let worktree = initiative["integration_worktree"].as_str().unwrap();
        let worktree_path = Path::new(worktree);
        assert!(branch.starts_with("initiative/"));
        assert!(worktree_path.starts_with(external_root.join("initiative").join("T-init")));
        assert!(!worktree_path.starts_with(brehon_root.join("worktrees")));
        assert!(worktree_path.exists());
        assert_eq!(
            run_git(worktree_path, &["branch", "--show-current"]),
            branch
        );
        assert_eq!(
            run_git(
                temp.path(),
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
            ),
            run_git(worktree_path, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn test_reconcile_orphaned_worker_assignments_for_run_clears_dead_assignee() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        std::fs::write(
            tasks_dir.join("T-orphan.json"),
            serde_json::json!({
                "task_id": "T-orphan",
                "title": "Orphaned worker task",
                "task_type": "task",
                "status": "in_progress",
                "assignee": "dead-worker",
                "activity": "reading",
                "percent": 10
            })
            .to_string(),
        )
        .unwrap();

        let repaired = reconcile_orphaned_worker_assignments_for_run(
            &brehon_root,
            &["live-worker".to_string()],
        )
        .unwrap();
        assert_eq!(repaired.len(), 1);

        let repaired_task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tasks_dir.join("T-orphan.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(repaired_task["status"], "pending");
        assert!(repaired_task["assignee"].is_null());
        assert_eq!(repaired_task["orphaned_assignee"], "dead-worker");
        assert_eq!(repaired_task["orphaned_status"], "in_progress");
        assert!(repaired_task.get("activity").is_none());
    }

    #[test]
    fn test_reconcile_orphaned_worker_assignments_clears_recoverable_blocked_deadlock() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let tasks_dir = brehon_root.join("runtime").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        std::fs::write(
            tasks_dir.join("T-deadlock.json"),
            serde_json::json!({
                "task_id": "T-deadlock",
                "title": "Recoverable blocked worker task",
                "task_type": "task",
                "status": "blocked",
                "assignee": "dead-worker",
                "activity": "completing",
                "percent": 90,
                "latest_commit": "abc123",
                "blockers": "State deadlock: checkpoint created during pending state, need reassignment to complete"
            })
            .to_string(),
        )
        .unwrap();

        let repaired = reconcile_orphaned_worker_assignments_for_run(
            &brehon_root,
            &["live-worker".to_string()],
        )
        .unwrap();
        assert_eq!(repaired.len(), 1);

        let repaired_task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tasks_dir.join("T-deadlock.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(repaired_task["status"], "pending");
        assert!(repaired_task["assignee"].is_null());
        assert_eq!(repaired_task["orphaned_assignee"], "dead-worker");
        assert_eq!(repaired_task["orphaned_status"], "blocked");
        assert!(repaired_task.get("blockers").is_none());
        assert!(repaired_task.get("activity").is_none());
    }

    #[test]
    fn test_reconcile_orphaned_worker_assignments_keeps_unconsolidated_review_owner() {
        let temp = tempfile::tempdir().unwrap();
        let brehon_root = temp.path().join(".brehon");
        let tasks_dir = brehon_root.join("runtime").join("tasks");
        let round_dir = brehon_root
            .join("runtime")
            .join("reviews")
            .join("T-reviewing")
            .join("round-1");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&round_dir).unwrap();

        std::fs::write(
            tasks_dir.join("T-reviewing.json"),
            serde_json::json!({
                "task_id": "T-reviewing",
                "title": "Review still running",
                "task_type": "task",
                "status": "changes_requested",
                "assignee": "dead-worker",
                "review_owner": "dead-worker",
                "activity": "reviewing",
                "percent": 100
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            round_dir.join("request.json"),
            serde_json::json!({
                "task_id": "T-reviewing",
                "review_id": "REV-running",
                "requested_by": "supervisor",
                "requested_at": chrono::Utc::now().to_rfc3339(),
                "title": "Review",
                "description": "Still collecting",
                "commit": "abc123",
                "base_commit": "base123",
                "merge_target_head": "base123",
                "commits": ["abc123"],
                "context": ""
            })
            .to_string(),
        )
        .unwrap();

        let repaired = reconcile_orphaned_worker_assignments_for_run(
            &brehon_root,
            &["live-worker".to_string()],
        )
        .unwrap();
        assert!(repaired.is_empty(), "{repaired:?}");

        let task: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tasks_dir.join("T-reviewing.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(task["assignee"], "dead-worker");
        assert_eq!(task["review_owner"], "dead-worker");
        assert!(task.get("orphaned_assignee").is_none());
    }

    #[tokio::test]
    async fn test_prepare_worker_worktrees_creates_isolated_cwds() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        let worker_names = vec!["worker-1".to_string(), "worker-2".to_string()];
        let worker_cwds =
            prepare_scoped_worktrees(temp.path(), &config, Some("session-a"), None, &worker_names)
                .await
                .unwrap();

        assert_eq!(worker_cwds.len(), 2);
        for worker_name in &worker_names {
            let worktree_path = worker_cwds.get(worker_name).unwrap();
            assert!(worktree_path.exists());
            assert!(worktree_path.starts_with(external_root.join("runs/session-a")));
            assert!(!worktree_path.starts_with(temp.path().join(".brehon/worktrees")));
            let branch = run_git(worktree_path, &["rev-parse", "--abbrev-ref", "HEAD"]);
            assert_eq!(branch, format!("brehon/runs/session-a/{worker_name}"));
        }

        cleanup_scoped_worktrees(temp.path(), &worker_cwds).await;
        let worktree_list = run_git(temp.path(), &["worktree", "list", "--porcelain"]);
        for worktree_path in worker_cwds.values() {
            assert!(
                !worktree_list.contains(&worktree_path.to_string_lossy().to_string()),
                "worktree should be unregistered: {}",
                worktree_path.display()
            );
        }
    }

    #[tokio::test]
    async fn test_prepare_worker_worktrees_default_root_is_external_to_repo() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        config.orchestration.worktree_root = None;
        let external_root = effective_worktree_root(temp.path(), &config);
        let repo_identity = external_root
            .file_name()
            .and_then(|name| name.to_str())
            .expect("default external root should end with repo identity")
            .to_string();

        let worker_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-default-external"),
            None,
            &["worker-default".to_string()],
        )
        .await
        .unwrap();

        let worktree_path = worker_cwds.get("worker-default").unwrap();
        assert!(worktree_path.exists());
        assert!(worktree_path.starts_with(external_root.join("runs/session-default-external")));
        assert!(
            !worktree_path.starts_with(temp.path()),
            "default worktree root should be outside repo: {}",
            worktree_path.display()
        );
        assert!(
            worktree_path.to_string_lossy().contains(&repo_identity),
            "default path should include repo identity: {}",
            worktree_path.display()
        );
        let status = run_git(
            temp.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert!(
            status.is_empty(),
            "external worktree preparation should not dirty shared root:\n{status}"
        );

        cleanup_scoped_worktrees(temp.path(), &worker_cwds).await;
        let _ = std::fs::remove_dir_all(external_root);
    }

    #[tokio::test]
    async fn test_prepare_worker_worktrees_honors_explicit_absolute_root() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        let worker_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-explicit-root"),
            None,
            &["worker-explicit".to_string()],
        )
        .await
        .unwrap();

        let worktree_path = worker_cwds.get("worker-explicit").unwrap();
        assert!(worktree_path.starts_with(external_root.join("runs/session-explicit-root")));
        assert!(!worktree_path.starts_with(temp.path().join(".brehon")));
        let status = run_git(
            temp.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert!(
            status.is_empty(),
            "explicit external root should not dirty shared root:\n{status}"
        );

        cleanup_scoped_worktrees(temp.path(), &worker_cwds).await;
    }

    #[tokio::test]
    async fn test_cleanup_scoped_worktrees_removes_legacy_in_repo_worktree() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let legacy_root = OrchestrationConfig::legacy_worktree_root(temp.path());
        let legacy_path = legacy_root.join("runs/session-legacy/worker-legacy");
        let legacy_path_arg = legacy_path.to_string_lossy().to_string();
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        run_git(
            temp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "brehon/runs/session-legacy/worker-legacy",
                &legacy_path_arg,
            ],
        );

        let role_cwds = HashMap::from([("worker-legacy".to_string(), legacy_path.clone())]);
        cleanup_scoped_worktrees(temp.path(), &role_cwds).await;

        let worktree_list = run_git(temp.path(), &["worktree", "list", "--porcelain"]);
        assert!(
            !worktree_list.contains(&legacy_path.to_string_lossy().to_string()),
            "legacy worktree should be unregistered: {}",
            legacy_path.display()
        );
        assert!(
            !legacy_path.exists(),
            "legacy worktree directory should be removed: {}",
            legacy_path.display()
        );
    }

    #[tokio::test]
    async fn test_prepare_worker_worktrees_returns_empty_when_disabled() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = false;

        let worker_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-disabled"),
            None,
            &["worker-1".to_string(), "worker-2".to_string()],
        )
        .await
        .unwrap();

        assert!(worker_cwds.is_empty());
        assert!(!temp
            .path()
            .join(".brehon/worktrees/runs/session-disabled/worker-1")
            .exists());
    }

    #[tokio::test]
    async fn test_prepare_role_worktrees_creates_supervisor_and_reviewer_cwds() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();
        std::fs::create_dir_all(temp.path().join(".agents")).unwrap();
        std::fs::create_dir_all(temp.path().join(".claude")).unwrap();
        std::fs::write(
            temp.path().join(".claude/settings.local.json"),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"agora":{"command":"agora","args":["serve"]},"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join(".agents/mcp_config.json"),
            r#"{"mcpServers":{"agora":{"command":"agora","args":["serve"]},"other":{"command":"other"}}}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join("opencode.json"),
            r#"{"$schema":"https://opencode.ai/config.json","mcp":{"other":{"type":"local","command":["other"]}}}"#,
        )
        .unwrap();
        run_git(temp.path(), &["add", ".mcp.json"]);
        run_git(temp.path(), &["commit", "-m", "add mcp config"]);
        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        let supervisor_names = vec!["claude-code".to_string()];
        let reviewer_names = vec!["reviewer-a".to_string(), "reviewer-b".to_string()];
        let supervisor_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-role"),
            Some("supervisor"),
            &supervisor_names,
        )
        .await
        .unwrap();
        let reviewer_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-role"),
            Some("reviewer"),
            &reviewer_names,
        )
        .await
        .unwrap();

        let supervisor_path = supervisor_cwds.get("claude-code").unwrap();
        assert!(supervisor_path.exists());
        assert!(supervisor_path.starts_with(&external_root));
        assert!(supervisor_path.ends_with("runs/session-role/supervisor/claude-code"));
        assert_eq!(
            run_git(supervisor_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-role/supervisor/claude-code"
        );

        let reviewer_path = reviewer_cwds.get("reviewer-a").unwrap();
        assert!(reviewer_path.exists());
        assert!(reviewer_path.starts_with(&external_root));
        assert!(reviewer_path.ends_with("runs/session-role/reviewer/reviewer-a"));
        assert_eq!(
            run_git(reviewer_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-role/reviewer/reviewer-a"
        );
        assert_eq!(
            std::fs::read_to_string(supervisor_path.join(".claude/settings.local.json")).unwrap(),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#
        );
        assert_eq!(
            std::fs::read_to_string(reviewer_path.join(".claude/settings.local.json")).unwrap(),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#
        );
        let supervisor_mcp: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(supervisor_path.join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert!(supervisor_mcp["mcpServers"].get("agora").is_none());
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
            "claude-code"
        );
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"]["BREHON_AGENT_ROLE"],
            "supervisor"
        );
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"]["BREHON_SESSION_NAME"],
            "session-role"
        );
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"]["BREHON_PROJECT_ROOT"],
            temp.path().to_string_lossy().to_string()
        );
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"]["BREHON_WORKSPACE_ROOT"],
            supervisor_path.to_string_lossy().to_string()
        );
        assert_eq!(
            supervisor_mcp["mcpServers"]["brehon"]["env"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );

        let reviewer_mcp: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reviewer_path.join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert!(reviewer_mcp["mcpServers"].get("agora").is_none());
        assert_eq!(
            reviewer_mcp["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
            "reviewer-a"
        );
        assert_eq!(
            reviewer_mcp["mcpServers"]["brehon"]["env"]["BREHON_AGENT_ROLE"],
            "reviewer"
        );
        assert_eq!(
            reviewer_mcp["mcpServers"]["brehon"]["env"]["BREHON_WORKSPACE_ROOT"],
            reviewer_path.to_string_lossy().to_string()
        );
        assert_eq!(
            reviewer_mcp["mcpServers"]["brehon"]["env"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );

        let supervisor_agy: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(supervisor_path.join(".agents/mcp_config.json")).unwrap(),
        )
        .unwrap();
        assert!(supervisor_agy["mcpServers"].get("agora").is_none());
        assert_eq!(supervisor_agy["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            supervisor_agy["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
            "claude-code"
        );
        assert_eq!(
            supervisor_agy["mcpServers"]["brehon"]["env"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );

        let reviewer_agy: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reviewer_path.join(".agents/mcp_config.json")).unwrap(),
        )
        .unwrap();
        assert!(reviewer_agy["mcpServers"].get("agora").is_none());
        assert_eq!(reviewer_agy["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            reviewer_agy["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
            "reviewer-a"
        );
        assert_eq!(
            reviewer_agy["mcpServers"]["brehon"]["env"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );

        let reviewer_opencode: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reviewer_path.join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reviewer_opencode["mcp"]["other"]["command"],
            serde_json::json!(["other"])
        );
        assert_eq!(reviewer_opencode["mcp"]["brehon"]["type"], "local");
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["command"],
            serde_json::json!([current_brehon_exe(), "serve"])
        );
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["environment"]["BREHON_AGENT_NAME"],
            "reviewer-a"
        );
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["environment"]["BREHON_AGENT_ROLE"],
            "reviewer"
        );
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["environment"]["BREHON_PROJECT_ROOT"],
            temp.path().to_string_lossy().to_string()
        );
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["environment"]["BREHON_WORKSPACE_ROOT"],
            reviewer_path.to_string_lossy().to_string()
        );
        assert_eq!(
            reviewer_opencode["mcp"]["brehon"]["environment"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );

        cleanup_scoped_worktrees(temp.path(), &supervisor_cwds).await;
        cleanup_scoped_worktrees(temp.path(), &reviewer_cwds).await;
    }

    #[tokio::test]
    async fn test_prepare_role_worktrees_reuses_existing_role_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        run_git(
            temp.path(),
            &["branch", "brehon/runs/session-reuse/supervisor/claude-code"],
        );

        let supervisor_names = vec!["claude-code".to_string()];
        let supervisor_cwds = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-reuse"),
            Some("supervisor"),
            &supervisor_names,
        )
        .await
        .unwrap();

        let supervisor_path = supervisor_cwds.get("claude-code").unwrap();
        assert!(supervisor_path.exists());
        assert!(supervisor_path.starts_with(&external_root));
        assert_eq!(
            run_git(supervisor_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-reuse/supervisor/claude-code"
        );

        cleanup_scoped_worktrees(temp.path(), &supervisor_cwds).await;
    }

    #[tokio::test]
    async fn test_prepare_role_worktrees_can_reuse_same_agent_name_across_sessions() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        let supervisor_names = vec!["claude-code".to_string()];
        let first = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-one"),
            Some("supervisor"),
            &supervisor_names,
        )
        .await
        .unwrap();
        let second = prepare_scoped_worktrees(
            temp.path(),
            &config,
            Some("session-two"),
            Some("supervisor"),
            &supervisor_names,
        )
        .await
        .unwrap();

        let first_path = first.get("claude-code").unwrap();
        let second_path = second.get("claude-code").unwrap();
        assert_ne!(first_path, second_path);
        assert!(first_path.starts_with(&external_root));
        assert!(second_path.starts_with(&external_root));
        assert_eq!(
            run_git(first_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-one/supervisor/claude-code"
        );
        assert_eq!(
            run_git(second_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-two/supervisor/claude-code"
        );

        cleanup_scoped_worktrees(temp.path(), &first).await;
        cleanup_scoped_worktrees(temp.path(), &second).await;
    }

    #[tokio::test]
    async fn test_prepare_scoped_worktrees_uses_distinct_paths_per_session() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        let names = vec!["worker-1".to_string()];
        let session_a =
            prepare_scoped_worktrees(temp.path(), &config, Some("session-a"), None, &names)
                .await
                .unwrap();
        let session_a_path = session_a.get("worker-1").cloned().unwrap();
        cleanup_scoped_worktrees(temp.path(), &session_a).await;
        let session_b =
            prepare_scoped_worktrees(temp.path(), &config, Some("session-b"), None, &names)
                .await
                .unwrap();
        let session_b_path = session_b.get("worker-1").cloned().unwrap();

        assert_ne!(
            session_a_path, session_b_path,
            "worker worktrees must not be reused across runs"
        );
        assert!(session_a_path.starts_with(&external_root));
        assert!(session_b_path.starts_with(&external_root));

        cleanup_scoped_worktrees(temp.path(), &session_b).await;
    }

    #[test]
    fn test_normalize_project_root_strips_dotbrehon_suffix() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let brehon_dir = temp.path().join(".brehon");
        std::fs::create_dir_all(&brehon_dir).unwrap();

        let normalized = normalize_project_root(&brehon_dir);
        assert_eq!(normalized, temp.path());
    }

    #[test]
    fn test_normalize_project_root_leaves_non_brehon_path_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let sub_dir = temp.path().join("src").join("components");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let normalized = normalize_project_root(&sub_dir);
        assert_eq!(normalized, sub_dir);
    }

    #[test]
    fn test_normalize_project_root_falls_back_to_input_when_no_git() {
        let temp = tempfile::tempdir().unwrap();
        // No git repo initialized.
        let normalized = normalize_project_root(temp.path());
        assert_eq!(normalized, temp.path());
    }

    #[test]
    fn test_normalize_project_root_relative_dotbrehon_returns_dot() {
        // Regression: a relative `.brehon` path has an empty parent, which
        // used to resolve to an empty PathBuf and poisoned env vars.
        let normalized = normalize_project_root(Path::new(".brehon"));
        assert_eq!(normalized, Path::new("."));
    }

    #[test]
    fn test_compute_repo_identity_includes_name_and_hash() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        let identity = compute_repo_identity(temp.path());
        let parts: Vec<&str> = identity.split('-').collect();
        // Expected format: <repo-name>-<8-char-hex-hash>
        assert!(
            parts.len() >= 2,
            "identity should be '<name>-<hash>' format, got: {identity}"
        );
        let hash_part = parts.last().unwrap();
        assert_eq!(
            hash_part.len(),
            8,
            "hash part should be 8 hex chars, got: {hash_part}"
        );
        assert!(
            hash_part.chars().all(|c| c.is_ascii_hexdigit()),
            "hash part should be hex digits, got: {hash_part}"
        );
    }

    #[test]
    fn test_compute_repo_identity_fallback_without_git() {
        let temp = tempfile::tempdir().unwrap();
        // Do NOT init a git repo — force the DefaultHasher fallback path.

        let identity = compute_repo_identity(temp.path());
        let parts: Vec<&str> = identity.split('-').collect();
        assert!(
            parts.len() >= 2,
            "identity should be '<name>-<hash>' format, got: {identity}"
        );
        let hash_part = parts.last().unwrap();
        assert_eq!(
            hash_part.len(),
            8,
            "fallback hash part should be 8 hex chars, got: {hash_part}"
        );
        assert!(
            hash_part.chars().all(|c| c.is_ascii_hexdigit()),
            "fallback hash part should be hex digits, got: {hash_part}"
        );
    }

    #[test]
    fn test_compute_repo_identity_uses_cache_on_subsequent_calls() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();

        // First call computes identity and writes the cache.
        let identity1 = compute_repo_identity(temp.path());
        let cache_path = temp.path().join(".brehon/runtime/repo-identity-cache.json");
        assert!(
            cache_path.exists(),
            "cache file should be created after first call"
        );

        // Second call should read from cache and return the same identity.
        let identity2 = compute_repo_identity(temp.path());
        assert_eq!(
            identity1, identity2,
            "cached identity should match computed identity"
        );

        // Verify the cache contains the repo name for invalidation.
        let content = std::fs::read_to_string(&cache_path).unwrap();
        let cache: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            cache["repo_name"].as_str().unwrap(),
            temp.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_lowercase()
        );
        assert_eq!(cache["identity"].as_str().unwrap(), identity1);
    }

    #[test]
    fn test_compute_repo_identity_uses_remote_url_when_available() {
        let temp1 = tempfile::tempdir().unwrap();
        init_git_repo(temp1.path());
        run_git(
            temp1.path(),
            &["remote", "add", "origin", "https://example.com/repo.git"],
        );

        let temp2 = tempfile::tempdir().unwrap();
        init_git_repo(temp2.path());
        run_git(
            temp2.path(),
            &["remote", "add", "origin", "https://example.com/repo.git"],
        );

        let identity1 = compute_repo_identity(temp1.path());
        let identity2 = compute_repo_identity(temp2.path());

        let parts1: Vec<&str> = identity1.split('-').collect();
        let parts2: Vec<&str> = identity2.split('-').collect();

        // Both should have valid 8-char hex hashes.
        assert_eq!(parts1.last().unwrap().len(), 8);
        assert_eq!(parts2.last().unwrap().len(), 8);

        // Same remote URL should produce the same hash even in different dirs.
        assert_eq!(
            parts1.last(),
            parts2.last(),
            "same remote URL should produce identical hash part"
        );
    }

    #[tokio::test]
    async fn test_prepare_worker_worktrees_with_dotbrehon_cwd_syncs_from_project_root() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::create_dir_all(temp.path().join(".brehon")).unwrap();
        std::fs::create_dir_all(temp.path().join(".claude")).unwrap();
        std::fs::write(
            temp.path().join(".claude/settings.local.json"),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"agora":{"command":"agora","args":["serve"]},"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#,
        )
        .unwrap();
        run_git(temp.path(), &["add", ".mcp.json"]);
        run_git(temp.path(), &["commit", "-m", "add mcp config"]);

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();
        let external = tempfile::tempdir().unwrap();
        let external_root = configure_test_external_worktree_root(&mut config, external.path());

        // Invoke with the `.brehon` directory as cwd — this is a supported form.
        let brehon_dir = temp.path().join(".brehon");
        let worker_names = vec!["worker-dotbrehon".to_string()];
        let worker_cwds = prepare_scoped_worktrees(
            &brehon_dir,
            &config,
            Some("session-dotbrehon"),
            None,
            &worker_names,
        )
        .await
        .unwrap();

        assert_eq!(worker_cwds.len(), 1);
        let worktree_path = worker_cwds.get("worker-dotbrehon").unwrap();
        assert!(worktree_path.exists());

        // Worktree must be under the configured external root, not under
        // <repo>/.brehon/.brehon/worktrees/ (cwd was the .brehon dir).
        assert!(
            worktree_path.starts_with(&external_root),
            "worktree should start with '{}', got: {}",
            external_root.display(),
            worktree_path.display()
        );
        assert!(!worktree_path.starts_with(temp.path().join(".brehon")));

        // Scaffolding files must be synced from the project root, not from .brehon/.
        assert_eq!(
            std::fs::read_to_string(worktree_path.join(".claude/settings.local.json")).unwrap(),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#
        );
        let mcp_config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(worktree_path.join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert!(mcp_config["mcpServers"].get("agora").is_none());
        assert_eq!(
            mcp_config["mcpServers"]["brehon"]["env"]["BREHON_AGENT_NAME"],
            "worker-dotbrehon"
        );
        assert_eq!(
            mcp_config["mcpServers"]["brehon"]["env"]["BREHON_AGENT_ROLE"],
            "worker"
        );
        assert_eq!(
            mcp_config["mcpServers"]["brehon"]["env"]["BREHON_PROJECT_ROOT"],
            temp.path().to_string_lossy().to_string()
        );

        // Identity must be derived from the repo name, not `.brehon`.
        let normalized_root = normalize_project_root(&brehon_dir);
        let repo_identity = compute_repo_identity(&normalized_root);
        assert!(
            !repo_identity.starts_with(".brehon-"),
            "identity should not start with '.brehon-', got: {repo_identity}"
        );

        cleanup_scoped_worktrees(&brehon_dir, &worker_cwds).await;
    }

    #[test]
    fn test_ensure_shared_root_on_default_branch_restores_clean_role_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        run_git(
            temp.path(),
            &["checkout", "-b", "brehon/supervisor/claude-code"],
        );

        let default_branch = ensure_shared_root_on_default_branch(temp.path()).unwrap();
        assert_eq!(default_branch, "main");
        assert_eq!(run_git(temp.path(), &["branch", "--show-current"]), "main");
    }

    #[test]
    fn test_ensure_shared_root_on_default_branch_rejects_dirty_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::write(temp.path().join("leaked.txt"), "dirty\n").unwrap();

        let err = ensure_shared_root_on_default_branch(temp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Shared repo root is on default branch 'main'"));
        assert_eq!(run_git(temp.path(), &["branch", "--show-current"]), "main");
    }

    #[test]
    fn test_ensure_shared_root_on_default_branch_rejects_dirty_role_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        run_git(
            temp.path(),
            &["checkout", "-b", "brehon/supervisor/claude-code"],
        );
        std::fs::write(temp.path().join("README.md"), "dirty\n").unwrap();

        let err = ensure_shared_root_on_default_branch(temp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Shared repo root is on 'brehon/supervisor/claude-code'"));
        assert_eq!(
            run_git(temp.path(), &["branch", "--show-current"]),
            "brehon/supervisor/claude-code"
        );
    }

    #[test]
    fn test_restore_shared_root_branch_recovers_clean_drift() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        run_git(
            temp.path(),
            &["checkout", "-b", "brehon/supervisor/claude-code"],
        );

        restore_shared_root_branch(temp.path(), "main").unwrap();
        assert_eq!(run_git(temp.path(), &["branch", "--show-current"]), "main");
    }

    #[test]
    fn test_restore_shared_root_branch_rejects_dirty_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::write(temp.path().join("leaked.txt"), "dirty\n").unwrap();

        let err = restore_shared_root_branch(temp.path(), "main")
            .unwrap_err()
            .to_string();
        assert!(err.contains("became dirty during Brehon run"));
        assert_eq!(run_git(temp.path(), &["branch", "--show-current"]), "main");
    }

    #[test]
    fn test_ensure_codex_instruction_files_writes_role_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = brehon_config::parse_defaults().unwrap();
        config.roles.supervisor.name = "claude-supervisor".to_string();

        ensure_codex_instruction_files(temp.path(), &config).unwrap();

        let instructions_dir = temp.path().join(".brehon").join("instructions");
        let worker =
            std::fs::read_to_string(instructions_dir.join("codex-worker-instructions.md")).unwrap();
        let reviewer =
            std::fs::read_to_string(instructions_dir.join("codex-reviewer-instructions.md"))
                .unwrap();
        let supervisor =
            std::fs::read_to_string(instructions_dir.join("codex-supervisor-instructions.md"))
                .unwrap();
        let advisor =
            std::fs::read_to_string(instructions_dir.join("codex-advisor-instructions.md"))
                .unwrap();
        let research =
            std::fs::read_to_string(instructions_dir.join("codex-research-instructions.md"))
                .unwrap();

        assert!(
            worker.contains("Do NOT proactively call `mcp__brehon__agent action=session_start`")
        );
        assert!(worker.contains("mcp__brehon__task action=progress"));
        assert!(reviewer.contains("action=submit_review"));
        assert!(supervisor.contains("mcp__brehon__task action=ready"));
        assert!(supervisor.contains("After any action that may change the frontier"));
        assert!(supervisor.contains("claude-supervisor"));
        assert!(advisor.contains("Brehon advisor startup"));
        assert!(advisor.contains("mcp__brehon__advisor"));
        assert!(advisor.contains("read-only"));
        assert!(research.contains("Brehon research startup"));
        assert!(research.contains("mcp__brehon__research action=claim_next"));
        assert!(research.contains("read-only"));
    }

    // ── ensure_brehon_ignored_in_repo ────────────────────────────────────────

    fn exclude_lines(path: &Path) -> Vec<String> {
        let contents =
            std::fs::read_to_string(path.join(".git").join("info").join("exclude")).unwrap();
        contents
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_writes_all_local_patterns_on_fresh_repo() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let lines = exclude_lines(temp.path());
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(
                lines.iter().any(|l| l.as_str() == *pattern),
                "{pattern} missing: {lines:?}"
            );
        }
        for pattern in BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS {
            assert!(
                lines.iter().any(|l| l == *pattern),
                "{pattern} missing: {lines:?}"
            );
        }
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_preserves_user_entries() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        std::fs::write(
            &exclude,
            "# user: my editor scratch\n.my-editor-scratch/\nbuild-local/\n",
        )
        .unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&exclude).unwrap();
        // User's existing lines must be preserved verbatim.
        assert!(contents.contains(".my-editor-scratch/"));
        assert!(contents.contains("build-local/"));
        // Brehon patterns get appended.
        assert!(contents.contains(".brehon/"));
        assert!(contents.contains(".mcp.json"));
        assert!(contents.contains(".agents/mcp_config.json"));
        assert!(contents.contains("opencode.json"));
        assert!(contents.contains(".antigravitycli"));
        assert!(contents.contains(".claude/settings.local.json"));
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_is_idempotent_when_all_patterns_present() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();
        let first = std::fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap();

        // Run a second time; the file shouldn't change (no duplicate
        // patterns, no extra headers, no trailing churn).
        ensure_brehon_ignored_in_repo(temp.path()).unwrap();
        let second = std::fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap();
        assert_eq!(
            first, second,
            "second call produced a diff; expected idempotence"
        );
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_only_adds_missing_patterns() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // Pre-populate with a legacy Brehon pattern and one current pattern.
        std::fs::write(&exclude, "!/.brehon/\n.mcp.json\n").unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let lines = exclude_lines(temp.path());
        assert!(!lines.iter().any(|l| l == ".brehon"));
        assert!(!lines.iter().any(|l| l == "!/.brehon/"));
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(
                lines.iter().any(|l| l.as_str() == *pattern),
                "{pattern} missing: {lines:?}"
            );
        }
        for pattern in BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS {
            assert!(
                lines.iter().any(|l| l == *pattern),
                "{pattern} missing: {lines:?}"
            );
        }
        assert_eq!(
            lines.iter().filter(|l| **l == ".mcp.json").count(),
            1,
            ".mcp.json was duplicated: {lines:?}"
        );
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_no_duplicate_header_when_partially_present() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // Pre-populate with the full header and one Brehon pattern.
        std::fs::write(
            &exclude,
            "# Brehon local scaffolding (auto-managed; safe to edit)\n.brehon/\n",
        )
        .unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&exclude).unwrap();
        let header_count = contents
            .lines()
            .filter(|l| l.trim() == "# Brehon local scaffolding (auto-managed; safe to edit)")
            .count();
        assert_eq!(header_count, 1, "duplicate header found:\n{contents}");

        // Patterns should land contiguously under the existing header.
        let all_lines: Vec<_> = contents.lines().collect();
        let header_idx = all_lines
            .iter()
            .position(|l| l.trim() == "# Brehon local scaffolding (auto-managed; safe to edit)")
            .expect("header exists");
        let brehon_block = &all_lines[header_idx..];
        let blank_inside_block = brehon_block
            .windows(2)
            .any(|w| w[0].trim() == ".brehon/" && w[1].trim().is_empty());
        assert!(
            !blank_inside_block,
            "blank line inside Brehon block:\n{contents}"
        );

        // All patterns should now be present.
        let lines: std::collections::HashSet<_> = contents.lines().map(str::trim).collect();
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {contents}");
        }
        for pattern in BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {contents}");
        }
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_no_duplicate_header_when_header_text_edited() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // User edited the parenthetical note — this is advertised as safe to do.
        std::fs::write(
            &exclude,
            "# Brehon local scaffolding (safe to edit — modified by user)\n.brehon/\n",
        )
        .unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&exclude).unwrap();
        let header_count = contents
            .lines()
            .filter(|l| l.trim().starts_with(BREHON_GITIGNORE_HEADER_PREFIX))
            .count();
        assert_eq!(
            header_count, 1,
            "duplicate header found after user edit:\n{contents}"
        );

        // All patterns should still be present under the existing edited header.
        let lines: std::collections::HashSet<_> = contents.lines().map(str::trim).collect();
        for pattern in brehon_git::WORKTREE_AWARE_BREHON_IGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {contents}");
        }
        for pattern in BREHON_LOCAL_EXTRA_GITIGNORE_PATTERNS {
            assert!(lines.contains(*pattern), "{pattern} missing: {contents}");
        }
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_no_ops_outside_git_repo() {
        // Bare tempdir — no .git present.
        let temp = tempfile::tempdir().unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        // Must not create .git/info/exclude in a non-git dir.
        assert!(
            !temp.path().join(".git").exists(),
            "ensure_brehon_ignored_in_repo polluted a non-git directory"
        );
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_handles_existing_file_without_trailing_newline() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let exclude = temp.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // Deliberately no trailing newline.
        std::fs::write(&exclude, "existing-no-newline").unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&exclude).unwrap();
        // The user's line must survive, and brehon's entries must be on
        // their own lines (no concatenation).
        assert!(contents.contains("existing-no-newline\n"));
        assert!(contents.contains("\n.brehon/\n"));
        assert!(contents.contains("\n.mcp.json\n"));
        assert!(contents.contains("\n.agents/mcp_config.json\n"));
        assert!(contents.contains("\nopencode.json\n"));
        assert!(contents.contains("\n.antigravitycli\n"));
    }

    #[test]
    fn ensure_brehon_ignored_in_repo_hides_nested_worktree_gitignore_rules() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let worktree_dir = temp
            .path()
            .join(".brehon/worktrees/runs/session-a/agy-worker");
        std::fs::create_dir_all(worktree_dir.join(".agents")).unwrap();
        std::fs::write(worktree_dir.join(".git"), "gitdir: /tmp/not-used\n").unwrap();
        std::fs::write(worktree_dir.join(".gitignore"), "!.mcp.json.example\n").unwrap();
        std::fs::write(worktree_dir.join(".agents/mcp_config.json"), "{}\n").unwrap();
        std::fs::write(worktree_dir.join(".mcp.json.example"), "{}\n").unwrap();
        std::fs::write(worktree_dir.join("src.txt"), "work\n").unwrap();

        let ignored_dir = run_git(
            temp.path(),
            &[
                "check-ignore",
                "-v",
                ".brehon/worktrees/runs/session-a/agy-worker",
            ],
        );
        assert!(
            ignored_dir.contains(".brehon/"),
            "worktree directory should stay ignored from the shared root: {ignored_dir}"
        );

        let ignored_file = run_git(
            temp.path(),
            &[
                "check-ignore",
                "-v",
                ".brehon/worktrees/runs/session-a/agy-worker/src.txt",
            ],
        );
        assert!(
            ignored_file.contains(".brehon/"),
            "worktree files should stay ignored from the shared root: {ignored_file}"
        );

        let status = run_git(
            temp.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert!(
            !status.contains(".brehon"),
            "worktree contents dirtied shared root status:\n{status}"
        );
    }

    #[test]
    fn ensure_mcp_config_auto_ignores_generated_files() {
        // End-to-end contract: generating .mcp.json + .claude settings
        // also registers local agent MCP config paths in .git/info/exclude
        // in one call, so a fresh developer checkout never sees
        // uncommitted noise.
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        ensure_mcp_config(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join(".agents")).unwrap();
        std::fs::write(
            temp.path().join(".agents/mcp_config.json"),
            r#"{"mcpServers":{"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#,
        )
        .unwrap();

        // Files were generated as expected.
        assert!(temp.path().join(".mcp.json").exists());
        assert!(temp.path().join("opencode.json").exists());
        assert!(temp.path().join(".claude/settings.local.json").exists());
        assert!(temp.path().join(".agents/mcp_config.json").exists());

        // And they're also git-ignored.
        let lines = exclude_lines(temp.path());
        assert!(
            lines.iter().any(|l| l == ".mcp.json"),
            "ensure_mcp_config did not auto-ignore .mcp.json: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == ".agents/mcp_config.json"),
            "ensure_mcp_config did not auto-ignore .agents/mcp_config.json: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "opencode.json"),
            "ensure_mcp_config did not auto-ignore opencode.json: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == ".claude/settings.local.json"),
            "ensure_mcp_config did not auto-ignore .claude/settings.local.json: {lines:?}"
        );

        // Verify via actual git: `git status` should not show the
        // generated files as untracked.
        let status = run_git(temp.path(), &["status", "--porcelain"]);
        assert!(
            !status.contains(".mcp.json"),
            "git status still sees .mcp.json as untracked:\n{status}"
        );
        assert!(
            !status.contains(".agents"),
            "git status still sees .agents local config as untracked:\n{status}"
        );
        assert!(
            !status.contains("opencode.json"),
            "git status still sees opencode.json as untracked:\n{status}"
        );
        assert!(
            !status.contains(".claude/settings.local.json"),
            "git status still sees settings.local.json as untracked:\n{status}"
        );
    }

    #[test]
    fn ensure_mcp_config_updates_stale_brehon_server_entry() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        std::fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"agora":{"command":"agora","args":["serve"]},"other":{"command":"other"},"brehon":{"command":"/stale/brehon","args":["serve"]}}}"#,
        )
        .unwrap();

        ensure_mcp_config(temp.path()).unwrap();

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(temp.path().join(".mcp.json")).unwrap())
                .unwrap();
        let expected_exe = std::env::current_exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| "brehon".to_string());
        assert_eq!(config["mcpServers"]["brehon"]["command"], expected_exe);
        assert_eq!(
            config["mcpServers"]["brehon"]["args"],
            serde_json::json!(["serve"])
        );
        assert_eq!(
            config["mcpServers"]["brehon"]["env"]["BREHON_PROJECT_ROOT"],
            temp.path().to_string_lossy().to_string()
        );
        assert_eq!(
            config["mcpServers"]["brehon"]["env"]["BREHON_ROOT"],
            temp.path().join(".brehon").to_string_lossy().to_string()
        );
        assert_eq!(
            config["mcpServers"]["brehon"]["env"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );
        assert_eq!(config["mcpServers"]["other"]["command"], "other");
        assert!(config["mcpServers"].get("agora").is_none());

        let opencode_config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(temp.path().join("opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(opencode_config["mcp"]["brehon"]["type"], "local");
        assert_eq!(
            opencode_config["mcp"]["brehon"]["command"],
            serde_json::json!([expected_exe, "serve"])
        );
        assert_eq!(
            opencode_config["mcp"]["brehon"]["environment"]["BREHON_PROJECT_ROOT"],
            temp.path().to_string_lossy().to_string()
        );
        assert_eq!(
            opencode_config["mcp"]["brehon"]["environment"]["BREHON_ROOT"],
            temp.path().join(".brehon").to_string_lossy().to_string()
        );
        assert_eq!(
            opencode_config["mcp"]["brehon"]["environment"][BREHON_MCP_BACKING_ENV],
            MCP_BACKING_RUNTIME_FILES
        );
    }
}
