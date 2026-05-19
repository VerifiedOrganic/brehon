use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use brehon_ports::GitOperations;
use brehon_types::{is_terminal_task_status, normalize_task_status, BrehonConfig};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const BREHON_PROTECTED_BRANCH_GUARD_BEGIN: &str = "# BEGIN BREHON PROTECTED BRANCH GUARD";
const BREHON_PROTECTED_BRANCH_GUARD_END: &str = "# END BREHON PROTECTED BRANCH GUARD";
const BREHON_PROTECTED_BRANCH_GUARD_MARKER: &str = "protected-branch-guard-active";
const BREHON_PROTECTED_BRANCH_HOOKS: &[&str] = &[
    "pre-commit",
    "pre-merge-commit",
    "commit-msg",
    "reference-transaction",
];

/// Ensure `.mcp.json` exists at the project root so agents can discover the
/// Brehon MCP server.  Also update `.claude/settings.local.json` to allow
/// `mcp__brehon__*` tool calls for Claude Code agents.
///
/// Both files are machine-local (absolute brehon binary path in the
/// former, per-developer permissions in the latter). The helper also
/// calls [`ensure_brehon_ignored_in_repo`] so the generated files are
/// immediately added to `.git/info/exclude` on the first run — no
/// teammate ever sees them as uncommitted work they need to reason
/// about.
pub(crate) fn ensure_mcp_config(cwd: &Path) -> Result<()> {
    let brehon_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string());

    // ── .mcp.json ────────────────────────────────────────────────────────
    let mcp_path = cwd.join(".mcp.json");
    let brehon_server = serde_json::json!({
        "command": brehon_exe,
        "args": ["serve"]
    });

    if mcp_path.exists() {
        let content = std::fs::read_to_string(&mcp_path).unwrap_or_default();
        let mut doc: serde_json::Value =
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
        let servers = doc
            .as_object_mut()
            .unwrap()
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(obj) = servers.as_object_mut() {
            if !obj.contains_key("brehon") {
                obj.insert("brehon".to_string(), brehon_server);
                std::fs::write(&mcp_path, serde_json::to_string_pretty(&doc)?)?;
                tracing::info!("Updated .mcp.json with brehon server");
            }
        }
    } else {
        let doc = serde_json::json!({
            "mcpServers": {
                "brehon": brehon_server
            }
        });
        std::fs::write(&mcp_path, serde_json::to_string_pretty(&doc)?)?;
        tracing::info!("Created .mcp.json with brehon server");
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
            "Failed to update .git/info/exclude; .mcp.json and .claude/settings.local.json may show up as uncommitted"
        );
    }

    Ok(())
}

/// Copy machine-local agent bootstrap files into isolated worktrees.
///
/// These files are deliberately excluded from git because they contain
/// developer-local paths and permissions, but agents launched from a worktree
/// still need them for MCP discovery and tool authorization.
fn sync_local_agent_scaffolding_to_worktree(
    project_root: &Path,
    worktree_path: &Path,
) -> Result<()> {
    sync_project_local_file_to_worktree(
        project_root,
        worktree_path,
        Path::new(".mcp.json"),
        "MCP discovery config",
    )?;
    sync_project_local_file_to_worktree(
        project_root,
        worktree_path,
        Path::new(".claude/settings.local.json"),
        "Claude local settings",
    )?;
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

    for (name, body) in [
        ("codex-worker-instructions.md", worker_body),
        ("codex-reviewer-instructions.md", reviewer_body),
        ("codex-supervisor-instructions.md", supervisor_body),
        ("codex-advisor-instructions.md", advisor_body),
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

pub(crate) fn detect_builtin_cli(
    agent_name: &str,
    config: &BrehonConfig,
) -> Option<brehon_mux::SupervisorCli> {
    use brehon_mux::SupervisorCli;

    match agent_name {
        "claude-code" | "claude" => return Some(SupervisorCli::Claude),
        "codex" => return Some(SupervisorCli::Codex),
        "gemini" => return Some(SupervisorCli::Gemini),
        "kimi" => return Some(SupervisorCli::Kimi),
        "opencode" => return Some(SupervisorCli::OpenCode),
        "junie" => return Some(SupervisorCli::Junie),
        "copilot" => return Some(SupervisorCli::Copilot),
        _ => {}
    }

    let agent_config = config.lane_launcher(agent_name)?;
    if agent_config.adapter != brehon_types::agent::AdapterKind::Acp {
        return None;
    }
    let args = agent_config
        .args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    match (
        agent_config.command_str().unwrap_or_default(),
        args.as_slice(),
    ) {
        ("claude", []) => Some(SupervisorCli::Claude),
        ("codex", args) if args == vec!["app-server"] => Some(SupervisorCli::Codex),
        ("gemini", ["--acp"]) | ("gemini", ["--experimental-acp"]) => Some(SupervisorCli::Gemini),
        ("kimi", args) if args == vec!["acp"] => Some(SupervisorCli::Kimi),
        ("opencode", [])
        | ("opencode", ["acp"])
        | ("opencode", ["acp", "--cwd", "."])
        | ("opencode", ["serve"])
        | ("opencode", ["serve", "--pure"]) => Some(SupervisorCli::OpenCode),
        ("junie", []) => Some(SupervisorCli::Junie),
        ("copilot", args) if args.is_empty() || args.contains(&"--acp") => {
            Some(SupervisorCli::Copilot)
        }
        _ => None,
    }
}

fn launches_codex_app_server(command: &str, args: &[String]) -> bool {
    command == "codex" && args.iter().any(|arg| arg == "app-server")
}

fn native_agent_command(configured: Option<&str>) -> String {
    if let Some(command) = configured
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        return command.to_string();
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

pub(crate) fn agent_to_adapter(name: &str, config: &BrehonConfig) -> brehon_mux::AgentAdapter {
    use brehon_mux::{
        AgentAdapter, CustomAgentConfig, HarnessCapabilities, HarnessControlPlane,
        HarnessTransport, SupervisorCli,
    };
    use brehon_types::agent::AdapterKind;

    if let Some(cli) = detect_builtin_cli(name, config) {
        return AgentAdapter::BuiltIn(cli);
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
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
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
                tool_prefix: std::borrow::Cow::Borrowed("mcp_brehon_"),
                transport: HarnessTransport::AppServer,
                preferred_control_plane: HarnessControlPlane::Acp,
            },
            AdapterKind::PtyHooks => SupervisorCli::Claude.capabilities(),
        };
        if let Some(transport) = agent_config
            .transport_str()
            .and_then(|value| value.parse::<HarnessTransport>().ok())
        {
            capabilities.transport = transport;
        }
        if let Some(control_plane) = agent_config
            .control_plane_str()
            .and_then(|value| value.parse::<HarnessControlPlane>().ok())
        {
            capabilities.preferred_control_plane = control_plane;
        }
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
    brehon_root: &Path,
    task_id: &str,
) -> PathBuf {
    brehon_root
        .join("worktrees")
        .join("initiative")
        .join(task_id)
}

/// Gitignore patterns that cover every file `brehon run` generates in the
/// shared repo root. All of these are machine-local — committing them
/// poisons teammate checkouts:
///
/// * `.brehon/` — entire runtime/state tree (sessions, worktrees, tasks,
///   factory caches). Per-developer; never portable.
/// * `.mcp.json` — Claude Code MCP discovery file. Written with an
///   absolute path to the current machine's brehon binary (see
///   [`ensure_mcp_config`]); the path won't resolve on any other host.
/// * `.claude/settings.local.json` — Claude Code per-developer
///   permissions file. The `.local.json` suffix already signals
///   machine-local by Claude Code convention, but it's worth making
///   explicit.
///
/// Written to `.git/info/exclude` (the local-only ignore list) rather
/// than the committed `.gitignore` so the rule follows each clone
/// without requiring a team-wide .gitignore update. This is the same
/// pattern most tooling uses for auto-generated dev scaffolding.
const BREHON_LOCAL_GITIGNORE_PATTERNS: &[&str] =
    &[".brehon/", ".mcp.json", ".claude/settings.local.json"];

/// Ensure all Brehon-generated machine-local files are git-ignored
/// via `.git/info/exclude`.
///
/// No-ops silently when the target directory is not a git repository —
/// we'd rather skip than pollute a non-git dir with a spurious
/// `.git/info/exclude` file.
pub(crate) fn ensure_brehon_ignored_in_repo(repo_root: &Path) -> Result<()> {
    let git_dir = repo_root.join(".git");
    if !git_dir.exists() {
        // `brehon run` can be invoked from a non-git directory; that's
        // legal and shouldn't trigger filesystem writes just to set up
        // a gitignore rule that has no home.
        return Ok(());
    }

    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();

    // Collect the trimmed set of lines already present so we only write
    // patterns that are genuinely missing — and preserve anything else
    // the developer has put in their exclude file (custom tool caches,
    // editor scratch files, etc.).
    let already_present: std::collections::HashSet<&str> =
        existing.lines().map(|line| line.trim()).collect();

    let missing: Vec<&&str> = BREHON_LOCAL_GITIGNORE_PATTERNS
        .iter()
        .filter(|pattern| !already_present.contains(**pattern))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.contains("# Brehon local scaffolding") {
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

        std::fs::write(&path, serde_json::to_string_pretty(&task)?)?;
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
    let hook_command = format!("{} {}", brehon_bin.display(), "claude-hook");

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
    pretooluse.retain(|entry| {
        !entry_contains_brehon_marker(entry)
    });

    pretooluse.push(serde_json::json!({
        "matcher": "Bash",
        "hooks": [
            { "type": "command", "command": hook_command }
        ]
    }));

    std::fs::write(&settings_path, serde_json::to_string_pretty(&doc)?).with_context(|| {
        format!(
            "Failed to write Claude settings at '{}'",
            settings_path.display()
        )
    })?;
    tracing::info!(
        path = %settings_path.display(),
        "Installed Brehon Claude PreToolUse hook"
    );
    Ok(())
}

fn entry_contains_brehon_marker(entry: &serde_json::Value) -> bool {
    let inner = entry.get("hooks").and_then(|h| h.as_array());
    if let Some(arr) = inner {
        for h in arr {
            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                if cmd.contains(CLAUDE_HOOK_MARKER) {
                    return true;
                }
            }
        }
    }
    false
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
    if hook_name == "reference-transaction" {
        return format!(
            r#"{BREHON_PROTECTED_BRANCH_GUARD_BEGIN}
{active_guard}
brehon_ref_txn_input="$(mktemp "${{TMPDIR:-/tmp}}/brehon-ref-transaction.XXXXXX")" || exit 1
cat > "$brehon_ref_txn_input"
trap 'rm -f "$brehon_ref_txn_input"' EXIT HUP INT TERM
if [ "${{BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}}" != "1" ] && [ "$brehon_protected_branch_guard_active" = "1" ] && [ "${{1:-}}" = "prepared" ]; then
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
                        echo "Use Brehon's task integration/close flow. Only deliberate repair commands should set BREHON_ALLOW_PROTECTED_BRANCH_COMMIT=1." >&2
                        exit 1
                    fi
                done
                ;;
        esac
    done < "$brehon_ref_txn_input"
fi
unset brehon_old_ref brehon_new_ref brehon_ref_name brehon_ref_branch brehon_protected_branches brehon_protected_branch
unset brehon_protected_branch_guard_active brehon_git_common_dir brehon_git_root brehon_guard_marker brehon_guard_pid brehon_guard_line
exec < "$brehon_ref_txn_input"
{BREHON_PROTECTED_BRANCH_GUARD_END}"#
        );
    }

    format!(
        r#"{BREHON_PROTECTED_BRANCH_GUARD_BEGIN}
{active_guard}
if [ "${{BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}}" != "1" ] && [ "$brehon_protected_branch_guard_active" = "1" ]; then
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
                echo "Use Brehon's task integration/close flow. Only deliberate repair commands should set BREHON_ALLOW_PROTECTED_BRANCH_COMMIT=1." >&2
                exit 1
            fi
        done
    fi
fi
unset brehon_current_branch brehon_protected_branches brehon_protected_branch
unset brehon_protected_branch_guard_active brehon_git_common_dir brehon_git_root brehon_guard_marker brehon_guard_pid brehon_guard_line
{BREHON_PROTECTED_BRANCH_GUARD_END}"#
    )
}

fn protected_branch_guard_active_shell() -> &'static str {
    r#"brehon_protected_branch_guard_active=0
if [ "${BREHON_ALLOW_PROTECTED_BRANCH_COMMIT:-}" != "1" ]; then
    brehon_git_common_dir="$(git rev-parse --git-common-dir 2>/dev/null || true)"
    if [ -n "$brehon_git_common_dir" ]; then
        case "$brehon_git_common_dir" in
            /*) ;;
            *)
                brehon_git_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
                brehon_git_common_dir="$brehon_git_root/$brehon_git_common_dir"
                ;;
        esac
        brehon_guard_marker="$brehon_git_common_dir/brehon/protected-branch-guard-active"
        if [ -f "$brehon_guard_marker" ]; then
            brehon_guard_pid=""
            while IFS= read -r brehon_guard_line; do
                case "$brehon_guard_line" in
                    pid=*)
                        brehon_guard_pid="${brehon_guard_line#pid=}"
                        break
                        ;;
                esac
            done < "$brehon_guard_marker" || true
            case "$brehon_guard_pid" in
                ""|*[!0-9]*) ;;
                *)
                    if kill -0 "$brehon_guard_pid" 2>/dev/null; then
                        brehon_protected_branch_guard_active=1
                    fi
                    ;;
            esac
        fi
    fi
fi"#
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
            .unwrap_or_else(|| default_initiative_integration_worktree(brehon_root, &task_id));

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
            std::fs::write(&path, serde_json::to_string_pretty(&task)?)?;
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

    let worktrees_dir = cwd.join(".brehon").join("worktrees");
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
        sync_local_agent_scaffolding_to_worktree(cwd, &worktree_path)?;
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

    fn default_security() -> brehon_types::config::SecurityConfig {
        brehon_config::parse_defaults().unwrap().security
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-b", "main"]);
        run_git(path, &["config", "user.email", "brehon@example.com"]);
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
                assistant_message_passthrough_fields: Vec::new(),
                reasoning_effort_param: None,
                extra_body: None,
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
            },
        );

        let adapter = agent_to_adapter("kimi-worker", &config);
        assert_eq!(adapter.as_builtin(), Some(brehon_mux::SupervisorCli::Kimi));
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: Some(4),
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
            max_parallel_tool_calls: None,
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
            max_parallel_tool_calls: None,
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
            max_parallel_tool_calls: None,
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
                max_parallel_tool_calls: None,
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
            .any(|window| window == ["--permission-mode", "bypass"]));
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
            max_parallel_tool_calls: None,
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

        let repaired = reconcile_initiative_hierarchy_for_run(temp.path(), &brehon_root)
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
        assert!(branch.starts_with("initiative/"));
        assert!(worktree.contains(".brehon/worktrees/initiative/T-init"));
        assert!(Path::new(worktree).exists());
        assert_eq!(
            run_git(Path::new(worktree), &["branch", "--show-current"]),
            branch
        );
        assert_eq!(
            run_git(
                temp.path(),
                &["rev-parse", "--verify", &format!("refs/heads/{branch}")]
            ),
            run_git(Path::new(worktree), &["rev-parse", "HEAD"])
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

        let worker_names = vec!["worker-1".to_string(), "worker-2".to_string()];
        let worker_cwds =
            prepare_scoped_worktrees(temp.path(), &config, Some("session-a"), None, &worker_names)
                .await
                .unwrap();

        assert_eq!(worker_cwds.len(), 2);
        for worker_name in &worker_names {
            let worktree_path = worker_cwds.get(worker_name).unwrap();
            assert!(worktree_path.exists());
            assert!(worktree_path.starts_with(temp.path().join(".brehon/worktrees/runs/session-a")));
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
        std::fs::create_dir_all(temp.path().join(".claude")).unwrap();
        std::fs::write(
            temp.path().join(".claude/settings.local.json"),
            r#"{"permissions":{"allow":["mcp__brehon__*"]}}"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#,
        )
        .unwrap();
        run_git(temp.path(), &["add", ".mcp.json"]);
        run_git(temp.path(), &["commit", "-m", "add mcp config"]);

        let mut config = brehon_config::parse_defaults().unwrap();
        config.orchestration.worktree_isolation = true;
        config.orchestration.auto_cleanup_worktrees = true;
        config.orchestration.branch_prefix = "brehon/".to_string();

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
        assert!(supervisor_path.ends_with("runs/session-role/supervisor/claude-code"));
        assert_eq!(
            run_git(supervisor_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "brehon/runs/session-role/supervisor/claude-code"
        );

        let reviewer_path = reviewer_cwds.get("reviewer-a").unwrap();
        assert!(reviewer_path.exists());
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
        assert_eq!(
            std::fs::read_to_string(supervisor_path.join(".mcp.json")).unwrap(),
            r#"{"mcpServers":{"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#
        );
        assert_eq!(
            std::fs::read_to_string(reviewer_path.join(".mcp.json")).unwrap(),
            r#"{"mcpServers":{"brehon":{"command":"/tmp/brehon","args":["serve"]}}}"#
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

        cleanup_scoped_worktrees(temp.path(), &session_b).await;
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
    fn test_protected_branch_hooks_allow_default_branch_without_active_run() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        ensure_protected_branch_hooks(temp.path(), "main").unwrap();

        std::fs::write(temp.path().join("allowed.txt"), "allowed\n").unwrap();
        run_git(temp.path(), &["add", "allowed.txt"]);
        let output = std::process::Command::new("git")
            .args(["commit", "-m", "allowed on inactive main"])
            .current_dir(temp.path())
            .env_remove("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT")
            .env_remove("BREHON_PROTECTED_BRANCHES")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "commit on inactive main should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_ensure_protected_branch_hooks_blocks_default_branch_commit() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let initial_main = run_git(temp.path(), &["rev-parse", "main"]);
        ensure_protected_branch_hooks(temp.path(), "main").unwrap();
        let _activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();

        std::fs::write(temp.path().join("blocked.txt"), "blocked\n").unwrap();
        run_git(temp.path(), &["add", "blocked.txt"]);
        let blocked = std::process::Command::new("git")
            .args(["commit", "-m", "blocked on main"])
            .current_dir(temp.path())
            .env_remove("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT")
            .env_remove("BREHON_PROTECTED_BRANCHES")
            .output()
            .unwrap();
        assert!(
            !blocked.status.success(),
            "commit on main unexpectedly succeeded: {}",
            String::from_utf8_lossy(&blocked.stdout)
        );
        let stderr = String::from_utf8_lossy(&blocked.stderr);
        assert!(
            stderr.contains("Brehon protected branch guard"),
            "stderr: {stderr}"
        );

        run_git(temp.path(), &["checkout", "-b", "feature/protected-hook"]);
        run_git(temp.path(), &["commit", "-m", "feature branch allowed"]);
        let blocked_ref_update = std::process::Command::new("git")
            .args(["update-ref", "refs/heads/main", "HEAD"])
            .current_dir(temp.path())
            .env_remove("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT")
            .env_remove("BREHON_PROTECTED_BRANCHES")
            .output()
            .unwrap();
        assert!(
            !blocked_ref_update.status.success(),
            "direct main ref update unexpectedly succeeded"
        );
        let ref_stderr = String::from_utf8_lossy(&blocked_ref_update.stderr);
        assert!(
            ref_stderr.contains("protected branch"),
            "stderr: {ref_stderr}"
        );
        assert_eq!(run_git(temp.path(), &["rev-parse", "main"]), initial_main);

        run_git(temp.path(), &["checkout", "main"]);
        std::fs::write(temp.path().join("repair.txt"), "repair\n").unwrap();
        run_git(temp.path(), &["add", "repair.txt"]);
        let repair = std::process::Command::new("git")
            .args(["commit", "-m", "deliberate repair"])
            .current_dir(temp.path())
            .env("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT", "1")
            .output()
            .unwrap();
        assert!(
            repair.status.success(),
            "explicit repair bypass should succeed: {}",
            String::from_utf8_lossy(&repair.stderr)
        );
    }

    #[test]
    fn test_protected_branch_guard_activation_drop_disarms_hooks() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        ensure_protected_branch_hooks(temp.path(), "main").unwrap();
        let activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();
        drop(activation);

        std::fs::write(temp.path().join("after-shutdown.txt"), "allowed\n").unwrap();
        run_git(temp.path(), &["add", "after-shutdown.txt"]);
        let output = std::process::Command::new("git")
            .args(["commit", "-m", "allowed after shutdown"])
            .current_dir(temp.path())
            .env_remove("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT")
            .env_remove("BREHON_PROTECTED_BRANCHES")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "commit after guard activation drop should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_ensure_protected_branch_hooks_preserves_existing_hook_body() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let hook_path = temp.path().join(".git").join("hooks").join("pre-commit");
        std::fs::write(
            &hook_path,
            "#!/bin/sh\necho existing hook ran >&2\nexit 42\n",
        )
        .unwrap();

        ensure_protected_branch_hooks(temp.path(), "main").unwrap();
        run_git(temp.path(), &["checkout", "-b", "feature/existing-hook"]);
        std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
        run_git(temp.path(), &["add", "feature.txt"]);

        let output = std::process::Command::new("git")
            .args(["commit", "-m", "feature"])
            .current_dir(temp.path())
            .env_remove("BREHON_ALLOW_PROTECTED_BRANCH_COMMIT")
            .env_remove("BREHON_PROTECTED_BRANCHES")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("existing hook ran"), "stderr: {stderr}");
        assert!(
            !stderr.contains("Brehon protected branch guard"),
            "feature branch should reach the preserved hook body: {stderr}"
        );
    }

    #[test]
    fn test_remove_protected_branch_hooks_preserves_existing_hook_body() {
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());
        let hooks_dir = temp.path().join(".git").join("hooks");
        let hook_path = hooks_dir.join("pre-commit");
        std::fs::write(
            &hook_path,
            "#!/bin/sh\necho existing hook ran >&2\nexit 42\n",
        )
        .unwrap();

        ensure_protected_branch_hooks(temp.path(), "main").unwrap();
        let marker_path = protected_branch_guard_marker_path(temp.path()).unwrap();
        let _activation = activate_protected_branch_guard(temp.path(), "test-session").unwrap();
        assert!(marker_path.exists());

        let removed = remove_protected_branch_hooks(temp.path()).unwrap();
        assert!(removed.iter().any(|path| path.ends_with("pre-commit")));
        assert!(!hooks_dir.join("commit-msg").exists());
        assert!(!marker_path.exists());

        let contents = std::fs::read_to_string(&hook_path).unwrap();
        assert!(!contents.contains(BREHON_PROTECTED_BRANCH_GUARD_BEGIN));
        assert!(contents.contains("existing hook ran"));

        run_git(temp.path(), &["checkout", "-b", "feature/cleaned-hook"]);
        std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
        run_git(temp.path(), &["add", "feature.txt"]);
        let output = std::process::Command::new("git")
            .args(["commit", "-m", "feature"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("existing hook ran"), "stderr: {stderr}");
        assert!(!stderr.contains("Brehon protected branch guard"));
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
        assert!(
            lines.iter().any(|l| l == ".brehon/"),
            ".brehon/ missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == ".mcp.json"),
            ".mcp.json missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == ".claude/settings.local.json"),
            ".claude/settings.local.json missing: {lines:?}"
        );
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
        // Pre-populate with only one of the three brehon patterns.
        std::fs::write(&exclude, ".brehon/\n").unwrap();

        ensure_brehon_ignored_in_repo(temp.path()).unwrap();

        let lines = exclude_lines(temp.path());
        // .brehon/ should appear exactly once — we don't re-add.
        assert_eq!(
            lines.iter().filter(|l| **l == ".brehon/").count(),
            1,
            ".brehon/ was duplicated: {lines:?}"
        );
        // The other two should have been added.
        assert!(lines.iter().any(|l| l == ".mcp.json"));
        assert!(lines.iter().any(|l| l == ".claude/settings.local.json"));
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
    }

    #[test]
    fn ensure_mcp_config_auto_ignores_generated_files() {
        // End-to-end contract: generating .mcp.json + .claude settings
        // also registers them in .git/info/exclude in one call, so a
        // fresh developer checkout never sees uncommitted noise.
        let temp = tempfile::tempdir().unwrap();
        init_git_repo(temp.path());

        ensure_mcp_config(temp.path()).unwrap();

        // Files were generated as expected.
        assert!(temp.path().join(".mcp.json").exists());
        assert!(temp.path().join(".claude/settings.local.json").exists());

        // And they're also git-ignored.
        let lines = exclude_lines(temp.path());
        assert!(
            lines.iter().any(|l| l == ".mcp.json"),
            "ensure_mcp_config did not auto-ignore .mcp.json: {lines:?}"
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
            !status.contains(".claude/settings.local.json"),
            "git status still sees settings.local.json as untracked:\n{status}"
        );
    }
}
