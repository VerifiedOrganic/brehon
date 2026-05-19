//! Claude CLI adapter for Brehon — PTY-native hooks path.
//!
//! This crate contains claude-specific harness configuration, hook
//! definitions, and the [`AgentAdapter`](brehon_adapter_sdk::AgentAdapter) implementation that governs how
//! Brehon interacts with the Claude Code CLI over its native hooks protocol.
//!
//! # Architecture
//!
//! Claude is the *only* built-in agent that uses the `NativeHooks`
//! transport.  It does **not** have an ACP session variant — the
//! generic ACP stdio path in `brehon-acp` is sufficient for agents
//! that opt into that transport.  All claude-specific logic (CLI
//! arguments, environment variables, hook configuration, teams
//! inbox polling) lives here.

pub mod harness;

use brehon_adapter_sdk::{
    prepend_current_exe_dir_to_path, push_brehon_root_env, push_workspace_root_env,
};

/// Claude-specific session configuration.
///
/// Captures everything the Claude adapter needs to spawn and manage a
/// Claude Code CLI session beyond what the generic [`SessionSpec`](brehon_types::SessionSpec)
/// already carries.
#[derive(Debug, Clone)]
pub struct ClaudeSessionConfig {
    /// CLI command (usually `"claude"`).
    pub command: String,
    /// Command-line arguments forwarded to the CLI.
    pub args: Vec<String>,
    /// Environment variables set at spawn time.
    pub env: Vec<(String, String)>,
    /// Working directory for the spawned process.
    pub cwd: Option<std::path::PathBuf>,
    /// Initial terminal rows.
    pub rows: u16,
    /// Initial terminal columns.
    pub cols: u16,
}

impl ClaudeSessionConfig {
    /// Build the standard Claude CLI argument list from a [`ClaudeSpawnParams`].
    ///
    /// This is the core of the PTY harness: it assembles all CLI flags,
    /// session IDs, model overrides, and Agent Teams parameters that the
    /// Claude Code CLI expects.
    pub fn from_params(params: &ClaudeSpawnParams) -> Self {
        let mut args = vec!["--dangerously-skip-permissions".to_string()];

        if params.teams.is_none() {
            args.push("--session-id".to_string());
            args.push(params.session_id.clone());
        }

        if let Some(model) = &params.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }

        args.push("--effort".to_string());
        let effort = params
            .reasoning_effort
            .as_deref()
            .unwrap_or(if params.role == "supervisor" {
                "high"
            } else {
                "medium"
            });
        args.push(effort.to_string());

        if let Some(teams) = &params.teams {
            args.push("--team-name".to_string());
            args.push(teams.team_name.clone());
            args.push("--agent-id".to_string());
            args.push(teams.agent_id.clone());
            args.push("--agent-name".to_string());
            args.push(teams.agent_name.clone());
            args.push("--agent-color".to_string());
            args.push(teams.agent_color.clone());
            args.push("--agent-type".to_string());
            args.push(teams.agent_type.clone());
            args.push("--teammate-mode".to_string());
            args.push("tmux".to_string());
            if let Some(parent_id) = &teams.parent_session_id {
                args.push("--parent-session-id".to_string());
                args.push(parent_id.clone());
            }
        }

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), params.name.clone()),
            ("BREHON_AGENT_ROLE".to_string(), params.role.clone()),
            (
                "BREHON_AGENT_TYPE".to_string(),
                params.brehon_agent_type.clone(),
            ),
            ("BREHON_SESSION_ID".to_string(), params.session_id.clone()),
            (
                "BREHON_CLONE_PATH".to_string(),
                params.cwd.to_string_lossy().to_string(),
            ),
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
        push_workspace_root_env(&mut env, &params.cwd);

        if let Some(root) = &params.brehon_root {
            push_brehon_root_env(&mut env, root);
        }

        if let Some(sup) = &params.supervisor_name {
            env.push(("BREHON_SUPERVISOR_NAME".to_string(), sup.clone()));
        }

        if let Some(worker_cli) = &params.factory_worker_cli {
            env.push(("BREHON_FACTORY_WORKER_CLI".to_string(), worker_cli.clone()));
        }

        if let Some(teams) = &params.teams {
            env.push(("BREHON_TEAM_NAME".to_string(), teams.team_name.clone()));
            env.push((
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "1".to_string(),
            ));
        }

        Self {
            command: "claude".to_string(),
            args,
            env,
            cwd: Some(params.cwd.clone()),
            rows: 24,
            cols: 80,
        }
    }
}

/// Parameters for spawning a Claude CLI session.
///
/// This struct carries all the information needed to construct the
/// full CLI invocation and is the primary interface that `brehon-pty`
/// and `brehon-mux` use when they need to spawn a Claude agent.
#[derive(Debug, Clone)]
pub struct ClaudeSpawnParams {
    /// Agent name (e.g. `"worker-1"`, `"supervisor"`).
    pub name: String,
    /// Agent role (`"worker"`, `"supervisor"`, `"reviewer"`).
    pub role: String,
    /// Agent type override (e.g. `"claude-reviewer"`). Defaults to `"claude-code"`.
    pub brehon_agent_type: String,
    /// Stable session UUID for MCP registration.
    pub session_id: String,
    /// Working directory for the spawned process.
    pub cwd: std::path::PathBuf,
    /// Optional `.brehon` root path (for workers in clones).
    pub brehon_root: Option<std::path::PathBuf>,
    /// Optional supervisor name (worker-only).
    pub supervisor_name: Option<String>,
    /// Optional factory worker CLI type string.
    pub factory_worker_cli: Option<String>,
    /// Optional model override.
    pub model: Option<String>,
    /// Optional reasoning effort override.
    pub reasoning_effort: Option<String>,
    /// Optional Agent Teams configuration.
    pub teams: Option<ClaudeTeamsConfig>,
}

impl ClaudeSpawnParams {
    /// Construct a [`ClaudeSpawnParams`] with sensible defaults.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        role: &str,
        brehon_agent_type: Option<&str>,
        cwd: std::path::PathBuf,
        brehon_root: Option<&std::path::PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        teams: Option<&ClaudeTeamsConfig>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let agent_type = brehon_agent_type
            .filter(|v| !v.trim().is_empty())
            .unwrap_or("claude-code")
            .to_string();
        Self {
            name: name.to_string(),
            role: role.to_string(),
            brehon_agent_type: agent_type,
            session_id: uuid::Uuid::new_v4().to_string(),
            cwd,
            brehon_root: brehon_root.cloned(),
            supervisor_name: supervisor_name.map(|s| s.to_string()),
            factory_worker_cli: factory_worker_cli.map(|s| s.to_string()),
            model: model.map(|m| m.to_string()),
            reasoning_effort: reasoning_effort.map(|r| r.to_string()),
            teams: teams.cloned(),
        }
    }
}

/// Native Agent Teams configuration for Claude CLI.
///
/// Mirrors the `TeamsSpawnConfig` from
/// `brehon-pty`, kept in this crate to avoid `brehon-pty` depending on
/// `brehon-adapter-claude`.
#[derive(Debug, Clone)]
pub struct ClaudeTeamsConfig {
    /// Team name (factory session name).
    pub team_name: String,
    /// Agent ID (e.g. `"worker-1@session-name"`).
    pub agent_id: String,
    /// Agent display name.
    pub agent_name: String,
    /// Agent color for UI.
    pub agent_color: String,
    /// Agent type (e.g. `"team-lead"`, `"general-purpose"`).
    pub agent_type: String,
    /// Parent session ID for analytics correlation (workers only).
    pub parent_session_id: Option<String>,
}

/// Claude-specific adapter placeholder.
///
/// This adapter governs the Claude Code CLI's **native hooks** path.
/// It does NOT implement ACP (Agent Client Protocol) — claude
/// communicates through its hook protocol and PTY interaction.
///
/// The full [`brehon_adapter_sdk::AgentAdapter`] implementation for
/// Claude's PTY/native-hooks lifecycle is not yet wired; spawning
/// is currently handled directly by `brehon-pty`.
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    /// Create a new Claude adapter instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_spawn_params_defaults_agent_type() {
        let params = ClaudeSpawnParams::new(
            "worker-1",
            "worker",
            None,
            std::path::PathBuf::from("/tmp/work"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(params.brehon_agent_type, "claude-code");
        assert_eq!(params.name, "worker-1");
        assert_eq!(params.role, "worker");
        assert!(!params.session_id.is_empty());
    }

    #[test]
    fn claude_spawn_params_preserves_agent_type() {
        let params = ClaudeSpawnParams::new(
            "reviewer-1",
            "reviewer",
            Some("claude-reviewer"),
            std::path::PathBuf::from("/tmp/work2"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(params.brehon_agent_type, "claude-reviewer");
    }

    #[test]
    fn claude_session_config_includes_dangerously_skip_permissions() {
        let params = ClaudeSpawnParams::new(
            "test",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert_eq!(config.command, "claude");
        assert!(
            config
                .args
                .contains(&"--dangerously-skip-permissions".to_string()),
            "should include --dangerously-skip-permissions"
        );
    }

    #[test]
    fn claude_session_config_without_teams_includes_session_id() {
        let params = ClaudeSpawnParams::new(
            "test",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(config.args.contains(&"--session-id".to_string()));
        assert!(config.args.contains(&params.session_id));
    }

    #[test]
    fn claude_session_config_with_teams_omits_session_id() {
        let teams = ClaudeTeamsConfig {
            team_name: "test-team".to_string(),
            agent_id: "worker-1@test-team".to_string(),
            agent_name: "worker-1".to_string(),
            agent_color: "blue".to_string(),
            agent_type: "general-purpose".to_string(),
            parent_session_id: Some("lead-123".to_string()),
        };
        let params = ClaudeSpawnParams::new(
            "worker-1",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            Some(&teams),
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(config.args.contains(&"--team-name".to_string()));
        assert!(config.args.contains(&"--teammate-mode".to_string()));
        assert!(config.args.contains(&"tmux".to_string()));
        assert!(!config.args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn claude_session_config_with_model() {
        let params = ClaudeSpawnParams::new(
            "test",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            Some("claude-opus-4-6"),
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(config.args.contains(&"--model".to_string()));
        assert!(config.args.contains(&"claude-opus-4-6".to_string()));
    }

    #[test]
    fn claude_session_config_env_sets_agent_name() {
        let params = ClaudeSpawnParams::new(
            "my-agent",
            "worker",
            None,
            std::path::PathBuf::from("/tmp/work"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_NAME" && v == "my-agent")
        );
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_AGENT_ROLE" && v == "worker")
        );
    }

    #[test]
    fn claude_session_config_includes_effort_medium_for_worker() {
        let params = ClaudeSpawnParams::new(
            "test",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(config.args.contains(&"--effort".to_string()));
        assert!(config.args.contains(&"medium".to_string()));
    }

    #[test]
    fn claude_session_config_includes_effort_high_for_supervisor() {
        let params = ClaudeSpawnParams::new(
            "test",
            "supervisor",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(config.args.contains(&"high".to_string()));
    }

    #[test]
    fn claude_session_config_with_brehon_root() {
        let root = std::path::PathBuf::from("/home/user/project/.brehon");
        let params = ClaudeSpawnParams::new(
            "test",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            Some(&root),
            None,
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_ROOT" && v == "/home/user/project/.brehon")
        );
    }

    #[test]
    fn claude_session_config_with_supervisor_name() {
        let params = ClaudeSpawnParams::new(
            "worker-1",
            "worker",
            None,
            std::path::PathBuf::from("/tmp"),
            None,
            Some("my-supervisor"),
            None,
            None,
            None,
            None,
        );
        let config = ClaudeSessionConfig::from_params(&params);
        assert!(
            config
                .env
                .iter()
                .any(|(k, v)| k == "BREHON_SUPERVISOR_NAME" && v == "my-supervisor")
        );
    }
}
