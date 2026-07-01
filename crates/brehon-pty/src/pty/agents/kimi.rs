use std::path::PathBuf;

use crate::pty::config::PtyConfig;
use crate::pty::prompts::{
    build_reviewer_startup_prompt, build_supervisor_startup_prompt, build_worker_startup_prompt,
    project_policy_for_role, sandbox_profile_allows_privileged_mode,
};

impl PtyConfig {
    /// Create config for an interactive Kimi CLI session.
    #[allow(clippy::too_many_arguments)]
    pub fn kimi(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> Self {
        let allow_privileged_mode = sandbox_profile_allows_privileged_mode(brehon_root);
        let mut config = brehon_adapter_kimi::build_kimi_spawn_config(
            name,
            role,
            cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
            allow_privileged_mode,
        );

        if role == "worker" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_worker_startup_prompt(
                name,
                supervisor_name.unwrap_or("supervisor"),
                "agent",
                "task",
                project_policy.as_deref(),
            );
            config.args.push("--prompt".to_string());
            config.args.push(startup_prompt);
        } else if role == "supervisor" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt =
                build_supervisor_startup_prompt(name, "agent", "task", project_policy.as_deref());
            config.args.push("--prompt".to_string());
            config.args.push(startup_prompt);
        } else if role == "reviewer" {
            let project_policy = project_policy_for_role(brehon_root, role);
            let startup_prompt = build_reviewer_startup_prompt(
                name,
                "agent",
                "verification",
                project_policy.as_deref(),
            );
            config.args.push("--prompt".to_string());
            config.args.push(startup_prompt);
        }

        Self {
            command: config.command,
            args: config.args,
            cwd: config.cwd,
            env: config.env,
            rows: config.rows,
            cols: config.cols,
        }
    }

    /// Create config for a Kimi CLI ACP session.
    #[allow(clippy::too_many_arguments)]
    pub fn kimi_acp(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        launch_policy: Option<&crate::pty::config::LaunchPolicy>,
    ) -> Self {
        let mut config = Self::kimi(
            name,
            role,
            cwd,
            brehon_root,
            supervisor_name,
            factory_worker_cli,
            model,
            reasoning_effort,
        );
        config.args = vec!["acp".to_string()];
        if let Some(policy) = launch_policy {
            config.env.push((
                "BREHON_SANDBOX_PROFILE".to_string(),
                policy.profile_name().to_string(),
            ));
            config.env.push((
                "BREHON_LAUNCH_POLICY_UNSAFE".to_string(),
                policy.is_unsafe().to_string(),
            ));
        }
        config
    }
}

// Re-export Kimi runtime helpers so existing callers in brehon-pty keep working.
// Re-export Kimi runtime helpers so existing callers in brehon-pty keep working.
#[allow(unused_imports)]
pub use brehon_adapter_kimi::{
    desired_kimi_mcp_config, kimi_share_dir, prepare_local_kimi_runtime,
    prepare_local_kimi_runtime_with_global_share,
};
