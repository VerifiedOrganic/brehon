pub(crate) mod agy;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod custom;
pub(crate) mod gemini;
pub(crate) mod junie;
pub(crate) mod kimi;
pub(crate) mod opencode;

pub(crate) fn current_brehon_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "brehon".to_string())
}

pub(crate) fn current_brehon_session_name() -> Option<String> {
    std::env::var("BREHON_SESSION_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) use brehon_adapter_sdk::{
    prepend_current_exe_dir_to_path, push_brehon_root_env, push_workspace_root_env,
};
