//! Path resolution helpers for task storage, archive, and workspace roots.

use std::path::{Component, Path, PathBuf};

/// Resolve the brehon root directory: `$BREHON_ROOT` or `$CWD/.brehon`.
pub(super) fn brehon_root_dir() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

pub(super) fn project_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("BREHON_PROJECT_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }

    if let Ok(root) = std::env::var("BREHON_ROOT") {
        let path = PathBuf::from(root);
        if path.file_name().and_then(|name| name.to_str()) == Some(".brehon") {
            return path.parent().map(PathBuf::from);
        }
        if path.join(".git").exists() {
            return Some(path);
        }
        if let Ok(workspace) = std::env::var("BREHON_WORKSPACE_ROOT") {
            let workspace = workspace.trim();
            if !workspace.is_empty() {
                let workspace = PathBuf::from(workspace);
                if workspace.join(".git").exists() {
                    return Some(workspace);
                }
            }
        }
        return Some(path);
    }

    std::env::current_dir()
        .ok()
        .filter(|cwd| cwd.join(".git").exists())
}

pub(super) fn resolve_project_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(project_root()?.join(path))
    }
}

pub(super) fn ensure_brehon_worktree_path(path: &Path, context: &str) -> Result<PathBuf, String> {
    let resolved = resolve_project_path(path)
        .ok_or_else(|| format!("No project root available to resolve {context} path."))?;
    if resolved
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Refusing to use {context} path '{}' because parent-directory components are not allowed.",
            resolved.display()
        ));
    }

    let worktrees_root = candidate_brehon_worktrees_root(&resolved)
        .or_else(|| brehon_root_dir().map(|root| root.join("worktrees")))
        .ok_or_else(|| format!("No BREHON_ROOT available for {context} path guard."))?;
    std::fs::create_dir_all(&worktrees_root).map_err(|err| {
        format!(
            "Failed to create Brehon worktrees root '{}': {err}",
            worktrees_root.display()
        )
    })?;
    let canonical_worktrees = worktrees_root.canonicalize().map_err(|err| {
        format!(
            "Failed to canonicalize Brehon worktrees root '{}': {err}",
            worktrees_root.display()
        )
    })?;

    if let Some(project) = project_root().and_then(|path| path.canonicalize().ok()) {
        if resolved.exists()
            && resolved
                .canonicalize()
                .ok()
                .is_some_and(|candidate| candidate == project)
        {
            return Err(format!(
                "Refusing to use {context} path '{}' because it is the primary project checkout.",
                resolved.display()
            ));
        }
    }

    if resolved.exists() {
        let canonical = resolved.canonicalize().map_err(|err| {
            format!(
                "Failed to canonicalize {context} path '{}': {err}",
                resolved.display()
            )
        })?;
        if !canonical.starts_with(&canonical_worktrees) {
            return Err(format!(
                "Refusing to use {context} path '{}' because it is outside Brehon-owned worktrees under '{}'.",
                resolved.display(),
                canonical_worktrees.display()
            ));
        }
        return Ok(resolved);
    }

    let mut ancestor = resolved.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            format!(
                "Cannot find an existing parent for {context} path '{}'.",
                resolved.display()
            )
        })?;
    }
    let canonical_ancestor = ancestor.canonicalize().map_err(|err| {
        format!(
            "Failed to canonicalize parent '{}' for {context} path '{}': {err}",
            ancestor.display(),
            resolved.display()
        )
    })?;
    if !canonical_ancestor.starts_with(&canonical_worktrees) {
        return Err(format!(
            "Refusing to create {context} path '{}' because its existing parent is outside Brehon-owned worktrees under '{}'.",
            resolved.display(),
            canonical_worktrees.display()
        ));
    }

    Ok(resolved)
}

fn candidate_brehon_worktrees_root(path: &Path) -> Option<PathBuf> {
    if std::env::var_os("BREHON_ROOT").is_some() {
        return None;
    }

    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.file_name().and_then(|name| name.to_str()) == Some("worktrees")
            && candidate
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                == Some(".brehon")
        {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

pub(super) fn workspace_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("BREHON_WORKSPACE_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }

    let brehon_root = brehon_root_dir()?;
    (brehon_root.file_name().and_then(|name| name.to_str()) == Some(".brehon"))
        .then(|| brehon_root.parent().map(PathBuf::from))
        .flatten()
}

/// Resolve the tasks directory: `$BREHON_ROOT/runtime/tasks/` or `$CWD/.brehon/runtime/tasks/`.
pub(super) fn tasks_dir() -> Option<PathBuf> {
    let root = brehon_root_dir()?;
    let dir = root.join("runtime").join("tasks");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub(super) fn task_path(task_id: &str) -> Option<PathBuf> {
    tasks_dir().map(|dir| dir.join(format!("{task_id}.json")))
}

pub(super) fn task_reviews_path(task_id: &str) -> Option<PathBuf> {
    brehon_root_dir().map(|root| root.join("runtime").join("reviews").join(task_id))
}

pub(super) fn archive_dir(kind: &str) -> Option<PathBuf> {
    let dir = brehon_root_dir()?
        .join("runtime")
        .join("archive")
        .join(kind);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub(super) fn unique_archive_path(dir: &Path, stem: &str, extension: Option<&str>) -> PathBuf {
    let mut candidate = match extension {
        Some(ext) => dir.join(format!("{stem}.{ext}")),
        None => dir.join(stem),
    };
    if !candidate.exists() {
        return candidate;
    }

    let mut index = 1usize;
    loop {
        candidate = match extension {
            Some(ext) => dir.join(format!("{stem}-{index}.{ext}")),
            None => dir.join(format!("{stem}-{index}")),
        };
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}
