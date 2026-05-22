use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::pty::config::{PtyConfig, TeamsSpawnConfig};
use crate::pty::filesystem::{copy_dir_recursive, load_json_config, write_json_config};
use crate::pty::prompts::{
    build_reviewer_startup_prompt, build_supervisor_startup_prompt, build_worker_startup_prompt,
    project_policy_for_role,
};

use super::brehon_skills::builtin_skill_names_for_role;
use super::{
    current_brehon_exe, prepend_current_exe_dir_to_path, push_brehon_root_env,
    push_workspace_root_env,
};

pub(crate) fn desired_gemini_mcp_servers(exe: &str) -> serde_json::Value {
    serde_json::json!({
        "brehon": {
            "command": exe,
            "args": ["serve"],
            "trust": true,
        }
    })
}

fn gemini_thinking_level(reasoning_effort: Option<&str>) -> Option<&'static str> {
    match reasoning_effort?.trim().to_ascii_lowercase().as_str() {
        "low" => Some("LOW"),
        "medium" => Some("MEDIUM"),
        "high" | "xhigh" | "max" => Some("HIGH"),
        _ => None,
    }
}

fn apply_gemini_reasoning_settings(
    settings: &mut serde_json::Value,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(), &'static str> {
    let Some(thinking_level) = gemini_thinking_level(reasoning_effort) else {
        return Ok(());
    };

    let Some(root) = settings.as_object_mut() else {
        return Err("Failed to update Gemini settings: settings.json is not a JSON object.");
    };
    let generation_config = root
        .entry("generationConfig".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(generation_config) = generation_config.as_object_mut() else {
        return Err("Failed to update Gemini settings: generationConfig is not a JSON object.");
    };
    let thinking_config = generation_config
        .entry("thinkingConfig".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(thinking_config) = thinking_config.as_object_mut() else {
        return Err("Failed to update Gemini settings: thinkingConfig is not a JSON object.");
    };

    thinking_config.remove("thinkingBudget");
    thinking_config.insert(
        "thinkingLevel".to_string(),
        serde_json::json!(thinking_level),
    );
    Ok(())
}

pub(crate) fn gemini_skill_root(base: &Path) -> PathBuf {
    base.join(".gemini/extensions/maestro/skills")
}

pub(crate) fn gemini_builtin_skill_names_for_role(role: &str) -> &'static [&'static str] {
    builtin_skill_names_for_role(role)
}

pub(crate) fn sync_local_gemini_skills(
    cwd: &Path,
    gemini_dir: &Path,
    role: &str,
) -> std::result::Result<(), &'static str> {
    let source_root = gemini_skill_root(cwd);
    let dest_root = gemini_dir.join("extensions/maestro/skills");
    std::fs::create_dir_all(&dest_root)
        .map_err(|_| "Failed to create Gemini runtime skills directory.")?;

    let allowed: HashSet<&str> = gemini_builtin_skill_names_for_role(role)
        .iter()
        .copied()
        .collect();

    for entry in
        std::fs::read_dir(&dest_root).map_err(|_| "Failed to inspect Gemini runtime skills.")?
    {
        let entry = entry.map_err(|_| "Failed to inspect Gemini runtime skill entry.")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("brehon-") {
            continue;
        }
        if !allowed.contains(name) {
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .map_err(|_| "Failed to remove stale Gemini runtime skill.")?;
            } else {
                std::fs::remove_file(&path)
                    .map_err(|_| "Failed to remove stale Gemini runtime skill.")?;
            }
        }
    }

    for skill_name in allowed {
        let src_dir = source_root.join(skill_name);
        let dst_dir = dest_root.join(skill_name);
        if !src_dir.exists() {
            if dst_dir.exists() {
                std::fs::remove_dir_all(&dst_dir)
                    .map_err(|_| "Failed to remove missing Gemini runtime skill.")?;
            }
            continue;
        }

        if dst_dir.exists() {
            std::fs::remove_dir_all(&dst_dir)
                .map_err(|_| "Failed to refresh Gemini runtime skill.")?;
        }
        copy_dir_recursive(&src_dir, &dst_dir)
            .map_err(|_| "Failed to copy Gemini runtime skill.")?;
    }

    Ok(())
}

pub(crate) fn prepare_local_gemini_home(
    cwd: &Path,
    exe: &str,
    role: &str,
    reasoning_effort: Option<&str>,
) -> std::result::Result<(PathBuf, PathBuf), &'static str> {
    let home_root = cwd.join(".brehon/factory-runtime/gemini/home");
    let gemini_dir = home_root.join(".gemini");
    std::fs::create_dir_all(&gemini_dir)
        .map_err(|_| "Failed to create local Gemini runtime directory.")?;

    if let Some(global_home) = dirs::home_dir().map(|d| d.join(".gemini")) {
        for name in [
            "gemini-credentials.json",
            "google_accounts.json",
            "oauth_creds.json",
            "installation_id",
            "state.json",
            "projects.json",
        ] {
            let src = global_home.join(name);
            if src.exists() {
                let dst = gemini_dir.join(name);
                std::fs::copy(&src, &dst).map_err(|_| "Failed to seed local Gemini auth state.")?;
            }
        }

        let settings_path = global_home.join("settings.json");
        let mut settings = load_json_config(&settings_path);
        let Some(root) = settings.as_object_mut() else {
            return Err(
                "Failed to update Gemini settings: ~/.gemini/settings.json is not a JSON object.",
            );
        };
        root.insert("mcpServers".to_string(), desired_gemini_mcp_servers(exe));
        apply_gemini_reasoning_settings(&mut settings, reasoning_effort)?;
        write_json_config(&gemini_dir.join("settings.json"), &settings)?;
    } else {
        let mut settings = serde_json::json!({ "mcpServers": desired_gemini_mcp_servers(exe) });
        apply_gemini_reasoning_settings(&mut settings, reasoning_effort)?;
        write_json_config(&gemini_dir.join("settings.json"), &settings)?;
    }

    let trusted_folders_path = gemini_dir.join("trustedFolders.json");
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    write_json_config(
        &trusted_folders_path,
        &serde_json::json!({
            canonical_cwd.to_string_lossy(): "TRUST_FOLDER"
        }),
    )?;

    sync_local_gemini_skills(cwd, &gemini_dir, role)?;

    Ok((home_root, trusted_folders_path))
}

impl PtyConfig {
    /// Create config for a Gemini CLI instance
    ///
    /// # Arguments
    /// * `name` - Agent name
    /// * `role` - Agent role (e.g., "worker", "supervisor")
    /// * `cwd` - Working directory for the agent
    /// * `brehon_root` - Optional path to the .brehon directory. If provided, sets BREHON_ROOT env var
    /// * `supervisor_name` - For workers, the name of their supervisor (enables `target: supervisor`)
    #[allow(clippy::too_many_arguments)]
    pub fn gemini(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        _teams: Option<&TeamsSpawnConfig>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        // Native Agent Teams is Claude Code-only; Gemini CLI does not support it.
        let session_id = uuid::Uuid::new_v4().to_string();
        let brehon_exe = current_brehon_exe();
        let (gemini_home, trusted_folders_path) =
            prepare_local_gemini_home(&cwd, &brehon_exe, role, reasoning_effort).unwrap_or_else(
                |_| {
                    let home = cwd.join(".brehon/factory-runtime/gemini/home");
                    let trusted = home.join(".gemini/trustedFolders.json");
                    (home, trusted)
                },
            );

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            ("BREHON_AGENT_TYPE".to_string(), "gemini".to_string()),
            // Provide session ID so Brehon MCP server can self-register without hooks
            ("BREHON_SESSION_ID".to_string(), session_id),
            (
                "BREHON_CLONE_PATH".to_string(),
                cwd.to_string_lossy().to_string(),
            ),
            // Disable sandbox so workers can edit files in worktrees
            ("GEMINI_SANDBOX".to_string(), "false".to_string()),
            (
                "HOME".to_string(),
                gemini_home.to_string_lossy().to_string(),
            ),
            (
                "GEMINI_CLI_TRUSTED_FOLDERS_PATH".to_string(),
                trusted_folders_path.to_string_lossy().to_string(),
            ),
        ];
        prepend_current_exe_dir_to_path(&mut env);
        push_workspace_root_env(&mut env, &cwd);

        if let Some(real_home) = dirs::home_dir() {
            let git_config_global = real_home.join(".gitconfig");
            if git_config_global.exists() {
                env.push((
                    "GIT_CONFIG_GLOBAL".to_string(),
                    git_config_global.to_string_lossy().to_string(),
                ));
            }
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

        let mut args = vec![
            "--approval-mode".to_string(),
            "yolo".to_string(),
            "--sandbox".to_string(),
            "false".to_string(),
        ];

        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m.to_string());
        }

        // Inject startup prompt via -i (--prompt-interactive) which starts the
        // REPL with the prompt text pre-submitted.
        if role == "worker" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_worker_startup_prompt(
                name,
                supervisor_name.unwrap_or("supervisor"),
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            );
            args.push("-i".to_string());
            args.push(startup_prompt);
        } else if role == "supervisor" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_supervisor_startup_prompt(
                name,
                "mcp_brehon_agent",
                "mcp_brehon_task",
                project_policy.as_deref(),
            );
            args.push("-i".to_string());
            args.push(startup_prompt);
        } else if role == "reviewer" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_reviewer_startup_prompt(
                name,
                "mcp_brehon_agent",
                "mcp_brehon_verification",
                project_policy.as_deref(),
            );
            args.push("-i".to_string());
            args.push(startup_prompt);
        }

        Self {
            command: "gemini".to_string(),
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn gemini_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let mut config = Self::gemini(
            name,
            role,
            cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            None,
            reasoning_effort,
        );
        let mut args = vec!["--acp".to_string()];
        if let Some(m) = model {
            args.push("--model".to_string());
            args.push(m.to_string());
        }
        config.args = args;
        config
    }
}
