//! Pane constructors and process spawn helpers.

use crate::error::{Error, Result};
use crate::harness::{AgentAdapter, SupervisorCli};
use crate::mux::AgentPaneMaterialization;
use crate::pane::types::{GatewaySpawnConfig, Pane, PaneBackend, PaneKind};
use crate::pty::{Pty, PtyConfig, TeamsSpawnConfig};
use brehon_acp::GatewayProtocol;
use brehon_types::config::SandboxProfile;
use ghostty_vt::{Rgb, Terminal};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACP_SIDECAR_CONNECT_TIMEOUT_MS: u64 = 5_000;
const GROK_BREHON_SANDBOX_CONFIG_DIR_ENV: &str = "GROK_BREHON_SANDBOX_CONFIG_DIR";
const GROK_BREHON_SANDBOX_MARKER_PREFIX: &str = "# Brehon managed Grok sandbox profile:";

pub(crate) fn uses_ink_echo_injection(cli_type: &AgentAdapter) -> bool {
    cli_type
        .capabilities()
        .prompt_injection_strategy
        .uses_ink_echo()
}

pub(crate) fn uses_delayed_submit_injection(cli_type: &AgentAdapter) -> bool {
    // Only used as fallback when ACP delivery is unavailable.
    cli_type
        .capabilities()
        .prompt_injection_strategy
        .uses_delayed_submit()
}

/// Resolve the gateway protocol for a gateway-backed adapter.
///
/// Callers must only invoke this after PTY-first built-ins have been filtered
/// out via `built_in_uses_pty_launch_contract()`. Unsupported built-in
/// transport/control-plane overrides are normalized away at adapter
/// construction/validation time, so built-in identity remains authoritative for
/// the gateway-capable contracts implemented here.
fn gateway_protocol_for(cli_type: &AgentAdapter) -> GatewayProtocol {
    if let Some(builtin) = cli_type.as_builtin() {
        let transport = cli_type.capabilities().transport;
        if matches!(
            transport,
            crate::harness::HarnessTransport::NativeHooks
                | crate::harness::HarnessTransport::InteractivePty
                | crate::harness::HarnessTransport::OneShotPty
        ) {
            debug_assert!(
                false,
                "gateway_protocol_for called for built-in '{}' with non-gateway transport {}; gate with built_in_uses_pty_launch_contract() before selecting a gateway protocol",
                builtin.as_str(),
                transport,
            );
            tracing::error!(
                builtin = builtin.as_str(),
                transport = %transport,
                "gateway_protocol_for called for built-in with non-gateway transport; defaulting to ACP stdio"
            );
            return GatewayProtocol::AcpStdio;
        }
        return match builtin {
            SupervisorCli::Codex => GatewayProtocol::CodexAppServerWs,
            SupervisorCli::Gemini => GatewayProtocol::GeminiAcpStdio,
            SupervisorCli::Kimi => GatewayProtocol::KimiAcpStdio,
            SupervisorCli::OpenCode => GatewayProtocol::OpenCodeServer,
            _ => GatewayProtocol::AcpStdio,
        };
    }

    match cli_type {
        AgentAdapter::Custom(custom) if is_custom_codex_app_server(custom) => {
            GatewayProtocol::CodexAppServerWs
        }
        AgentAdapter::Custom(custom)
            if custom.capabilities.preferred_control_plane
                == crate::harness::HarnessControlPlane::AcpSidecar =>
        {
            GatewayProtocol::AcpUnixSocket
        }
        AgentAdapter::Custom(custom)
            if custom.capabilities.preferred_control_plane
                == crate::harness::HarnessControlPlane::OpenAiCompatible =>
        {
            GatewayProtocol::OpenAiCompatibleChat
        }
        _ => GatewayProtocol::AcpStdio,
    }
}

fn built_in_uses_pty_launch_contract(
    adapter: &AgentAdapter,
    materialization: AgentPaneMaterialization,
) -> bool {
    materialization.is_plan_only()
        || matches!(
            adapter.capabilities().transport,
            crate::harness::HarnessTransport::NativeHooks
                | crate::harness::HarnessTransport::InteractivePty
        )
}

fn unsupported_builtin_one_shot_override(adapter: &AgentAdapter) -> Option<String> {
    use crate::harness::{HarnessControlPlane, HarnessTransport};

    let builtin = adapter.as_builtin()?;
    let builtin_capabilities = builtin.capabilities();
    let effective_capabilities = adapter.capabilities();
    let requests_one_shot_contract = effective_capabilities.one_shot
        || matches!(
            effective_capabilities.transport,
            HarnessTransport::OneShotPty
        )
        || matches!(
            effective_capabilities.preferred_control_plane,
            HarnessControlPlane::OneShot
        );
    if !requests_one_shot_contract || builtin_capabilities.one_shot {
        return None;
    }

    Some(format!(
        "Built-in agent '{}' does not support one-shot overrides; got transport={} control_plane={}. Configure a real one-shot launcher instead.",
        adapter.name(),
        effective_capabilities.transport,
        effective_capabilities.preferred_control_plane
    ))
}

#[cfg(any(test, feature = "test-pty-fallback"))]
pub(crate) fn spawn_config_for_pty_spawn(config: &PtyConfig) -> PtyConfig {
    let mut config = config.clone();
    apply_test_pty_spawn_fallback(&mut config);
    config
}

#[cfg(not(any(test, feature = "test-pty-fallback")))]
pub(crate) fn spawn_config_for_pty_spawn(config: &PtyConfig) -> PtyConfig {
    config.clone()
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn apply_test_pty_spawn_fallback(config: &mut PtyConfig) {
    if !is_test_pty_fallback_candidate(&config.command) || command_exists(&config.command) {
        return;
    }

    config.command = "sh".to_string();
    config.args = vec!["-c".to_string(), "cat".to_string()];
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn is_test_pty_fallback_candidate(command: &str) -> bool {
    matches!(
        command,
        "claude" | "codex" | "copilot" | "gemini" | "gh" | "junie" | "kimi" | "opencode" | "agy"
    )
}

#[cfg(any(test, feature = "test-pty-fallback"))]
fn command_exists(command: &str) -> bool {
    let path = std::path::Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }

    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

fn allocate_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| Error::pty(format!("Failed to allocate loopback port: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| Error::pty(format!("Failed to read allocated loopback port: {err}")))?
        .port();
    drop(listener);
    Ok(port)
}

fn is_custom_codex_app_server(custom: &crate::harness::CustomAgentConfig) -> bool {
    custom.command.as_deref() == Some("codex") && custom.args.iter().any(|arg| arg == "app-server")
}

fn custom_supervisor_requires_pty_error(
    adapter: &AgentAdapter,
    custom: &crate::harness::CustomAgentConfig,
) -> Option<String> {
    let capabilities = adapter.capabilities();
    if custom.command.as_deref().is_none() {
        return Some(format!(
            "Custom supervisor agent '{}' must provide an interactive PTY launch command; gateway/API-only supervisors are not supported",
            adapter.name()
        ));
    }
    let has_valid_supervisor_contract = if capabilities.preferred_control_plane
        == crate::harness::HarnessControlPlane::AcpSidecar
    {
        matches!(
            capabilities.transport,
            crate::harness::HarnessTransport::InteractivePty
        )
    } else {
        custom_agent_supports_direct_pty(adapter)
    };
    if !has_valid_supervisor_contract {
        return Some(format!(
            "Custom supervisor agent '{}' must be configured as an interactive PTY supervisor; got transport={} control_plane={}",
            adapter.name(),
            capabilities.transport,
            capabilities.preferred_control_plane
        ));
    }
    None
}

fn custom_non_supervisor_requires_pty_error(
    adapter: &AgentAdapter,
    custom: &crate::harness::CustomAgentConfig,
    role: &str,
) -> Option<String> {
    let capabilities = adapter.capabilities();
    if custom.command.as_deref().is_none() {
        return Some(format!(
            "Custom {role} agent '{}' must provide an interactive PTY launch command",
            adapter.name()
        ));
    }
    if !custom_agent_supports_direct_pty(adapter) {
        return Some(format!(
            "Custom {role} agent '{}' must be gateway-backed or configured as an interactive PTY; got transport={} control_plane={}",
            adapter.name(),
            capabilities.transport,
            capabilities.preferred_control_plane
        ));
    }
    None
}

fn custom_agent_supports_direct_pty(adapter: &AgentAdapter) -> bool {
    let capabilities = adapter.capabilities();
    match capabilities.preferred_control_plane {
        crate::harness::HarnessControlPlane::NativeHooks
        | crate::harness::HarnessControlPlane::PtyInjection => matches!(
            capabilities.transport,
            crate::harness::HarnessTransport::NativeHooks
                | crate::harness::HarnessTransport::InteractivePty
        ),
        _ => false,
    }
}

pub(crate) fn config_env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find_map(|(env_key, value)| (env_key == key).then(|| value.clone()))
}

fn set_config_env_value(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

fn acp_sidecar_contract_paths(
    config: &PtyConfig,
    brehon_root: Option<&PathBuf>,
    pane_name: &str,
) -> Result<(String, String)> {
    let root = brehon_root
        .cloned()
        .or_else(|| config_env_value(&config.env, "BREHON_ROOT").map(PathBuf::from))
        .ok_or_else(|| {
            Error::pty(format!(
                "Custom ACP sidecar supervisor '{pane_name}' requires BREHON_ROOT to publish its socket contract"
            ))
        })?;
    let session_id = config_env_value(&config.env, "BREHON_SESSION_ID").ok_or_else(|| {
        Error::pty(format!(
            "Custom ACP sidecar supervisor '{pane_name}' is missing BREHON_SESSION_ID"
        ))
    })?;
    let agent_name =
        config_env_value(&config.env, "BREHON_AGENT_NAME").unwrap_or_else(|| pane_name.to_string());
    let sidecar_dir = root
        .join("runtime")
        .join("sessions")
        .join(session_id)
        .join("agents")
        .join(agent_name);
    std::fs::create_dir_all(&sidecar_dir)?;
    let socket_path = sidecar_dir.join("acp.sock");
    let ready_path = sidecar_dir.join("acp.ready.json");
    Ok((
        socket_path.to_string_lossy().to_string(),
        ready_path.to_string_lossy().to_string(),
    ))
}

fn validate_codex_gateway_bootstrap(
    config: &PtyConfig,
    brehon_root: Option<&PathBuf>,
) -> Result<()> {
    if config.command != "codex" || !config.args.iter().any(|arg| arg == "app-server") {
        return Ok(());
    }

    if !config
        .env
        .iter()
        .any(|(key, value)| key == "CODEX_HOME" && !value.trim().is_empty())
    {
        return Err(Error::pty(
            "Codex app-server launch is missing CODEX_HOME bootstrap. Refusing to start a half-configured Codex session."
                .to_string(),
        ));
    }

    if !config
        .args
        .windows(2)
        .any(|window| window == ["--disable", "personality"])
    {
        return Err(Error::pty(
            "Codex app-server launch is missing '--disable personality'. Refusing to start without the standard Brehon Codex bootstrap."
                .to_string(),
        ));
    }
    if !config
        .args
        .windows(2)
        .any(|window| window == ["--disable", "apps"])
    {
        return Err(Error::pty(
            "Codex app-server launch is missing '--disable apps'. Refusing to start with Codex Apps MCP enabled."
                .to_string(),
        ));
    }

    let has_bypass = config
        .args
        .iter()
        .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox");
    let has_safe_approval_policy = config
        .args
        .windows(2)
        .any(|window| window == ["--ask-for-approval", "never"] || window == ["-a", "never"]);
    let has_safe_sandbox = config.args.windows(2).any(|window| {
        window == ["--sandbox", "read-only"]
            || window == ["--sandbox", "workspace-write"]
            || window == ["-s", "read-only"]
            || window == ["-s", "workspace-write"]
    });
    let has_safe_policy = has_safe_approval_policy && has_safe_sandbox;

    if !has_bypass && !has_safe_policy {
        return Err(Error::pty(
            "Codex app-server launch is missing the standard approval/sandbox bootstrap flags. Refusing to start."
                .to_string(),
        ));
    }

    let Some(brehon_root) = brehon_root else {
        return Ok(());
    };
    let role = config_env_value(&config.env, "BREHON_AGENT_ROLE").unwrap_or_default();
    let instructions_filename = match role.as_str() {
        "supervisor" => "codex-supervisor-instructions.md",
        "reviewer" => "codex-reviewer-instructions.md",
        "advisor" => "codex-advisor-instructions.md",
        "research" => "codex-research-instructions.md",
        _ => "codex-worker-instructions.md",
    };
    let instructions_path = brehon_root.join("instructions").join(instructions_filename);
    if !instructions_path.exists() {
        return Err(Error::pty(format!(
            "Codex app-server launch for role '{role}' requires '{}', but it does not exist. Refusing to start a degraded Codex session.",
            instructions_path.display()
        )));
    }
    let instructions_path_str = instructions_path.to_string_lossy();
    if !config.args.iter().any(|arg| {
        arg.contains("model_instructions_file=") && arg.contains(instructions_path_str.as_ref())
    }) {
        return Err(Error::pty(format!(
            "Codex app-server launch for role '{role}' is missing model_instructions_file='{}'. Refusing to start a degraded Codex session.",
            instructions_path.display()
        )));
    }

    Ok(())
}

fn apply_configured_agent_type(config: &mut PtyConfig, configured_agent_type: Option<&str>) {
    let Some(configured_agent_type) = configured_agent_type
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    set_config_env_value(&mut config.env, "BREHON_AGENT_TYPE", configured_agent_type);
}

fn merge_launcher_env(target_env: &mut Vec<(String, String)>, launcher_env: &[(String, String)]) {
    for (key, value) in launcher_env {
        if (key.starts_with("BREHON_")
            && key != "BREHON_ROLE_SYSTEM_PROMPT"
            && key != "BREHON_WORKTREE_ROOT")
            || key == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"
        {
            continue;
        }
        set_config_env_value(target_env, key, value);
    }
}

fn apply_runtime_model_metadata(
    env: &mut Vec<(String, String)>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        set_config_env_value(env, "BREHON_AGENT_MODEL", model);
    }
    if let Some(reasoning_effort) = reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        set_config_env_value(env, "BREHON_REASONING_EFFORT", reasoning_effort);
    }
}

fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

fn command_basename(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

fn is_native_agent_command(command: &str) -> bool {
    let name = command_basename(command);
    name == "native-agent" || name.ends_with("-native-agent")
}

fn is_grok_agent_stdio(command: &str, args: &[String]) -> bool {
    command_basename(command) == "grok"
        && args.iter().any(|arg| arg == "agent")
        && args.iter().any(|arg| arg == "stdio")
}

fn args_contain_option(args: &[String], option: &str) -> bool {
    args.iter().any(|arg| {
        arg == option
            || arg
                .strip_prefix(option)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

fn apply_grok_acp_hardening(config: &mut PtyConfig, command: &str) -> Result<()> {
    if !is_grok_agent_stdio(command, &config.args) {
        return Ok(());
    }

    let mut prefix_args = Vec::new();
    if !args_contain_option(&config.args, "--sandbox")
        && config_env_value(&config.env, "GROK_SANDBOX").is_none()
    {
        prefix_args.push("--sandbox".to_string());
        prefix_args.push(grok_sandbox_profile_for_config(config)?);
    }
    if !args_contain_option(&config.args, "--cwd")
        && let Some(cwd) = config.cwd.as_ref()
    {
        prefix_args.push("--cwd".to_string());
        prefix_args.push(cwd.to_string_lossy().to_string());
    }
    if !prefix_args.is_empty() {
        config.args.splice(0..0, prefix_args);
    }

    let server_env = config
        .env
        .iter()
        .filter(|(key, _)| key.starts_with("BREHON_"))
        .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
        .collect::<Vec<_>>();
    let mcp_servers = serde_json::json!([{
        "name": "brehon",
        "type": "stdio",
        "command": current_brehon_exe(),
        "args": ["serve"],
        "env": server_env,
    }]);
    set_config_env_value(
        &mut config.env,
        "BREHON_ACP_MCP_SERVERS_JSON",
        &mcp_servers.to_string(),
    );
    Ok(())
}

fn grok_sandbox_profile_for_config(config: &PtyConfig) -> Result<String> {
    if config_env_value(&config.env, "BREHON_LAUNCH_POLICY_UNSAFE").as_deref() == Some("true") {
        return Ok("off".to_string());
    }

    let Some(cwd) = config.cwd.as_deref() else {
        return Ok("workspace".to_string());
    };

    let read_write_paths = grok_brehon_read_write_paths(config);
    if read_write_paths.is_empty() {
        return Ok("workspace".to_string());
    }

    let profile_name = grok_brehon_profile_name(config, cwd);
    upsert_grok_brehon_sandbox_profile(config, &profile_name, &read_write_paths)?;
    Ok(profile_name)
}

fn grok_brehon_profile_name(config: &PtyConfig, cwd: &Path) -> String {
    let project_root = config_env_value(&config.env, "BREHON_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());
    let label = project_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_grok_profile_component)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let hash = stable_path_hash(&project_root.to_string_lossy());
    format!("brehon-{label}-{hash:016x}")
}

fn sanitize_grok_profile_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn stable_path_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn grok_brehon_read_write_paths(config: &PtyConfig) -> Vec<PathBuf> {
    let cwd = config.cwd.as_deref();
    let mut paths = Vec::new();
    push_env_path(&mut paths, cwd, &config.env, "BREHON_ROOT");

    if let Some(project_root) = config_env_path(cwd, &config.env, "BREHON_PROJECT_ROOT") {
        push_path(&mut paths, project_root.join(".git"));
        push_git_metadata_paths(&mut paths, &project_root);
    }
    if let Some(cwd) = cwd {
        push_git_metadata_paths(&mut paths, cwd);
    }

    dedupe_paths(paths)
}

fn push_env_path(
    paths: &mut Vec<PathBuf>,
    cwd: Option<&Path>,
    env: &[(String, String)],
    key: &str,
) {
    if let Some(path) = config_env_path(cwd, env, key) {
        push_path(paths, path);
    }
}

fn config_env_path(cwd: Option<&Path>, env: &[(String, String)], key: &str) -> Option<PathBuf> {
    let value = config_env_value(env, key)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    Some(if path.is_absolute() {
        path
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path
    })
}

fn push_git_metadata_paths(paths: &mut Vec<PathBuf>, repo_root: &Path) {
    let git_entry = repo_root.join(".git");
    if git_entry.is_dir() {
        push_path(paths, git_entry);
        return;
    }
    if !git_entry.is_file() {
        return;
    }

    let Ok(contents) = std::fs::read_to_string(&git_entry) else {
        return;
    };
    let Some(gitdir) = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let gitdir = resolve_relative_path(repo_root, Path::new(gitdir));
    push_path(paths, gitdir.clone());

    let commondir_path = gitdir.join("commondir");
    let Ok(commondir) = std::fs::read_to_string(&commondir_path) else {
        return;
    };
    let commondir = commondir.trim();
    if !commondir.is_empty() {
        push_path(paths, resolve_relative_path(&gitdir, Path::new(commondir)));
    }
}

fn resolve_relative_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn push_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.as_os_str().is_empty() {
        return;
    }
    paths.push(std::fs::canonicalize(&path).unwrap_or(path));
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            unique.push(path);
        }
    }
    unique
}

fn upsert_grok_brehon_sandbox_profile(
    config: &PtyConfig,
    profile_name: &str,
    read_write_paths: &[PathBuf],
) -> Result<()> {
    let sandbox_path = grok_sandbox_config_path(config)?;
    if let Some(parent) = sandbox_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            Error::pty(format!(
                "Failed to create Grok sandbox config directory '{}': {err}",
                parent.display()
            ))
        })?;
    }

    let existing = match std::fs::read_to_string(&sandbox_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(Error::pty(format!(
                "Failed to read Grok sandbox config '{}': {err}",
                sandbox_path.display()
            )));
        }
    };
    let block = grok_brehon_sandbox_block(profile_name, read_write_paths);
    let updated = upsert_grok_brehon_sandbox_block(&sandbox_path, &existing, profile_name, &block)?;
    if updated != existing {
        std::fs::write(&sandbox_path, updated).map_err(|err| {
            Error::pty(format!(
                "Failed to write Grok sandbox config '{}': {err}",
                sandbox_path.display()
            ))
        })?;
    }
    Ok(())
}

fn grok_sandbox_config_path(config: &PtyConfig) -> Result<PathBuf> {
    if let Some(config_dir) = config_env_value(&config.env, GROK_BREHON_SANDBOX_CONFIG_DIR_ENV)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(config_dir).join("sandbox.toml"));
    }

    let home = std::env::var_os("HOME").ok_or_else(|| {
        Error::pty("HOME is not set; cannot install managed Grok sandbox profile".to_string())
    })?;
    Ok(PathBuf::from(home).join(".grok").join("sandbox.toml"))
}

fn grok_brehon_sandbox_block(profile_name: &str, read_write_paths: &[PathBuf]) -> String {
    let begin_marker = grok_brehon_begin_marker(profile_name);
    let end_marker = grok_brehon_end_marker(profile_name);
    let read_write = read_write_paths
        .iter()
        .map(|path| format!("  \"{}\"", toml_escape(&path.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(",\n");

    format!(
        "{begin_marker}\n[profiles.\"{}\"]\nextends = \"workspace\"\nread_write = [\n{read_write}\n]\n{end_marker}\n",
        toml_escape(profile_name),
    )
}

fn upsert_grok_brehon_sandbox_block(
    sandbox_path: &Path,
    existing: &str,
    profile_name: &str,
    block: &str,
) -> Result<String> {
    let begin_marker = grok_brehon_begin_marker(profile_name);
    let end_marker = grok_brehon_end_marker(profile_name);

    if let Some(start) = existing.find(&begin_marker) {
        let Some(end_offset) = existing[start..].find(&end_marker) else {
            return Err(Error::pty(format!(
                "Failed to update Grok sandbox profile '{profile_name}' in '{}': missing managed end marker",
                sandbox_path.display()
            )));
        };
        let end = start + end_offset + end_marker.len();
        let replace_end = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut updated = String::new();
        updated.push_str(&existing[..start]);
        updated.push_str(block);
        updated.push_str(&existing[replace_end..]);
        return Ok(updated);
    }

    let mut updated = existing.to_string();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(block);
    Ok(updated)
}

fn grok_brehon_begin_marker(profile_name: &str) -> String {
    format!("{GROK_BREHON_SANDBOX_MARKER_PREFIX} {profile_name} BEGIN")
}

fn grok_brehon_end_marker(profile_name: &str) -> String {
    format!("{GROK_BREHON_SANDBOX_MARKER_PREFIX} {profile_name} END")
}

fn toml_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

fn apply_runtime_session_name(env: &mut Vec<(String, String)>, session_name: Option<&str>) {
    let Some(session_name) = session_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        // None/empty is never valid in production — every agent spawn must
        // carry a resolved session name so the spawned MCP subprocess can
        // stamp prompt-queue writes with the active runtime session. A silent
        // no-op here causes prompt entries to be tagged with a mismatched
        // scope and swept before delivery. Log loudly so regressions surface
        // immediately instead of manifesting as undelivered messages.
        tracing::error!(
            "apply_runtime_session_name called with empty/None session_name — \
             spawned agent MCP children cannot stamp prompt entries with the \
             active runtime session. Fix the caller to thread the live session \
             name through."
        );
        return;
    };
    set_config_env_value(env, "BREHON_SESSION_NAME", session_name);
}

#[allow(clippy::too_many_arguments)]
fn build_gateway_metadata_env(
    name: &str,
    role: &str,
    session_name: Option<&str>,
    agent_type: Option<&str>,
    cwd: &std::path::Path,
    brehon_root: Option<&PathBuf>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("BREHON_AGENT_NAME".to_string(), name.to_string()),
        ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
        (
            "BREHON_AGENT_TYPE".to_string(),
            agent_type.unwrap_or(name).to_string(),
        ),
        (
            "BREHON_SESSION_ID".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ),
        (
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
    ];

    if let Some(root) = brehon_root {
        env.push((
            "BREHON_ROOT".to_string(),
            root.to_string_lossy().to_string(),
        ));
        if root.file_name().and_then(|name| name.to_str()) == Some(".brehon")
            && let Some(project_root) = root.parent().filter(|path| !path.as_os_str().is_empty())
        {
            env.push((
                "BREHON_PROJECT_ROOT".to_string(),
                project_root.to_string_lossy().to_string(),
            ));
        }
    }
    env.push((
        "BREHON_WORKSPACE_ROOT".to_string(),
        cwd.to_string_lossy().to_string(),
    ));
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
    apply_runtime_session_name(&mut env, session_name);

    env
}

// Disabled: Gemini exits 0 when receiving a single Ctrl-C at an empty prompt.
// Long-term fix is migrating Gemini to native ACP delivery.
pub(crate) fn uses_pre_submit_interrupt_reset(_cli_type: &AgentAdapter) -> bool {
    false
}

/// Delay between the two Ctrl-C pulses in the double-tap interrupt reset.
pub(crate) const PRE_SUBMIT_INTER_INTERRUPT_DELAY: Duration = Duration::from_millis(80);
/// Settle time after the second Ctrl-C, giving the CLI time to redraw.
pub(crate) const PRE_SUBMIT_SETTLE_DELAY: Duration = Duration::from_millis(150);

impl Pane {
    /// Create a new pane with a specific backend.
    fn new_with_backend(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: PaneKind,
        backend: PaneBackend,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::new_with_backend_cli(
            id,
            title,
            kind,
            backend,
            rows,
            cols,
            AgentAdapter::BuiltIn(SupervisorCli::Claude),
        )
    }

    pub(crate) fn new_with_backend_cli(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: PaneKind,
        backend: PaneBackend,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
    ) -> Result<Self> {
        let id = id.into();
        let terminal = Terminal::new(rows, cols).map_err(|e| Error::terminal(e.to_string()))?;
        terminal.set_default_colors(Rgb { r: 0, g: 0, b: 0 }, Rgb { r: 0, g: 0, b: 0 });
        let info = terminal.scrollback_info();
        let mut pane = Self {
            title: title.into(),
            id,
            kind,
            terminal,
            backend,
            panesmith_managed: false,
            focused: false,
            color: None,
            exited: false,
            exit_code: None,
            rows,
            cols,
            recorder: None,
            force_all_dirty: true,
            render_generation: 0,
            last_total_scrollback: info.total_scrollback,
            seq_counter: 0,
            cli_type,
            configured_agent_type: None,
            last_output_at: Instant::now(),
            is_tool_executing: true,
            pending_messages: VecDeque::new(),
            notify_socket_path: None,
            agent_session_id: None,
            pending_ink_submit: Arc::new(std::sync::Mutex::new(None)),
            ink_submit_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            synthetic_prev_was_cr: false,
            terminal_prompt_prefilter_tail: String::new(),
            supervisor_pending_structured_output: Vec::new(),
            gateway_session_id: None,
            current_generation: crate::pane::Generation(0),
            pty_spawn_config: None,
            gateway_spawn_config: None,
            gateway_terminal_id: None,
            gateway_event_bridge_started: false,
            pending_inbox_nudge: false,
            pending_inbox_nudge_since: None,
            inbox_nudge_not_before: None,
            activity_buffer: None,
            prompt_queue: crate::pane::state::PanePromptQueue::default(),
            pane_state: None,
            blocked_resume_state: None,
            permission_resolution_fallback_until: None,
            task_context: None,
            review_context: None,
            last_prompt_delivery_attempt: None,
            last_successful_mcp_call: None,
            restart_count: 0,
            last_restart_reason: None,
            blocked_dead_unavailable_reason: None,
            last_restart_at: None,
            consecutive_crashes: 0,
        };
        pane.arm_claude_inbox_nudge_grace_period();
        Ok(pane)
    }

    pub(crate) fn set_agent_session_id(&mut self, session_id: Option<String>) {
        self.agent_session_id = session_id;
    }

    pub(crate) fn set_configured_agent_type(&mut self, configured_agent_type: Option<&str>) {
        self.configured_agent_type = configured_agent_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }

    pub(crate) fn set_pty_spawn_config(&mut self, config: PtyConfig) {
        self.pty_spawn_config = Some(config);
    }

    pub(crate) fn is_agy(&self) -> bool {
        match &self.cli_type {
            AgentAdapter::BuiltIn(cli) => *cli == SupervisorCli::Agy,
            AgentAdapter::BuiltInOverride(cfg) => cfg.cli == SupervisorCli::Agy,
            AgentAdapter::Custom(_) => false,
        }
    }

    pub(crate) fn is_opencode_supervisor(&self) -> bool {
        let is_sup = matches!(self.kind, crate::pane::PaneKind::Supervisor);
        let is_opencode = match &self.cli_type {
            AgentAdapter::BuiltIn(cli) => *cli == SupervisorCli::OpenCode,
            AgentAdapter::BuiltInOverride(cfg) => cfg.cli == SupervisorCli::OpenCode,
            AgentAdapter::Custom(_) => false,
        };
        is_sup && is_opencode
    }

    pub(crate) fn is_agy_or_opencode_supervisor(&self) -> bool {
        self.is_agy() || self.is_opencode_supervisor()
    }

    #[allow(clippy::too_many_arguments)]
    fn pty_pane_from_config(
        name: &str,
        kind: PaneKind,
        mut config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        configured_agent_type: Option<&str>,
        adapter_name: &str,
        materialization: AgentPaneMaterialization,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        // Open the PTY at the layout-aware size. Several TUI-style CLIs read
        // TIOCGWINSZ during startup and commit to that geometry before the
        // first resize can be delivered.
        config.rows = rows;
        config.cols = cols;
        let stored_config = config.clone();
        let mut pane = match materialization {
            AgentPaneMaterialization::Spawn => {
                let spawn_config = spawn_config_for_pty_spawn(&config);
                let pty = Pty::spawn(name, spawn_config)?;
                Self::with_pty_cli(name, kind, pty, rows, cols, cli_type)?
            }
            AgentPaneMaterialization::Panesmith | AgentPaneMaterialization::PlanOnly => {
                let mut pane = Self::new_with_backend_cli(
                    name,
                    name,
                    kind,
                    PaneBackend::None,
                    rows,
                    cols,
                    cli_type,
                )?;
                pane.set_tool_executing(false);
                pane
            }
        };
        pane.set_agent_session_id(session_id);
        pane.set_configured_agent_type(configured_agent_type.or(Some(adapter_name)));
        pane.set_pty_spawn_config(stored_config);
        pane.set_notify_socket_path(brehon_root, name);
        Ok(pane)
    }

    /// Create a new pane with a PTY
    pub fn with_pty(
        id: impl Into<String>,
        kind: PaneKind,
        pty: Pty,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::with_pty_cli(
            id,
            kind,
            pty,
            rows,
            cols,
            AgentAdapter::BuiltIn(SupervisorCli::Claude),
        )
    }

    /// Create a new pane with a PTY and explicit agent adapter
    pub fn with_pty_cli(
        id: impl Into<String>,
        kind: PaneKind,
        pty: Pty,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
    ) -> Result<Self> {
        let id_str: String = id.into();
        Self::new_with_backend_cli(
            id_str.clone(),
            id_str,
            kind,
            PaneBackend::Pty(pty),
            rows,
            cols,
            cli_type,
        )
    }

    /// Create a director pane (no PTY)
    pub fn director(id: impl Into<String>, rows: u16, cols: u16) -> Result<Self> {
        let id_str: String = id.into();
        Self::new_with_backend(
            id_str,
            "Director",
            PaneKind::Director,
            PaneBackend::None,
            rows,
            cols,
        )
    }

    /// Create a shell pane running the user's default shell (or a specific command).
    pub fn shell(
        name: &str,
        cwd: PathBuf,
        shell_command: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        Self::shell_materialized(
            name,
            cwd,
            shell_command,
            rows,
            cols,
            AgentPaneMaterialization::Spawn,
        )
    }

    pub(crate) fn shell_materialized(
        name: &str,
        cwd: PathBuf,
        shell_command: Option<&str>,
        rows: u16,
        cols: u16,
        materialization: AgentPaneMaterialization,
    ) -> Result<Self> {
        let shell = shell_command
            .map(|s| s.to_string())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string()));

        let config = PtyConfig {
            command: shell,
            args: vec![],
            cwd: Some(cwd),
            env: vec![],
            rows,
            cols,
        };
        let mut pane = Self::pty_pane_from_config(
            name,
            PaneKind::Shell,
            config,
            rows,
            cols,
            AgentAdapter::BuiltIn(SupervisorCli::Claude),
            None,
            "shell",
            materialization,
            None,
        )?;
        pane.set_tool_executing(false);
        Ok(pane)
    }

    /// Create a gateway-backed pane from a PtyConfig.
    ///
    /// Instead of spawning a PTY, extracts the command/args/env from the config
    /// and stores them for later gateway session spawning. The pane starts with
    /// `PaneBackend::None` and receives output via `append_output()`.
    fn gateway_pane_from_config(
        name: &str,
        kind: PaneKind,
        config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        validate_codex_gateway_bootstrap(&config, brehon_root)?;
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        let cwd = config
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let model = config_env_value(&config.env, "BREHON_AGENT_MODEL")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let mut args = config.args;
        if is_native_agent_command(&config.command)
            && let Some(model) = model.as_deref()
            && !args_contain_option(&args, "--model")
        {
            args.push("--model".to_string());
            args.push(model.to_string());
        }
        let spawn_config = GatewaySpawnConfig {
            command: Some(config.command),
            args,
            env: config.env,
            cwd,
            protocol: gateway_protocol_for(&cli_type),
            tool_prefix: None,
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model,
            sidecar_socket_path: None,
            sidecar_ready_path: None,
            sidecar_connect_timeout_ms: None,
        };
        Self::gateway_pane_from_spawn_config(
            name,
            kind,
            spawn_config,
            session_id,
            rows,
            cols,
            cli_type,
            brehon_root,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn gateway_pane_from_spawn_config(
        name: &str,
        kind: PaneKind,
        spawn_config: GatewaySpawnConfig,
        session_id: Option<String>,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let mut pane =
            Self::new_with_backend_cli(name, name, kind, PaneBackend::None, rows, cols, cli_type)?;
        // Gateway-backed panes start idle. Unlike Claude-Code PTY panes, there is no splash-screen
        // period during which we want to suppress delivery; the ACP session either is not yet
        // spawned (in which case delivery is gated by gateway-session readiness, not by this flag)
        // or is spawned and ready to accept a prompt. Leaving this true relied on every ACP agent
        // emitting OperationStarted/OperationCompleted pairs, which Kimi (and likely others) does
        // not. The flag is re-set to true by ActivityEvent handlers when tools/operations begin.
        pane.set_tool_executing(false);
        pane.set_agent_session_id(session_id);
        pane.set_configured_agent_type(
            config_env_value(&spawn_config.env, "BREHON_AGENT_TYPE").as_deref(),
        );
        pane.set_notify_socket_path(brehon_root, name);
        pane.gateway_spawn_config = Some(spawn_config);
        Ok(pane)
    }

    #[allow(clippy::too_many_arguments)]
    fn acp_sidecar_supervisor_pane_from_config(
        name: &str,
        mut config: PtyConfig,
        rows: u16,
        cols: u16,
        cli_type: AgentAdapter,
        configured_agent_type: Option<&str>,
        adapter_name: &str,
        materialization: AgentPaneMaterialization,
        brehon_root: Option<&PathBuf>,
    ) -> Result<Self> {
        let (socket_path, ready_path) = acp_sidecar_contract_paths(&config, brehon_root, name)?;
        set_config_env_value(&mut config.env, "BREHON_NATIVE_AGENT_SOCKET", &socket_path);
        set_config_env_value(
            &mut config.env,
            "BREHON_NATIVE_AGENT_READY_FILE",
            &ready_path,
        );
        let session_id = config_env_value(&config.env, "BREHON_SESSION_ID");
        let cwd = config
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let spawn_config = GatewaySpawnConfig {
            command: None,
            args: Vec::new(),
            env: config.env.clone(),
            cwd,
            protocol: GatewayProtocol::AcpUnixSocket,
            tool_prefix: None,
            base_url: None,
            api_key_env: None,
            headers: Vec::new(),
            model: None,
            sidecar_socket_path: Some(socket_path),
            sidecar_ready_path: Some(ready_path),
            sidecar_connect_timeout_ms: Some(ACP_SIDECAR_CONNECT_TIMEOUT_MS),
        };
        let mut pane = Self::pty_pane_from_config(
            name,
            PaneKind::Supervisor,
            config,
            rows,
            cols,
            cli_type,
            configured_agent_type,
            adapter_name,
            materialization,
            brehon_root,
        )?;
        pane.set_agent_session_id(session_id);
        pane.gateway_spawn_config = Some(spawn_config);
        Ok(pane)
    }

    /// Create a worker pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn worker(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::worker_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            supervisor_name,
            adapter,
            model,
            server_url,
            rows,
            cols,
            teams,
            reasoning_effort,
            None,
            &[],
            sandbox_profile,
        )
    }

    /// Create a worker pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn worker_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
        configured_agent_type: Option<&str>,
        launcher_env: &[(String, String)],
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::worker_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            supervisor_name,
            adapter,
            model,
            server_url,
            rows,
            cols,
            teams,
            reasoning_effort,
            configured_agent_type,
            launcher_env,
            AgentPaneMaterialization::Spawn,
            sandbox_profile,
        )
    }

    /// Create a worker pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn worker_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        supervisor_name: &str,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        rows: u16,
        cols: u16,
        teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
        configured_agent_type: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        let launch_policy =
            sandbox_profile.map(|sandbox_profile| crate::pty::LaunchPolicy { sandbox_profile });
        let launch_policy = launch_policy.as_ref();
        let worker_cli_str = Some(adapter.name());
        let adapter_owned = adapter.clone();

        if let AgentAdapter::Custom(custom) = adapter {
            match adapter.capabilities().preferred_control_plane {
                crate::harness::HarnessControlPlane::Acp => {
                    let command = custom.command.as_deref().ok_or_else(|| {
                        Error::pty(format!(
                            "Custom ACP agent '{}' is missing a launch command",
                            adapter.name()
                        ))
                    })?;
                    let mut config = if is_custom_codex_app_server(custom) {
                        PtyConfig::custom_codex_acp(
                            name,
                            "worker",
                            cwd,
                            configured_agent_type.or(Some(adapter.name())),
                            brehon_root,
                            launcher_env,
                            Some(supervisor_name),
                            worker_cli_str,
                            model,
                            &custom.args,
                            launch_policy,
                        )
                    } else {
                        PtyConfig::custom_acp(
                            name,
                            "worker",
                            command,
                            &custom.args,
                            configured_agent_type.or(Some(adapter.name())),
                            cwd,
                            brehon_root,
                            Some(supervisor_name),
                            worker_cli_str,
                            launch_policy,
                        )
                    };
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    apply_grok_acp_hardening(&mut config, command)?;
                    return Self::gateway_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::OpenAiCompatible => {
                    let mut env = build_gateway_metadata_env(
                        name,
                        "worker",
                        session_name,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd.as_path(),
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                    );
                    merge_launcher_env(&mut env, launcher_env);
                    apply_runtime_model_metadata(&mut env, model, reasoning_effort);
                    let session_id = config_env_value(&env, "BREHON_SESSION_ID");
                    let spawn_config = GatewaySpawnConfig {
                        command: None,
                        args: Vec::new(),
                        env,
                        cwd: cwd.to_string_lossy().to_string(),
                        protocol: gateway_protocol_for(adapter),
                        tool_prefix: Some(adapter.capabilities().tool_prefix.as_ref().to_string()),
                        base_url: custom.base_url.clone(),
                        api_key_env: custom.api_key_env.clone(),
                        headers: custom.headers.clone(),
                        model: model.map(str::to_string),
                        sidecar_socket_path: None,
                        sidecar_ready_path: None,
                        sidecar_connect_timeout_ms: None,
                    };
                    return Self::gateway_pane_from_spawn_config(
                        name,
                        PaneKind::Worker,
                        spawn_config,
                        session_id,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::NativeHooks
                | crate::harness::HarnessControlPlane::PtyInjection => {
                    if let Some(reason) =
                        custom_non_supervisor_requires_pty_error(adapter, custom, "worker")
                    {
                        return Err(Error::pty(reason));
                    }
                    let command = custom
                        .command
                        .as_deref()
                        .expect("custom worker PTY contract checked command presence");
                    let mut config = PtyConfig::custom_pty(
                        name,
                        "worker",
                        command,
                        &custom.args,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        launch_policy,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                _ => {
                    return Err(Error::pty(format!(
                        "Custom agent '{}' is not yet supported for worker spawn unless it is gateway-backed",
                        adapter.name()
                    )));
                }
            }
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");
        if let Some(reason) = unsupported_builtin_one_shot_override(adapter) {
            return Err(Error::pty(reason));
        }

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    "worker",
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                    reasoning_effort,
                    sandbox_profile,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            // ACP-capable agents: spawned via AgentGateway with piped stdio, not PTY.
            // Build PtyConfig for command/args/env, then create a gateway pane.
            SupervisorCli::Codex => {
                let _ = server_url;
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::codex(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        launcher_env,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::codex_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    launcher_env,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::gemini(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        teams,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::gemini_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::kimi(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::kimi_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::opencode(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                        teams,
                        launch_policy,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let server_port = allocate_loopback_port()?;
                let mut config = PtyConfig::opencode_headless_server(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    server_port,
                    teams,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config = PtyConfig::junie(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config = PtyConfig::agy(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    teams,
                )
                .map_err(Error::pty)?;
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::copilot(
                        name,
                        "worker",
                        cwd,
                        brehon_root,
                        Some(supervisor_name),
                        worker_cli_str,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        PaneKind::Worker,
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::copilot_acp(
                    name,
                    "worker",
                    cwd,
                    brehon_root,
                    Some(supervisor_name),
                    worker_cli_str,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    PaneKind::Worker,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
        }
    }

    /// Create a reviewer pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn reviewer(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::reviewer_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            None,
            None,
            &[],
            sandbox_profile,
        )
    }

    /// Create a reviewer pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn reviewer_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::reviewer_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
            sandbox_profile,
        )
    }

    /// Create an advisor pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn advisor_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::advisor_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
            sandbox_profile,
        )
    }

    /// Create a reviewer pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reviewer_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::role_agent_with_agent_type_materialized(
            "reviewer",
            PaneKind::Reviewer,
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            materialization,
            sandbox_profile,
        )
    }

    /// Create an advisor pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advisor_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::role_agent_with_agent_type_materialized(
            "advisor",
            PaneKind::Advisor,
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            materialization,
            sandbox_profile,
        )
    }

    /// Create a research pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn research_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::role_agent_with_agent_type_materialized(
            "research",
            PaneKind::Research,
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            model,
            server_url,
            teams,
            configured_agent_type,
            reasoning_effort,
            launcher_env,
            materialization,
            sandbox_profile,
        )
    }

    /// Create a non-worker role agent pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    fn role_agent_with_agent_type_materialized(
        role: &str,
        pane_kind: PaneKind,
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        configured_agent_type: Option<&str>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        let launch_policy =
            sandbox_profile.map(|sandbox_profile| crate::pty::LaunchPolicy { sandbox_profile });
        let launch_policy = launch_policy.as_ref();
        let adapter_owned = adapter.clone();

        if let AgentAdapter::Custom(custom) = adapter {
            match adapter.capabilities().preferred_control_plane {
                crate::harness::HarnessControlPlane::Acp => {
                    let command = custom.command.as_deref().ok_or_else(|| {
                        Error::pty(format!(
                            "Custom ACP agent '{}' is missing a launch command",
                            adapter.name()
                        ))
                    })?;
                    let mut config = if is_custom_codex_app_server(custom) {
                        PtyConfig::custom_codex_acp(
                            name,
                            role,
                            cwd,
                            configured_agent_type.or(Some(adapter.name())),
                            brehon_root,
                            launcher_env,
                            None,
                            None,
                            model,
                            &custom.args,
                            launch_policy,
                        )
                    } else {
                        PtyConfig::custom_acp(
                            name,
                            role,
                            command,
                            &custom.args,
                            configured_agent_type.or(Some(adapter.name())),
                            cwd,
                            brehon_root,
                            None,
                            None,
                            launch_policy,
                        )
                    };
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    apply_grok_acp_hardening(&mut config, command)?;
                    return Self::gateway_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::OpenAiCompatible => {
                    let mut env = build_gateway_metadata_env(
                        name,
                        role,
                        session_name,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd.as_path(),
                        brehon_root,
                        None,
                        None,
                    );
                    merge_launcher_env(&mut env, launcher_env);
                    apply_runtime_model_metadata(&mut env, model, reasoning_effort);
                    let session_id = config_env_value(&env, "BREHON_SESSION_ID");
                    let spawn_config = GatewaySpawnConfig {
                        command: None,
                        args: Vec::new(),
                        env,
                        cwd: cwd.to_string_lossy().to_string(),
                        protocol: gateway_protocol_for(adapter),
                        tool_prefix: Some(adapter.capabilities().tool_prefix.as_ref().to_string()),
                        base_url: custom.base_url.clone(),
                        api_key_env: custom.api_key_env.clone(),
                        headers: custom.headers.clone(),
                        model: model.map(str::to_string),
                        sidecar_socket_path: None,
                        sidecar_ready_path: None,
                        sidecar_connect_timeout_ms: None,
                    };
                    return Self::gateway_pane_from_spawn_config(
                        name,
                        pane_kind.clone(),
                        spawn_config,
                        session_id,
                        rows,
                        cols,
                        adapter_owned,
                        brehon_root,
                    );
                }
                crate::harness::HarnessControlPlane::NativeHooks
                | crate::harness::HarnessControlPlane::PtyInjection => {
                    if let Some(reason) =
                        custom_non_supervisor_requires_pty_error(adapter, custom, role)
                    {
                        return Err(Error::pty(reason));
                    }
                    let command = custom
                        .command
                        .as_deref()
                        .expect("custom role PTY contract checked command presence");
                    let mut config = PtyConfig::custom_pty(
                        name,
                        role,
                        command,
                        &custom.args,
                        configured_agent_type.or(Some(adapter.name())),
                        cwd,
                        brehon_root,
                        None,
                        None,
                        launch_policy,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                _ => {
                    return Err(Error::pty(format!(
                        "Custom agent '{}' is not yet supported for {role} spawn unless it is gateway-backed",
                        adapter.name()
                    )));
                }
            }
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");
        if let Some(reason) = unsupported_builtin_one_shot_override(adapter) {
            return Err(Error::pty(reason));
        }

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    role,
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    teams,
                    reasoning_effort,
                    sandbox_profile,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            // ACP-capable role agents: gateway panes (piped stdio, no PTY)
            SupervisorCli::Codex => {
                let _ = server_url;
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::codex(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        launcher_env,
                        None,
                        None,
                        model,
                        reasoning_effort,
                        teams,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::codex_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    launcher_env,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::gemini(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        teams,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::gemini_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::kimi(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::kimi_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::opencode(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                        teams,
                        launch_policy,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let server_port = allocate_loopback_port()?;
                let mut config = PtyConfig::opencode_headless_server(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    server_port,
                    teams,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config =
                    PtyConfig::junie(name, role, cwd, brehon_root, None, None, model, teams);
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config =
                    PtyConfig::agy(name, role, cwd, brehon_root, None, None, model, teams)
                        .map_err(Error::pty)?;
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::pty_pane_from_config(
                    name,
                    pane_kind.clone(),
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                if built_in_uses_pty_launch_contract(adapter, materialization) {
                    let mut config = PtyConfig::copilot(
                        name,
                        role,
                        cwd,
                        brehon_root,
                        None,
                        None,
                        model,
                        reasoning_effort,
                    );
                    apply_runtime_session_name(&mut config.env, session_name);
                    apply_configured_agent_type(&mut config, configured_agent_type);
                    merge_launcher_env(&mut config.env, launcher_env);
                    apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                    return Self::pty_pane_from_config(
                        name,
                        pane_kind.clone(),
                        config,
                        rows,
                        cols,
                        adapter_owned,
                        configured_agent_type,
                        adapter.name(),
                        materialization,
                        brehon_root,
                    );
                }
                let mut config = PtyConfig::copilot_acp(
                    name,
                    role,
                    cwd,
                    brehon_root,
                    None,
                    None,
                    model,
                    reasoning_effort,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::gateway_pane_from_config(
                    name,
                    pane_kind,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    brehon_root,
                )
            }
        }
    }

    /// Create a supervisor pane using the default agent type.
    #[allow(clippy::too_many_arguments)]
    pub fn supervisor(
        name: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::supervisor_with_agent_type(
            name,
            cwd,
            None,
            brehon_root,
            rows,
            cols,
            adapter,
            worker_adapter,
            worker_names,
            model,
            server_url,
            teams,
            worker_cli_map,
            None,
            &HashMap::new(),
            None,
            &[],
            sandbox_profile,
        )
    }

    /// Create a supervisor pane with an explicit configured agent type override.
    #[allow(clippy::too_many_arguments)]
    pub fn supervisor_with_agent_type(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
        configured_agent_type: Option<&str>,
        worker_agent_type_map: &HashMap<String, String>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        Self::supervisor_with_agent_type_materialized(
            name,
            cwd,
            session_name,
            brehon_root,
            rows,
            cols,
            adapter,
            worker_adapter,
            worker_names,
            model,
            server_url,
            teams,
            worker_cli_map,
            configured_agent_type,
            worker_agent_type_map,
            reasoning_effort,
            launcher_env,
            AgentPaneMaterialization::Spawn,
            sandbox_profile,
        )
    }

    /// Create a supervisor pane with explicit materialization behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn supervisor_with_agent_type_materialized(
        name: &str,
        cwd: PathBuf,
        session_name: Option<&str>,
        brehon_root: Option<&PathBuf>,
        rows: u16,
        cols: u16,
        adapter: &AgentAdapter,
        worker_adapter: &AgentAdapter,
        worker_names: &[String],
        model: Option<&str>,
        server_url: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
        worker_cli_map: &HashMap<String, AgentAdapter>,
        configured_agent_type: Option<&str>,
        worker_agent_type_map: &HashMap<String, String>,
        reasoning_effort: Option<&str>,
        launcher_env: &[(String, String)],
        materialization: AgentPaneMaterialization,
        sandbox_profile: Option<SandboxProfile>,
    ) -> Result<Self> {
        let launch_policy =
            sandbox_profile.map(|sandbox_profile| crate::pty::LaunchPolicy { sandbox_profile });
        let launch_policy = launch_policy.as_ref();
        let worker_cli_str = worker_adapter.name();
        let worker_names_csv = if worker_names.is_empty() {
            None
        } else {
            Some(worker_names.join(","))
        };

        // Build BREHON_FACTORY_WORKER_POOL JSON: {"codex-1":"codex","gemini-1":"gemini"}
        let worker_pool_json = if !worker_cli_map.is_empty() {
            let map: std::collections::HashMap<&str, &str> = worker_cli_map
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str(),
                        worker_agent_type_map
                            .get(k)
                            .map(String::as_str)
                            .unwrap_or_else(|| v.name()),
                    )
                })
                .collect();
            serde_json::to_string(&map).ok()
        } else {
            None
        };

        let adapter_owned = adapter.clone();
        if let AgentAdapter::Custom(custom) = adapter {
            if let Some(reason) = custom_supervisor_requires_pty_error(adapter, custom) {
                return Err(Error::pty(reason));
            }
            let command = custom
                .command
                .as_deref()
                .expect("custom supervisor PTY contract checked command presence");
            let mut config = PtyConfig::custom_pty(
                name,
                "supervisor",
                command,
                &custom.args,
                configured_agent_type.or(Some(adapter.name())),
                cwd,
                brehon_root,
                None,
                Some(worker_cli_str),
                launch_policy,
            );
            apply_runtime_session_name(&mut config.env, session_name);
            apply_configured_agent_type(&mut config, configured_agent_type);
            merge_launcher_env(&mut config.env, launcher_env);
            apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
            Self::push_supervisor_env(
                &mut config.env,
                adapter,
                &worker_names_csv,
                &worker_pool_json,
            );
            if adapter.capabilities().preferred_control_plane
                == crate::harness::HarnessControlPlane::AcpSidecar
            {
                return Self::acp_sidecar_supervisor_pane_from_config(
                    name,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                );
            }
            return Self::pty_pane_from_config(
                name,
                PaneKind::Supervisor,
                config,
                rows,
                cols,
                adapter_owned,
                configured_agent_type,
                adapter.name(),
                materialization,
                brehon_root,
            );
        }

        let builtin = adapter
            .as_builtin()
            .expect("custom adapters returned earlier");
        if let Some(reason) = unsupported_builtin_one_shot_override(adapter) {
            return Err(Error::pty(reason));
        }

        match builtin {
            SupervisorCli::Claude => {
                let mut config = PtyConfig::claude(
                    name,
                    "supervisor",
                    configured_agent_type.or(Some(adapter.name())),
                    session_name,
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                    reasoning_effort,
                    sandbox_profile,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Codex => {
                let _ = server_url;
                let mut config = PtyConfig::codex(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    launcher_env,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Gemini => {
                let _ = server_url;
                let mut config = PtyConfig::gemini(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Kimi => {
                let mut config = PtyConfig::kimi(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::OpenCode => {
                let _ = server_url;
                let mut config = PtyConfig::opencode(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                    teams,
                    launch_policy,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Junie => {
                let mut config = PtyConfig::junie(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Agy => {
                let mut config = PtyConfig::agy(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    teams,
                )
                .map_err(Error::pty)?;
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
            SupervisorCli::Copilot => {
                let mut config = PtyConfig::copilot(
                    name,
                    "supervisor",
                    cwd,
                    brehon_root,
                    None,
                    Some(worker_cli_str),
                    model,
                    reasoning_effort,
                );
                apply_runtime_session_name(&mut config.env, session_name);
                apply_configured_agent_type(&mut config, configured_agent_type);
                merge_launcher_env(&mut config.env, launcher_env);
                apply_runtime_model_metadata(&mut config.env, model, reasoning_effort);
                Self::push_supervisor_env(
                    &mut config.env,
                    adapter,
                    &worker_names_csv,
                    &worker_pool_json,
                );
                Self::pty_pane_from_config(
                    name,
                    PaneKind::Supervisor,
                    config,
                    rows,
                    cols,
                    adapter_owned,
                    configured_agent_type,
                    adapter.name(),
                    materialization,
                    brehon_root,
                )
            }
        }
    }

    fn push_supervisor_env(
        env: &mut Vec<(String, String)>,
        adapter: &AgentAdapter,
        worker_names_csv: &Option<String>,
        worker_pool_json: &Option<String>,
    ) {
        env.push((
            "BREHON_FACTORY_SUPERVISOR_CLI".to_string(),
            adapter.name().to_string(),
        ));
        if let Some(csv) = worker_names_csv {
            env.push(("BREHON_FACTORY_WORKER_NAMES".to_string(), csv.clone()));
        }
        if let Some(pool_json) = worker_pool_json {
            env.push(("BREHON_FACTORY_WORKER_POOL".to_string(), pool_json.clone()));
        }
    }

    pub(crate) fn set_notify_socket_path(
        &mut self,
        brehon_root: Option<&PathBuf>,
        agent_name: &str,
    ) {
        self.notify_socket_path =
            brehon_root.map(|root| root.join(format!("notify-{agent_name}.sock")));
    }

    #[cfg(test)]
    pub(crate) fn set_notify_socket_path_for_test(&mut self, path: PathBuf) {
        self.notify_socket_path = Some(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_runtime_model_metadata, build_gateway_metadata_env, gateway_protocol_for,
        merge_launcher_env,
    };
    use crate::harness::{AgentAdapter, HarnessControlPlane, HarnessTransport, SupervisorCli};
    use brehon_acp::GatewayProtocol;
    use std::path::{Path, PathBuf};

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find_map(|(env_key, value)| (env_key == key).then_some(value.as_str()))
    }

    #[test]
    fn merge_launcher_env_keeps_brehon_contract_keys() {
        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), "worker-1".to_string()),
            (
                "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                "true".to_string(),
            ),
        ];

        merge_launcher_env(
            &mut env,
            &[
                ("BREHON_AGENT_NAME".to_string(), "override".to_string()),
                (
                    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS".to_string(),
                    "false".to_string(),
                ),
                (
                    "BREHON_ROLE_SYSTEM_PROMPT".to_string(),
                    "Review for correctness.".to_string(),
                ),
                (
                    "BREHON_WORKTREE_ROOT".to_string(),
                    "/tmp/brehon-worktrees".to_string(),
                ),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://localhost:11434".to_string(),
                ),
            ],
        );

        assert!(
            env.iter()
                .any(|(key, value)| key == "BREHON_AGENT_NAME" && value == "worker-1")
        );
        assert!(env.iter().any(|(key, value)| {
            key == "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS" && value == "true"
        }));
        assert!(env.iter().any(|(key, value)| {
            key == "BREHON_ROLE_SYSTEM_PROMPT" && value == "Review for correctness."
        }));
        assert!(env.iter().any(|(key, value)| {
            key == "BREHON_WORKTREE_ROOT" && value == "/tmp/brehon-worktrees"
        }));
        assert!(env.iter().any(|(key, value)| {
            key == "ANTHROPIC_BASE_URL" && value == "http://localhost:11434"
        }));
    }

    #[test]
    fn apply_runtime_model_metadata_sets_model_and_reasoning() {
        let mut env = Vec::new();

        apply_runtime_model_metadata(&mut env, Some("gpt-5.4"), Some("xhigh"));

        assert!(
            env.iter()
                .any(|(key, value)| key == "BREHON_AGENT_MODEL" && value == "gpt-5.4")
        );
        assert!(
            env.iter()
                .any(|(key, value)| { key == "BREHON_REASONING_EFFORT" && value == "xhigh" })
        );
    }

    #[test]
    fn gateway_metadata_env_keeps_project_and_workspace_roots_distinct() {
        let env = build_gateway_metadata_env(
            "worker-1",
            "worker",
            Some("brehon-1"),
            Some("native-agent"),
            Path::new("/repo/.brehon/worktrees/runs/brehon-1/worker-1"),
            Some(&PathBuf::from("/repo/.brehon")),
            Some("supervisor"),
            Some("native-agent"),
        );

        assert_eq!(env_value(&env, "BREHON_PROJECT_ROOT"), Some("/repo"));
        assert_eq!(
            env_value(&env, "BREHON_WORKSPACE_ROOT"),
            Some("/repo/.brehon/worktrees/runs/brehon-1/worker-1")
        );
    }

    #[test]
    fn gateway_protocol_for_keeps_builtin_gateway_mapping() {
        assert_eq!(
            gateway_protocol_for(&AgentAdapter::BuiltIn(SupervisorCli::OpenCode)),
            GatewayProtocol::OpenCodeServer
        );
    }

    #[test]
    #[should_panic(expected = "gateway_protocol_for called for built-in")]
    fn gateway_protocol_for_panics_for_builtin_pty_override() {
        let mut capabilities = SupervisorCli::Gemini.capabilities();
        capabilities.transport = HarnessTransport::InteractivePty;
        capabilities.preferred_control_plane = HarnessControlPlane::PtyInjection;
        let adapter = AgentAdapter::built_in_with_capabilities(SupervisorCli::Gemini, capabilities);

        let _ = gateway_protocol_for(&adapter);
    }
}
