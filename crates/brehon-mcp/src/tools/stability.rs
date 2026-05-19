use std::path::PathBuf;

pub(crate) fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

pub(crate) fn refresh_runtime_stability_counters() {
    if let Some(root) = brehon_root() {
        let _ = brehon_types::refresh_runtime_stability_counters(&root);
    }
}

pub(crate) fn increment_assignment_history(delta: usize) {
    if let Some(root) = brehon_root() {
        let _ = brehon_types::increment_assignment_history(&root, delta);
        let _ = brehon_types::refresh_runtime_stability_counters(&root);
    }
}
