use std::path::{Path, PathBuf};

use brehon_types::config::SandboxSpec;

#[allow(unused_imports)]
pub use brehon_types::{
    build_advisor_startup_prompt, build_research_startup_prompt, build_reviewer_startup_prompt,
    build_supervisor_startup_prompt, build_worker_startup_prompt,
};

pub(crate) fn project_policy_for_role(brehon_root: Option<&PathBuf>, role: &str) -> Option<String> {
    let project_root = brehon_root?.parent()?;
    let config = brehon_config::load_config(Some(project_root)).ok()?;
    config.project_prompt_for_role_name(role)
}

pub(crate) fn sandbox_profile_allows_privileged_mode(brehon_root: Option<&PathBuf>) -> bool {
    let Some(project_root) = brehon_root.and_then(|root| root.parent()) else {
        return false;
    };
    let Ok(config) = brehon_config::load_config(Some(project_root)) else {
        return false;
    };

    matches!(
        config.security.sandbox_profile,
        brehon_types::SandboxProfile::None
    )
}

/// The confinement spec for an agent launched into `cwd`.
///
/// One call per launcher, so a new provider cannot be added without deciding.
pub(crate) fn agent_confinement(cwd: &Path, brehon_root: Option<&PathBuf>) -> SandboxSpec {
    let unsafe_profile = sandbox_profile_allows_privileged_mode(brehon_root);
    let root = brehon_root.cloned().unwrap_or_else(|| cwd.join(".brehon"));
    crate::pty::confine::default_spec(cwd, &root, unsafe_profile)
}

pub(crate) fn provider_cli_allows_privileged_mode(brehon_root: Option<&PathBuf>) -> bool {
    sandbox_profile_allows_privileged_mode(brehon_root)
}
