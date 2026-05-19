use std::path::PathBuf;

use crate::pty::config::PtyConfig;

use super::{
    current_brehon_exe, prepend_current_exe_dir_to_path, push_brehon_root_env,
    push_workspace_root_env,
};

// Re-export Copilot runtime helpers from the adapter crate so that
// brehon-pty tests and consumers continue to work.
pub use brehon_adapter_copilot::{copilot_launch_command, prepare_local_copilot_runtime};

#[cfg(test)]
pub use brehon_adapter_copilot::{
    desired_copilot_mcp_config, prepare_local_copilot_runtime_with_global_config,
};

impl PtyConfig {
    /// Create config for an interactive GitHub Copilot CLI TUI session.
    ///
    /// Brehon delivers the first-turn bootstrap through the normal startup-prompt
    /// queue after the PTY has initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn copilot(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        let brehon_exe = current_brehon_exe();
        let (config_dir, cache_dir) =
            prepare_local_copilot_runtime(&cwd, &brehon_exe, model, reasoning_effort)
                .unwrap_or_else(|_| {
                    (
                        cwd.join(".brehon/factory-runtime/copilot/home"),
                        cwd.join(".brehon/factory-runtime/copilot/cache"),
                    )
                });

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            ("BREHON_AGENT_TYPE".to_string(), "copilot".to_string()),
            ("BREHON_SESSION_ID".to_string(), session_id),
            (
                "BREHON_CLONE_PATH".to_string(),
                cwd.to_string_lossy().to_string(),
            ),
            (
                "COPILOT_HOME".to_string(),
                config_dir.to_string_lossy().to_string(),
            ),
            (
                "COPILOT_CACHE_HOME".to_string(),
                cache_dir.to_string_lossy().to_string(),
            ),
            ("COPILOT_AUTO_UPDATE".to_string(), "false".to_string()),
        ];
        prepend_current_exe_dir_to_path(&mut env);
        push_workspace_root_env(&mut env, &cwd);

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

        let (command, mut args) = copilot_launch_command();
        args.extend([
            "--allow-all".to_string(),
            "--no-ask-user".to_string(),
            "--no-auto-update".to_string(),
            "--config-dir".to_string(),
            config_dir.to_string_lossy().to_string(),
        ]);

        if let Some(model) = model {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        Self {
            command,
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    /// Create config for a GitHub Copilot CLI ACP session.
    #[allow(clippy::too_many_arguments)]
    pub fn copilot_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        let brehon_exe = current_brehon_exe();
        let (config_dir, cache_dir) =
            prepare_local_copilot_runtime(&cwd, &brehon_exe, model, reasoning_effort)
                .unwrap_or_else(|_| {
                    (
                        cwd.join(".brehon/factory-runtime/copilot/home"),
                        cwd.join(".brehon/factory-runtime/copilot/cache"),
                    )
                });

        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            ("BREHON_AGENT_TYPE".to_string(), "copilot".to_string()),
            ("BREHON_SESSION_ID".to_string(), session_id),
            (
                "BREHON_CLONE_PATH".to_string(),
                cwd.to_string_lossy().to_string(),
            ),
            (
                "COPILOT_HOME".to_string(),
                config_dir.to_string_lossy().to_string(),
            ),
            (
                "COPILOT_CACHE_HOME".to_string(),
                cache_dir.to_string_lossy().to_string(),
            ),
            ("COPILOT_AUTO_UPDATE".to_string(), "false".to_string()),
        ];
        prepend_current_exe_dir_to_path(&mut env);
        push_workspace_root_env(&mut env, &cwd);

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

        let (command, mut args) = copilot_launch_command();
        args.extend([
            "--acp".to_string(),
            "--stdio".to_string(),
            "--allow-all".to_string(),
            "--no-ask-user".to_string(),
            "--no-auto-update".to_string(),
            "--config-dir".to_string(),
            config_dir.to_string_lossy().to_string(),
        ]);

        if let Some(model) = model {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        Self {
            command,
            args,
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }
}
