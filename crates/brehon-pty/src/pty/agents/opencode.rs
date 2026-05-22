use std::path::{Path, PathBuf};

use crate::pty::config::{PtyConfig, TeamsSpawnConfig};
use crate::pty::filesystem::{
    linked_worktree_gitdir, load_json_config, mirror_path, write_json_config,
};
use crate::pty::prompts::{
    build_reviewer_startup_prompt, build_supervisor_startup_prompt, build_worker_startup_prompt,
    project_policy_for_role,
};

use super::brehon_skills::write_builtin_skills;
use super::{
    current_brehon_exe, prepend_current_exe_dir_to_path, push_brehon_root_env,
    push_workspace_root_env,
};

pub(crate) fn desired_opencode_mcp_config(exe: &str) -> serde_json::Value {
    serde_json::json!({
        "brehon": {
            "type": "local",
            "command": [exe, "serve"],
            "enabled": true,
        }
    })
}

pub(crate) const OPENCODE_MCP_TIMEOUT_MS: u64 = 600_000;

fn ensure_opencode_build_agent(
    root: &mut serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<&mut serde_json::Map<String, serde_json::Value>, &'static str> {
    let agent = root
        .entry("agent".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(agent_map) = agent.as_object_mut() else {
        return Err("Failed to update OpenCode config: `agent` is not a JSON object.");
    };
    let build = agent_map
        .entry("build".to_string())
        .or_insert_with(|| serde_json::json!({}));
    build
        .as_object_mut()
        .ok_or("Failed to update OpenCode config: `agent.build` is not a JSON object.")
}

fn resolve_opencode_variant_key(
    model_entry: &serde_json::Map<String, serde_json::Value>,
    reasoning_effort: &str,
) -> Option<String> {
    let variants = model_entry.get("variants")?.as_object()?;
    if variants.contains_key(reasoning_effort) {
        return Some(reasoning_effort.to_string());
    }

    None
}

pub(crate) fn apply_opencode_model_overrides(
    config: &mut serde_json::Value,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<bool, &'static str> {
    let Some(model) = model else {
        return Ok(false);
    };

    let Some((provider_id, model_id)) = model.split_once('/') else {
        return Ok(false);
    };

    let Some(root) = config.as_object_mut() else {
        return Err(
            "Failed to update OpenCode config: ~/.config/opencode/opencode.json is not a JSON object.",
        );
    };

    let mut changed = false;

    if root.get("model").and_then(|v| v.as_str()) != Some(model) {
        root.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
        changed = true;
    }

    {
        let build_agent = ensure_opencode_build_agent(root)?;
        if build_agent.get("model").and_then(|v| v.as_str()) != Some(model) {
            build_agent.insert(
                "model".to_string(),
                serde_json::Value::String(model.to_string()),
            );
            changed = true;
        }
    }

    if let Some(effort) = reasoning_effort {
        let provider = root
            .entry("provider".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(provider_map) = provider.as_object_mut() else {
            return Err("Failed to update OpenCode config: `provider` is not a JSON object.");
        };
        let provider_entry = provider_map
            .entry(provider_id.to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(provider_entry_map) = provider_entry.as_object_mut() else {
            return Err("Failed to update OpenCode config: provider entry is not a JSON object.");
        };
        let models = provider_entry_map
            .entry("models".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(models_map) = models.as_object_mut() else {
            return Err("Failed to update OpenCode config: provider models is not a JSON object.");
        };
        let model_entry = models_map
            .entry(model_id.to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(model_entry_map) = model_entry.as_object_mut() else {
            return Err(
                "Failed to update OpenCode config: provider model entry is not a JSON object.",
            );
        };

        if let Some(variant_key) = resolve_opencode_variant_key(model_entry_map, effort) {
            let build_agent = ensure_opencode_build_agent(root)?;
            if build_agent.get("variant").and_then(|v| v.as_str()) != Some(variant_key.as_str()) {
                build_agent.insert(
                    "variant".to_string(),
                    serde_json::Value::String(variant_key),
                );
                changed = true;
            }
            return Ok(changed);
        }

        let options = model_entry_map
            .entry("options".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(options_map) = options.as_object_mut() else {
            return Err(
                "Failed to update OpenCode config: provider model options is not a JSON object.",
            );
        };
        if options_map.get("reasoningEffort").and_then(|v| v.as_str()) != Some(effort) {
            options_map.insert(
                "reasoningEffort".to_string(),
                serde_json::Value::String(effort.to_string()),
            );
            changed = true;
        }
    }

    Ok(changed)
}

pub(crate) fn set_opencode_permission_pattern(
    config: &mut serde_json::Value,
    category: &str,
    pattern: String,
    action: &str,
) -> std::result::Result<bool, &'static str> {
    let Some(root) = config.as_object_mut() else {
        return Err(
            "Failed to update OpenCode config: ~/.config/opencode/opencode.json is not a JSON object.",
        );
    };

    let permission = root
        .entry("permission".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(permission_map) = permission.as_object_mut() else {
        return Err("Failed to update OpenCode config: `permission` is not a JSON object.");
    };
    let category_value = permission_map
        .entry(category.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(category_map) = category_value.as_object_mut() else {
        return Err("Failed to update OpenCode config: permission category is not a JSON object.");
    };

    if category_map.get(&pattern) == Some(&serde_json::Value::String(action.to_string())) {
        return Ok(false);
    }

    category_map.insert(pattern, serde_json::Value::String(action.to_string()));
    Ok(true)
}

pub(crate) fn set_opencode_permission_value(
    config: &mut serde_json::Value,
    category: &str,
    action: &str,
) -> std::result::Result<bool, &'static str> {
    let Some(root) = config.as_object_mut() else {
        return Err(
            "Failed to update OpenCode config: ~/.config/opencode/opencode.json is not a JSON object.",
        );
    };

    let permission = root
        .entry("permission".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(permission_map) = permission.as_object_mut() else {
        return Err("Failed to update OpenCode config: `permission` is not a JSON object.");
    };
    if permission_map.get(category) == Some(&serde_json::Value::String(action.to_string())) {
        return Ok(false);
    }

    permission_map.insert(
        category.to_string(),
        serde_json::Value::String(action.to_string()),
    );
    Ok(true)
}

pub(crate) fn merge_opencode_permission_pattern(
    config: &mut serde_json::Value,
    category: &str,
    pattern: String,
) -> std::result::Result<bool, &'static str> {
    set_opencode_permission_pattern(config, category, pattern, "allow")
}

pub(crate) fn ensure_opencode_noninteractive_permissions(
    config: &mut serde_json::Value,
) -> std::result::Result<bool, &'static str> {
    let mut changed = false;
    changed |=
        set_opencode_permission_pattern(config, "external_directory", "*".to_string(), "deny")?;
    changed |= set_opencode_permission_value(config, "doom_loop", "deny")?;
    changed |= set_opencode_permission_pattern(config, "read", "*.env".to_string(), "deny")?;
    changed |= set_opencode_permission_pattern(config, "read", "*.env.*".to_string(), "deny")?;
    Ok(changed)
}

pub(crate) fn ensure_opencode_factory_permissions(
    config: &mut serde_json::Value,
    cwd: &Path,
    _project_root: Option<&Path>,
) -> std::result::Result<bool, &'static str> {
    let mut changed = false;
    let mut allowed_roots = Vec::new();

    changed |= ensure_opencode_noninteractive_permissions(config)?;

    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    allowed_roots.push(canonical_cwd);

    if let Some(gitdir) = linked_worktree_gitdir(cwd) {
        let canonical_gitdir = std::fs::canonicalize(&gitdir).unwrap_or(gitdir);
        if !allowed_roots
            .iter()
            .any(|existing| existing == &canonical_gitdir)
        {
            allowed_roots.push(canonical_gitdir);
        }
    }

    for root in allowed_roots {
        let pattern = format!("{}/*", root.to_string_lossy());
        changed |=
            merge_opencode_permission_pattern(config, "external_directory", pattern.clone())?;
        changed |= merge_opencode_permission_pattern(config, "read", pattern)?;
    }

    Ok(changed)
}

pub(crate) fn ensure_opencode_spawn_config(
    config: &mut serde_json::Value,
    exe: &str,
    cwd: &Path,
    project_root: Option<&Path>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<bool, &'static str> {
    let Some(root) = config.as_object_mut() else {
        return Err(
            "Failed to update OpenCode config: ~/.config/opencode/opencode.json is not a JSON object.",
        );
    };
    let desired = desired_opencode_mcp_config(exe);
    let mut changed = false;
    if root.get("mcp") != Some(&desired) {
        root.insert("mcp".to_string(), desired);
        changed = true;
    }
    let experimental = root
        .entry("experimental".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(experimental_map) = experimental.as_object_mut() else {
        return Err("Failed to update OpenCode config: `experimental` is not a JSON object.");
    };
    if experimental_map.get("mcp_timeout").and_then(|v| v.as_u64()) != Some(OPENCODE_MCP_TIMEOUT_MS)
    {
        experimental_map.insert(
            "mcp_timeout".to_string(),
            serde_json::Value::Number(OPENCODE_MCP_TIMEOUT_MS.into()),
        );
        changed = true;
    }
    let _ = root;
    changed |= ensure_opencode_factory_permissions(config, cwd, project_root)?;
    changed |= apply_opencode_model_overrides(config, model, reasoning_effort)?;
    Ok(changed)
}

pub(crate) fn opencode_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("opencode"));
    }

    dirs::home_dir().map(|home| home.join(".config/opencode"))
}

pub(crate) fn opencode_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg).join("opencode"));
    }

    dirs::home_dir().map(|home| home.join(".local/share/opencode"))
}

pub(crate) fn prepare_local_opencode_runtime_with_global_config(
    cwd: &Path,
    exe: &str,
    global_config_dir: Option<&Path>,
    project_root: Option<&Path>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(PathBuf, String), &'static str> {
    let xdg_root = cwd.join(".brehon/factory-runtime/opencode/xdg");
    let local_config_dir = xdg_root.join("opencode");
    let local_data_dir = xdg_root.join("data/opencode");

    // Do not mirror arbitrary files from the user's global OpenCode config.
    // Global commands, agents, hooks, and node_modules make Brehon workers
    // depend on machine-local customization. We preserve the serialized
    // OpenCode config from opencode.json below and auth.json from the data
    // dir instead, but we do not copy the surrounding config tree.

    if let Some(global_data_dir) = opencode_data_dir()
        && global_data_dir.exists()
    {
        let name = "auth.json";
        let src = global_data_dir.join(name);
        if src.exists() {
            mirror_path(&src, &local_data_dir.join(name))
                .map_err(|_| "Failed to seed local OpenCode auth state.")?;
        }
    }

    let global_config_path = global_config_dir.map(|dir| dir.join("opencode.json"));
    let mut config = global_config_path
        .as_deref()
        .map(load_json_config)
        .unwrap_or_else(|| serde_json::json!({}));
    let _ =
        ensure_opencode_spawn_config(&mut config, exe, cwd, project_root, model, reasoning_effort)?;
    write_json_config(&local_config_dir.join("opencode.json"), &config)?;

    let content = serde_json::to_string(&config)
        .map_err(|_| "Failed to serialize OpenCode config for environment injection.")?;
    Ok((xdg_root, content))
}

pub(crate) fn prepare_local_opencode_runtime(
    cwd: &Path,
    project_root: Option<&Path>,
    exe: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(PathBuf, String), &'static str> {
    let global_config_dir = opencode_config_dir();
    prepare_local_opencode_runtime_with_global_config(
        cwd,
        exe,
        global_config_dir.as_deref(),
        project_root,
        model,
        reasoning_effort,
    )
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_opencode_env(
    name: &str,
    role: &str,
    cwd: &Path,
    brehon_root: Option<&PathBuf>,
    supervisor_name: Option<&str>,
    factory_worker_cli: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    teams: Option<&TeamsSpawnConfig>,
) -> Vec<(String, String)> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let brehon_exe = current_brehon_exe();
    let project_root = brehon_root
        .and_then(|root| root.parent())
        .map(Path::to_path_buf);
    let (xdg_root, config_content) = prepare_local_opencode_runtime(
        cwd,
        project_root.as_deref(),
        &brehon_exe,
        model,
        reasoning_effort,
    )
    .unwrap_or_else(|_| {
        let mut config = serde_json::json!({});
        let xdg_root = cwd.join(".brehon/factory-runtime/opencode/xdg");
        let _ = ensure_opencode_spawn_config(
            &mut config,
            &brehon_exe,
            cwd,
            project_root.as_deref(),
            model,
            reasoning_effort,
        );
        let _ = write_builtin_skills(&xdg_root.join("opencode/skills"), role);
        (
            xdg_root,
            serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string()),
        )
    });
    let _ = write_builtin_skills(&xdg_root.join("opencode/skills"), role);

    let mut env = vec![
        ("BREHON_AGENT_NAME".to_string(), name.to_string()),
        ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
        ("BREHON_AGENT_TYPE".to_string(), "opencode".to_string()),
        ("BREHON_SESSION_ID".to_string(), session_id),
        (
            "BREHON_CLONE_PATH".to_string(),
            cwd.to_string_lossy().to_string(),
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            xdg_root.to_string_lossy().to_string(),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            xdg_root.join("state").to_string_lossy().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            xdg_root.join("data").to_string_lossy().to_string(),
        ),
        (
            "XDG_CACHE_HOME".to_string(),
            cwd.join(".brehon/factory-runtime/opencode/home/.cache")
                .to_string_lossy()
                .to_string(),
        ),
        ("OPENCODE_CONFIG_CONTENT".to_string(), config_content),
        (
            "OPENCODE_DISABLE_AUTOUPDATE".to_string(),
            "true".to_string(),
        ),
        (
            "OPENCODE_DISABLE_CLAUDE_CODE".to_string(),
            "true".to_string(),
        ),
    ];
    prepend_current_exe_dir_to_path(&mut env);
    push_workspace_root_env(&mut env, cwd);

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

    if let Some(t) = teams {
        env.push(("BREHON_TEAM_NAME".to_string(), t.team_name.clone()));
    }

    env
}

pub(crate) fn opencode_runtime_root(cwd: &Path) -> PathBuf {
    cwd.join(".brehon/factory-runtime/opencode")
}

pub(crate) fn opencode_session_id_path(cwd: &Path) -> PathBuf {
    opencode_runtime_root(cwd).join("session-id")
}

fn append_opencode_server_auth(env: &mut Vec<(String, String)>) {
    let username = "brehon".to_string();
    let password = uuid::Uuid::new_v4().to_string();
    env.push(("OPENCODE_SERVER_USERNAME".to_string(), username));
    env.push(("OPENCODE_SERVER_PASSWORD".to_string(), password));
}

impl PtyConfig {
    /// Create config for an OpenCode CLI instance
    ///
    /// OpenCode runs as a persistent TUI harness.
    /// Model selection via `-m <provider>/<model>`.
    /// Provider-specific reasoning settings are written into the worker-local
    /// OpenCode config instead of being passed via CLI flags.
    ///
    /// # Arguments
    /// * `name` - Agent name
    /// * `role` - Agent role (e.g., "worker", "supervisor")
    /// * `cwd` - Working directory for the agent
    /// * `brehon_root` - Optional path to the .brehon directory
    /// * `supervisor_name` - For workers, the name of their supervisor
    #[allow(clippy::too_many_arguments)]
    pub fn opencode(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        let env = build_opencode_env(
            name,
            role,
            &cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            teams,
        );

        let mut args = Vec::new();

        if let Some(m) = model {
            args.push("-m".to_string());
            args.push(m.to_string());
        }

        // OpenCode supports an explicit startup prompt, which is more reliable
        // than PTY text injection for initial registration and task pickup.
        if role == "worker" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_worker_startup_prompt(
                name,
                supervisor_name.unwrap_or("supervisor"),
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            );
            args.push("--prompt".to_string());
            args.push(startup_prompt);
        } else if role == "supervisor" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_supervisor_startup_prompt(
                name,
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            );
            args.push("--prompt".to_string());
            args.push(startup_prompt);
        } else if role == "reviewer" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_reviewer_startup_prompt(
                name,
                "mcp_brehon_agent",
                "mcp_brehon_verification",
                project_policy.as_deref(),
            );
            args.push("--prompt".to_string());
            args.push(startup_prompt);
        }

        Self {
            command: "opencode".to_string(),
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn opencode_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let mut env = build_opencode_env(
            name,
            role,
            &cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            None,
        );
        if let Some(model) = model {
            env.push(("BREHON_AGENT_MODEL".to_string(), model.to_string()));
        }
        if let Some(reasoning_effort) = reasoning_effort {
            env.push((
                "BREHON_REASONING_EFFORT".to_string(),
                reasoning_effort.to_string(),
            ));
        }

        Self {
            command: "opencode".to_string(),
            args: vec![
                "acp".to_string(),
                "--cwd".to_string(),
                cwd.to_string_lossy().to_string(),
            ],
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    /// Create config for an OpenCode TUI instance that also exposes its local
    /// server on a fixed loopback URL. Brehon can then deliver turns through
    /// `opencode run --attach` instead of PTY text injection.
    #[allow(clippy::too_many_arguments)]
    pub fn opencode_server_backed(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        server_port: u16,
        teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        let mut env = build_opencode_env(
            name,
            role,
            &cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            teams,
        );
        append_opencode_server_auth(&mut env);
        let session_id_path = opencode_session_id_path(&cwd);
        let _ = std::fs::remove_file(&session_id_path);
        let server_url = format!("http://127.0.0.1:{server_port}");
        let script = format!(
            "url={url}\ndir={dir}\nsession_file={session_file}\nretries=0\nwhile true; do\n  if [ -s \"$session_file\" ]; then\n    session_id=$(tr -d '\\r\\n' < \"$session_file\")\n    if [ -n \"$session_id\" ]; then\n      exec opencode attach \"$url\" --session \"$session_id\" --dir \"$dir\"\n    fi\n  fi\n  if [ \"$retries\" -ge 50 ]; then\n    exec opencode attach \"$url\" --continue --dir \"$dir\"\n  fi\n  retries=$((retries + 1))\n  sleep 0.2\ndone",
            url = shell_single_quote(&server_url),
            dir = shell_single_quote(&cwd.to_string_lossy()),
            session_file = shell_single_quote(&session_id_path.to_string_lossy()),
        );

        Self {
            command: "zsh".to_string(),
            args: vec!["-lc".to_string(), script],
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn opencode_headless_server(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        server_port: u16,
        teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        let mut env = build_opencode_env(
            name,
            role,
            &cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            teams,
        );
        append_opencode_server_auth(&mut env);
        let server_url = format!("http://127.0.0.1:{server_port}");
        env.push(("BREHON_OPENCODE_SERVER_URL".to_string(), server_url));

        Self {
            command: "opencode".to_string(),
            args: vec![
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                server_port.to_string(),
            ],
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }
}
