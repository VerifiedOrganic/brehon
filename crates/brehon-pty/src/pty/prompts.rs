use std::path::PathBuf;

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

pub(crate) fn provider_cli_allows_privileged_mode(brehon_root: Option<&PathBuf>) -> bool {
    sandbox_profile_allows_privileged_mode(brehon_root)
}
