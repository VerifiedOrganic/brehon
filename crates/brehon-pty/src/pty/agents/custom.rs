use std::path::PathBuf;

use crate::pty::config::PtyConfig;

use super::{prepend_current_exe_dir_to_path, push_brehon_root_env, push_workspace_root_env};

impl PtyConfig {
    /// Create config for a custom interactive PTY launcher.
    #[allow(clippy::too_many_arguments)]
    pub fn custom_pty(
        name: &str,
        role: &str,
        command: &str,
        args: &[String],
        agent_type: Option<&str>,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        launch_policy: Option<&crate::pty::config::LaunchPolicy>,
    ) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut env = vec![
            ("BREHON_AGENT_NAME".to_string(), name.to_string()),
            ("BREHON_AGENT_ROLE".to_string(), role.to_string()),
            (
                "BREHON_AGENT_TYPE".to_string(),
                agent_type.unwrap_or(name).to_string(),
            ),
            ("BREHON_SESSION_ID".to_string(), session_id),
            (
                "BREHON_CLONE_PATH".to_string(),
                cwd.to_string_lossy().to_string(),
            ),
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

        if let Some(policy) = launch_policy {
            env.push((
                "BREHON_SANDBOX_PROFILE".to_string(),
                policy.profile_name().to_string(),
            ));
            env.push((
                "BREHON_LAUNCH_POLICY_UNSAFE".to_string(),
                policy.is_unsafe().to_string(),
            ));
        }

        Self {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: Some(cwd),
            env,
            rows: 24,
            cols: 80,
        }
    }

    /// Create config for a custom ACP-compatible launcher.
    #[allow(clippy::too_many_arguments)]
    pub fn custom_acp(
        name: &str,
        role: &str,
        command: &str,
        args: &[String],
        agent_type: Option<&str>,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        launch_policy: Option<&crate::pty::config::LaunchPolicy>,
    ) -> Self {
        Self::custom_pty(
            name,
            role,
            command,
            args,
            agent_type,
            cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            launch_policy,
        )
    }
}
