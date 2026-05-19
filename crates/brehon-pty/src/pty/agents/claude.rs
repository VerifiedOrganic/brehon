use std::path::{Path, PathBuf};

use crate::pty::config::{PtyConfig, TeamsSpawnConfig};
use crate::pty::filesystem::write_json_config;

use super::{
    current_brehon_exe, current_brehon_session_name, prepend_current_exe_dir_to_path,
    push_brehon_root_env, push_workspace_root_env,
};

/// Directory (inside the agent's cwd) where we persist the per-agent Claude
/// MCP config. This path is under `.brehon/` which is already gitignored, so
/// writing it does not dirty the worktree's git state.
fn claude_factory_runtime_dir(cwd: &Path, name: &str) -> PathBuf {
    cwd.join(".brehon/factory-runtime/claude").join(name)
}

fn claude_mcp_config_path(cwd: &Path, name: &str) -> PathBuf {
    claude_factory_runtime_dir(cwd, name).join("mcp.json")
}

/// Build the BREHON_* env block that the per-agent MCP server config must
/// declare explicitly. Claude Code's MCP subprocess spawn does not reliably
/// inherit these vars from the Claude CLI's process env (observed: Claude-
/// Teams reviewers whose `brehon serve` children wrote to `prompt-queue/_legacy/`
/// because `BREHON_SESSION_NAME` was missing from the MCP child env). Declaring
/// the env inline in the MCP config eliminates dependence on inheritance.
#[allow(clippy::too_many_arguments)]
fn claude_brehon_mcp_env(
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    name: &str,
    role: &str,
    brehon_agent_type: &str,
    session_id: &str,
    session_name: Option<&str>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
    team_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("BREHON_AGENT_NAME".to_string(), name.to_string()),
        ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
        ("BREHON_AGENT_TYPE".to_string(), brehon_agent_type.to_string()),
        ("BREHON_SESSION_ID".to_string(), session_id.to_string()),
        (
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
        (
            "BREHON_WORKSPACE_ROOT".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
    ];

    // Session name: prefer explicit argument, fall back to parent proc env.
    // `_legacy/` prompt-queue writes are the symptom when this resolves empty,
    // so we log loud at the boundary rather than silently dropping the key.
    let resolved_session_name = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(current_brehon_session_name);
    if let Some(value) = resolved_session_name {
        env.push(("BREHON_SESSION_NAME".to_string(), value));
    } else {
        tracing::error!(
            agent = %name,
            role = %role,
            "Claude agent MCP config being written without BREHON_SESSION_NAME — \
             MCP children will write prompts to _legacy/ and require recovery sweep. \
             Fix the caller to thread the live session name through."
        );
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
    if let Some(team_name) = team_name.filter(|value| !value.trim().is_empty()) {
        env.push(("BREHON_TEAM_NAME".to_string(), team_name.to_string()));
    }

    env
}

/// Build the MCP server config JSON that Claude will load via `--mcp-config`.
/// Includes an explicit `env` block so the spawned `brehon serve` subprocess
/// sees the BREHON_* vars regardless of Claude Code's inheritance behavior.
#[allow(clippy::too_many_arguments)]
pub(crate) fn desired_claude_mcp_config(
    exe: &str,
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    name: &str,
    role: &str,
    brehon_agent_type: &str,
    session_id: &str,
    session_name: Option<&str>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
    team_name: Option<&str>,
) -> serde_json::Value {
    let env_map = claude_brehon_mcp_env(
        cwd,
        brehon_root,
        name,
        role,
        brehon_agent_type,
        session_id,
        session_name,
        supervisor_name,
        factory_worker_cli,
        team_name,
    )
    .into_iter()
    .map(|(key, value)| (key, serde_json::Value::String(value)))
    .collect::<serde_json::Map<_, _>>();

    serde_json::json!({
        "mcpServers": {
            "brehon": {
                "command": exe,
                "args": ["serve"],
                "env": env_map,
            }
        }
    })
}

impl PtyConfig {
    /// Create config for a Claude CLI instance.
    ///
    /// # Arguments
    /// * `name` - Agent name
    /// * `role` - Agent role (e.g., "worker", "supervisor")
    /// * `session_name` - Live Brehon session name. Threaded into the MCP
    ///   child env via a generated `.brehon/factory-runtime/claude/<name>/mcp.json`
    ///   so Claude's spawned `brehon serve` subprocess can route prompt-queue
    ///   writes to the correct session dir. Silent-drop of this value was the
    ///   root cause of `prompt-queue/_legacy/` receiving live traffic.
    /// * `cwd` - Working directory for the agent
    /// * `brehon_root` - Optional path to the .brehon directory. If provided, sets BREHON_ROOT env var
    ///   so workers in clones can access the main repo's Brehon state.
    /// * `supervisor_name` - For workers, the name of their supervisor (enables `target: supervisor`)
    #[allow(clippy::too_many_arguments)]
    pub fn claude(
        name: &str,
        role: &str,
        brehon_agent_type: Option<&str>,
        session_name: Option<&str>,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        // Generate a stable session ID for this agent so the MCP server can
        // auto-register even when Claude Code hooks don't fire (e.g. in
        // worktree-isolated workers on Claude Code ≥2.1.58).
        let session_id = uuid::Uuid::new_v4().to_string();
        let brehon_agent_type = brehon_agent_type
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("claude-code");
        let brehon_exe = current_brehon_exe();
        let team_name = teams.map(|t| t.team_name.as_str());

        // Write the per-agent MCP config with explicit env so the spawned
        // `brehon serve` subprocess sees BREHON_SESSION_NAME and friends
        // without depending on Claude Code's env-inheritance behavior.
        let mcp_config_path = claude_mcp_config_path(&cwd, name);
        let mcp_config = desired_claude_mcp_config(
            &brehon_exe,
            &cwd,
            brehon_root,
            name,
            role,
            brehon_agent_type,
            &session_id,
            session_name,
            supervisor_name,
            factory_worker_cli,
            team_name,
        );
        if let Err(e) = write_json_config(&mcp_config_path, &mcp_config) {
            tracing::error!(
                agent = %name,
                path = %mcp_config_path.display(),
                error = %e,
                "Failed to write per-agent Claude MCP config; falling back to \
                 repo-root .mcp.json inheritance which is known-fragile."
            );
        }

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            ("BREHON_AGENT_TYPE".to_string(), brehon_agent_type.to_string()),
            // Provide session ID so Brehon MCP server can self-register without hooks
            ("BREHON_SESSION_ID".to_string(), session_id.clone()),
            // Set clone path so subagents know the worktree directory
            (
                "BREHON_CLONE_PATH".to_string(),
                cwd.to_string_lossy().to_string(),
            ),
            // Suppress interactive prompts, telemetry, and updates for factory agents
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                "1".to_string(),
            ),
            ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
            ("DISABLE_COST_WARNINGS".to_string(), "1".to_string()),
            (
                "CLAUDE_CODE_DISABLE_TERMINAL_TITLE".to_string(),
                "1".to_string(),
            ),
            ("IS_DEMO".to_string(), "true".to_string()),
        ];
        prepend_current_exe_dir_to_path(&mut env);
        push_workspace_root_env(&mut env, &cwd);

        // Propagate session name into the Claude CLI's own process env. This
        // covers anything Claude Code does that reads env directly (hooks,
        // skill launchers, subprocesses other than MCP). The MCP subprocess
        // env is handled separately via the generated --mcp-config above.
        if let Some(resolved) = session_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(current_brehon_session_name)
        {
            env.push(("BREHON_SESSION_NAME".to_string(), resolved));
        }

        // Set BREHON_ROOT / BREHON_PROJECT_ROOT env vars if provided
        // (enables workers in clones to use main's .brehon).
        if let Some(root) = brehon_root {
            push_brehon_root_env(&mut env, root);
        }

        // Set supervisor name for workers (enables `target: supervisor` in message action)
        if let Some(sup) = supervisor_name {
            env.push(("BREHON_SUPERVISOR_NAME".to_string(), sup.to_string()));
        }
        if let Some(worker_cli) = factory_worker_cli {
            env.push((
                "BREHON_FACTORY_WORKER_CLI".to_string(),
                worker_cli.to_string(),
            ));
        }

        // Expose team name so MCP tools can deliver to Teams inbox
        if let Some(t) = teams {
            env.push(("BREHON_TEAM_NAME".to_string(), t.team_name.clone()));
        }

        // Enable native Agent Teams for inter-agent messaging
        if teams.is_some() {
            env.push((
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string(),
            ));
        }

        let mut args = vec!["--dangerously-skip-permissions".to_string()];
        // Point Claude at the per-agent MCP config we just wrote. Strict mode
        // ensures Claude ignores the repo-root `.mcp.json` (which has no env
        // block) and uses only our config. Factory agents don't need any other
        // MCP servers, so strict mode is the deterministic choice.
        args.push("--mcp-config".to_string());
        args.push(mcp_config_path.to_string_lossy().to_string());
        args.push("--strict-mcp-config".to_string());
        // When using Agent Teams, session identity comes from --agent-id, not --session-id.
        // Including both causes conflicts in Claude Code's session tracking.
        if teams.is_none() {
            args.push("--session-id".to_string());
            args.push(session_id);
        }
        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m.to_string());
        }
        args.push("--effort".to_string());
        args.push(
            reasoning_effort
                .unwrap_or(if role == "supervisor" {
                    "high"
                } else {
                    "medium"
                })
                .to_string(),
        );

        // Add native Agent Teams CLI flags.
        // All agents (including the supervisor) get --teammate-mode tmux
        // so Claude Code activates inbox polling for everyone.
        if let Some(t) = teams {
            args.push("--team-name".to_string());
            args.push(t.team_name.clone());
            args.push("--agent-id".to_string());
            args.push(t.agent_id.clone());
            args.push("--agent-name".to_string());
            args.push(t.agent_name.clone());
            args.push("--agent-color".to_string());
            args.push(t.agent_color.clone());
            args.push("--agent-type".to_string());
            args.push(t.agent_type.clone());
            args.push("--teammate-mode".to_string());
            args.push("tmux".to_string());
            if let Some(ref parent_id) = t.parent_session_id {
                args.push("--parent-session-id".to_string());
                args.push(parent_id.clone());
            }
        }

        Self {
            command: "claude".to_string(),
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }
}
