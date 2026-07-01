use std::path::Path;

use anyhow::Result;
use brehon_types::BrehonConfig;

use super::setup::ensure_agent_git_worktree_guard;

pub(super) fn ensure_agent_git_guard_bin(
    cwd: &Path,
    config: &BrehonConfig,
    splash: &mut crate::ui::StartupSplash,
) -> Result<Option<std::path::PathBuf>> {
    if !config.orchestration.worktree_isolation {
        return Ok(None);
    }

    splash.record("Installing agent Git shared-root guard".to_string());
    ensure_agent_git_worktree_guard(cwd).map(Some)
}

pub(super) fn prepend_optional_launcher_path(
    pairs: &mut Vec<(String, String)>,
    dir: Option<&Path>,
) {
    if let Some(dir) = dir {
        prepend_launcher_path(pairs, dir);
    }
}

fn prepend_launcher_path(pairs: &mut Vec<(String, String)>, dir: &Path) {
    if let Some((_, value)) = pairs.iter_mut().find(|(key, _)| key == "PATH") {
        *value = path_with_prepended_dir(dir, Some(value.as_str()));
    } else {
        let inherited_path = std::env::var("PATH").ok();
        pairs.push((
            "PATH".to_string(),
            path_with_prepended_dir(dir, inherited_path.as_deref()),
        ));
    }
}

fn path_with_prepended_dir(dir: &Path, existing: Option<&str>) -> String {
    if existing
        .filter(|value| !value.is_empty())
        .is_some_and(|value| std::env::split_paths(value).any(|part| part == dir))
    {
        return existing.unwrap_or_default().to_string();
    }

    let mut entries = vec![dir.to_path_buf()];
    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        entries.extend(std::env::split_paths(existing));
    }
    std::env::join_paths(entries)
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|_| {
            let mut value = dir.to_string_lossy().to_string();
            if let Some(existing) = existing.filter(|value| !value.is_empty()) {
                value.push(':');
                value.push_str(existing);
            }
            value
        })
}
