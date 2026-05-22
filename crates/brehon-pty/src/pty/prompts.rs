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
