use std::path::PathBuf;

use brehon_types::{
    refresh_runtime_stability_counters, remove_session_stability_counters,
    write_session_stability_counters, StabilityCounters,
};

pub fn persist_session_snapshot(session_key: &str, counters: StabilityCounters) {
    if let Some(root) = brehon_root_from_env() {
        if write_session_stability_counters(&root, session_key, counters).is_ok() {
            let _ = refresh_runtime_stability_counters(&root);
        }
    }
}

pub fn clear_session_snapshot(session_key: &str) {
    if let Some(root) = brehon_root_from_env() {
        if remove_session_stability_counters(&root, session_key).is_ok() {
            let _ = refresh_runtime_stability_counters(&root);
        }
    }
}

fn brehon_root_from_env() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}
