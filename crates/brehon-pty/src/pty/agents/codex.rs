use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::pty::config::{PtyConfig, TeamsSpawnConfig};
use crate::pty::filesystem::linked_worktree_gitdir;
use crate::pty::prompts::{build_supervisor_startup_prompt, project_policy_for_role};

use super::opencode::shell_single_quote;
use super::{
    current_brehon_exe, prepend_current_exe_dir_to_path, push_brehon_root_env,
    push_workspace_root_env,
};

pub(crate) fn codex_trusted_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();

    let cwd_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if seen.insert(cwd_path.clone()) {
        paths.push(cwd_path);
    }

    if let Some(gitdir) = linked_worktree_gitdir(cwd) {
        let gitdir = std::fs::canonicalize(&gitdir).unwrap_or(gitdir);
        if seen.insert(gitdir.clone()) {
            paths.push(gitdir);
        }
    }

    paths
}

pub(crate) fn toml_basic_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn prepare_local_codex_home(
    cwd: &Path,
    exe: &str,
    mcp_env: &[(String, String)],
) -> std::result::Result<PathBuf, &'static str> {
    let home_root = cwd.join(".brehon/factory-runtime/codex/home");
    std::fs::create_dir_all(&home_root)
        .map_err(|_| "Failed to create local Codex home directory.")?;

    if let Some(global_home) = dirs::home_dir().map(|d| d.join(".codex")) {
        for name in ["auth.json", "version.json", "models_cache.json"] {
            let src = global_home.join(name);
            if src.exists() {
                let dst = home_root.join(name);
                std::fs::copy(&src, &dst).map_err(|_| "Failed to seed local Codex home.")?;
            }
        }
    }

    let mut config = String::new();
    for trusted_path in codex_trusted_paths(cwd) {
        config.push_str(&format!(
            "[projects.\"{}\"]\n\
             trust_level = \"trusted\"\n\
             \n",
            trusted_path.to_string_lossy()
        ));
    }
    config.push_str(&format!(
        "[mcp_servers.brehon]\n\
         command = \"{exe}\"\n\
         args = [\"serve\"]\n",
        exe = toml_basic_string(exe),
    ));
    if !mcp_env.is_empty() {
        config.push_str("\n[mcp_servers.brehon.env]\n");
        for (key, value) in mcp_env {
            config.push_str(&format!("{key} = \"{}\"\n", toml_basic_string(value)));
        }
    }
    std::fs::write(home_root.join("config.toml"), config)
        .map_err(|_| "Failed to write local Codex config.")?;

    Ok(home_root)
}

pub(crate) fn push_codex_unrestricted_args(args: &mut Vec<String>) {
    args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    args.push("-c".to_string());
    args.push("shell_environment_policy.inherit=all".to_string());
    args.push("-c".to_string());
    args.push("sandbox_permissions=[\"disk-full-read-access\"]".to_string());
}

fn push_codex_common_args(
    args: &mut Vec<String>,
    cwd: &Path,
    role: &str,
    brehon_root: Option<&PathBuf>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    push_codex_unrestricted_args(args);
    if role != "supervisor" {
        args.push("--no-alt-screen".to_string());
    }
    // Disable personality/superpowers so Codex doesn't try to use
    // its built-in skills instead of Brehon MCP tools.
    args.push("--disable".to_string());
    args.push("personality".to_string());

    // Auto-trust the working directory so Codex doesn't prompt for trust
    // on worktree paths it hasn't seen before.
    for trusted_path in codex_trusted_paths(cwd) {
        args.push("-c".to_string());
        args.push(format!(
            "projects.\"{}\".trust_level=\"trusted\"",
            trusted_path.to_string_lossy()
        ));
    }

    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }

    if let Some(effort) = reasoning_effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={effort:?}"));
    }

    // Load Brehon instructions via model_instructions_file if .brehon dir is available.
    // This is additive — supplements AGENTS.md rather than replacing it.
    if let Some(root) = brehon_root {
        let instructions_filename = match role {
            "supervisor" => "codex-supervisor-instructions.md",
            "reviewer" => "codex-reviewer-instructions.md",
            "advisor" => "codex-advisor-instructions.md",
            "research" => "codex-research-instructions.md",
            _ => "codex-worker-instructions.md",
        };
        let instructions_path = root.join("instructions").join(instructions_filename);
        if instructions_path.exists() {
            args.push("-c".to_string());
            args.push(format!(
                "model_instructions_file=\"{}\"",
                instructions_path.to_string_lossy()
            ));
        }
    }
}

fn filtered_codex_app_server_args(custom_args: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut skip_next = false;

    for arg in custom_args {
        if skip_next {
            skip_next = false;
            continue;
        }

        match arg.as_str() {
            "app-server" => {}
            "--listen" => skip_next = true,
            _ => filtered.push(arg.clone()),
        }
    }

    filtered
}

impl PtyConfig {
    /// Create config for a Codex CLI instance
    ///
    /// # Arguments
    /// * `name` - Agent name
    /// * `role` - Agent role (e.g., "worker", "supervisor")
    /// * `cwd` - Working directory for the agent
    /// * `brehon_root` - Optional path to the .brehon directory. If provided, sets BREHON_ROOT env var
    /// * `supervisor_name` - For workers, the name of their supervisor (enables `target: supervisor`)
    #[allow(clippy::too_many_arguments)]
    pub fn codex(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        _teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        // Native Agent Teams is Claude Code-only; Codex CLI does not support it.
        let session_id = uuid::Uuid::new_v4().to_string();
        let brehon_exe = current_brehon_exe();
        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            ("BREHON_AGENT_TYPE".to_string(), "codex".to_string()),
            // Provide session ID so Brehon MCP server can self-register without hooks
            ("BREHON_SESSION_ID".to_string(), session_id),
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

        if let Ok(term) = std::env::var("TERM")
            && term.contains("ghostty")
        {
            env.push(("TERM".to_string(), "xterm-256color".to_string()));
        }

        if let Some(root) = brehon_root {
            push_brehon_root_env(&mut env, root);
        }

        if let Some(sup) = supervisor_name {
            env.push(("BREHON_SUPERVISOR_NAME".to_string(), sup.to_string()));
        }
        if let Some(worker_cli) = factory_worker_cli {
            env.push((
                "BREHON_FACTORY_WORKER_CLI".to_string(),
                worker_cli.to_string(),
            ));
        }

        let mcp_env = env
            .iter()
            .filter(|(key, _)| key.starts_with("BREHON_"))
            .cloned()
            .collect::<Vec<_>>();
        let codex_home = prepare_local_codex_home(&cwd, &brehon_exe, &mcp_env)
            .unwrap_or_else(|_| cwd.join(".brehon/factory-runtime/codex/home"));
        env.push((
            "CODEX_HOME".to_string(),
            codex_home.to_string_lossy().to_string(),
        ));

        let mut args = Vec::new();
        push_codex_common_args(&mut args, &cwd, role, brehon_root, model, reasoning_effort);

        if role == "supervisor" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_supervisor_startup_prompt(
                name,
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            );
            args.push(startup_prompt);
        }

        Self {
            command: "codex".to_string(),
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn codex_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let mut config = Self::codex(
            name,
            role,
            cwd.clone(),
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            None,
        );

        let mut args = Vec::new();
        push_codex_common_args(&mut args, &cwd, role, brehon_root, None, reasoning_effort);
        if let Some(m) = model {
            args.push("-c".to_string());
            args.push(format!("model={m:?}"));
        }
        args.push("app-server".to_string());
        config.args = args;
        config
    }

    #[allow(clippy::too_many_arguments)]
    pub fn custom_codex_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        agent_type: Option<&str>,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        custom_args: &[String],
    ) -> Self {
        let mut config = Self::codex(
            name,
            role,
            cwd.clone(),
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            None,
            None,
            None,
        );
        if let Some((_, value)) = config
            .env
            .iter_mut()
            .find(|(key, _)| key == "BREHON_AGENT_TYPE")
        {
            *value = agent_type.unwrap_or("codex").to_string();
        }

        let mut args = Vec::new();
        push_codex_common_args(&mut args, &cwd, role, brehon_root, None, None);
        if let Some(m) = model {
            args.push("-c".to_string());
            args.push(format!("model={m:?}"));
        }
        args.extend(filtered_codex_app_server_args(custom_args));
        args.push("app-server".to_string());
        config.args = args;
        config
    }

    #[allow(clippy::too_many_arguments)]
    pub fn codex_remote(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        server_url: &str,
        _teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        let mut config = Self::codex(
            name,
            role,
            cwd.clone(),
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            None,
        );
        let codex_command = config.command.clone();
        let mut codex_args = config.args.clone();
        codex_args.push("--remote".to_string());
        codex_args.push(server_url.to_string());
        let ready_path = cwd.join(".brehon/factory-runtime/codex/remote-ready");
        let _ = std::fs::remove_file(&ready_path);
        let codex_home = config
            .env
            .iter()
            .find_map(|(key, value)| (key == "CODEX_HOME").then_some(value.clone()))
            .unwrap_or_else(|| {
                cwd.join(".brehon/factory-runtime/codex/home")
                    .to_string_lossy()
                    .into_owned()
            });
        let base_prefix = std::iter::once(codex_command)
            .chain(codex_args)
            .map(|arg| shell_single_quote(&arg))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "export CODEX_HOME={codex_home}; \
ready_path={ready}; \
for _ in $(seq 1 200); do [ -f \"$ready_path\" ] && break; sleep 0.1; done; \
if [ -f \"$ready_path\" ]; then \
  exec {base_prefix}; \
else \
  echo \"brehon: codex remote session bootstrap failed\" >&2; \
  exit 1; \
fi",
            codex_home = shell_single_quote(&codex_home),
            ready = shell_single_quote(&ready_path.to_string_lossy()),
            base_prefix = base_prefix,
        );
        config.command = "sh".to_string();
        config.args = vec!["-c".to_string(), script];
        config
    }
}
