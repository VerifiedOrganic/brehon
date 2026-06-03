use std::path::{Path, PathBuf};

pub(crate) fn load_json_config(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub(crate) fn write_json_config(
    path: &Path,
    value: &serde_json::Value,
) -> std::result::Result<(), &'static str> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "Failed to create directory for local factory config.")?;
    }

    let content = serde_json::to_string_pretty(value)
        .map_err(|_| "Failed to serialize local factory config.")?;
    std::fs::write(path, content).map_err(|_| "Failed to write local factory config.")?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn link_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
pub(crate) fn link_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub(crate) fn mirror_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Ok(());
    }

    if link_path(src, dst).is_ok() {
        return Ok(());
    }

    if src.is_dir() {
        copy_dir_recursive(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

pub(crate) fn linked_worktree_gitdir(cwd: &Path) -> Option<PathBuf> {
    let git_file = cwd.join(".git");
    let contents = std::fs::read_to_string(git_file).ok()?;
    let line = contents.trim();
    let gitdir = line.strip_prefix("gitdir:")?.trim();
    if gitdir.is_empty() {
        return None;
    }

    let path = PathBuf::from(gitdir);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}
