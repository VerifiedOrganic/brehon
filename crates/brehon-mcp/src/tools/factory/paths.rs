//! Shared filesystem helpers for the factory module.
//!
//! These mirror the path conventions used in `task_actions`.

use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::tools::agent::{session_is_live, session_matches_current_runtime};

pub(crate) fn brehon_root() -> Option<PathBuf> {
    std::env::var("BREHON_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))
}

pub(crate) fn worktrees_root() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("BREHON_WORKTREE_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return Some(PathBuf::from(root));
        }
    }

    brehon_root().map(|root| root.join("worktrees"))
}

pub(crate) fn tasks_dir() -> Option<PathBuf> {
    let dir = brehon_root()?.join("runtime").join("tasks");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub(crate) fn read_task(task_id: &str) -> Option<serde_json::Map<String, Value>> {
    let path = tasks_dir()?.join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_all_tasks() -> Vec<serde_json::Map<String, Value>> {
    let Some(dir) = tasks_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "json")
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str(&content).ok()
        })
        .collect()
}

pub(crate) fn write_task(task_id: &str, task: &serde_json::Map<String, Value>) -> bool {
    let Some(dir) = tasks_dir() else {
        return false;
    };
    let path = dir.join(format!("{task_id}.json"));
    let tmp = dir.join(format!(".{task_id}.tmp"));
    let Ok(data) = serde_json::to_string_pretty(&Value::Object(task.clone())) else {
        return false;
    };
    if std::fs::write(&tmp, &data).is_ok() {
        std::fs::rename(&tmp, &path).is_ok()
    } else {
        false
    }
}

pub(crate) fn sessions_dir() -> Option<PathBuf> {
    let dir = brehon_root()?.join("runtime").join("sessions");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub(crate) fn project_root() -> Option<PathBuf> {
    brehon_root()?.parent().map(PathBuf::from)
}

pub(crate) fn resolve_project_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(project_root()?.join(path))
    }
}

/// Read all session files to get registered worker info.
pub(crate) fn read_sessions() -> Vec<Value> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.path().extension().is_some_and(|ext| ext == "json")
                && !e.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let value: Value = serde_json::from_str(&content).ok()?;
            if session_is_live(&value) && session_matches_current_runtime(&value) {
                Some(value)
            } else {
                None
            }
        })
        .collect()
}
