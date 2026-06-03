use serde_json::Value;

use brehon_types::{infer_task_completion_mode, parse_task_completion_mode, TaskCompletionMode};

use super::helpers::{brehon_root, git_output};

/// Read a task JSON file by task_id.
pub(crate) fn read_task(task_id: &str) -> Option<Value> {
    let root = brehon_root()?;
    let path = root
        .join("runtime")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_task_status(task_id: &str) -> Option<String> {
    read_task(task_id).and_then(|task| {
        task.get("status")
            .and_then(|value| value.as_str())
            .map(String::from)
    })
}

pub(crate) fn read_task_assignee(task_id: &str) -> Option<String> {
    read_task(task_id).and_then(|task| {
        task.get("assignee")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

pub(crate) fn read_task_completion_mode(task_id: &str) -> TaskCompletionMode {
    let Some(task) = read_task(task_id) else {
        return TaskCompletionMode::Merge;
    };

    let task_type = task
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");
    if matches!(task_type, "epic" | "initiative") {
        return TaskCompletionMode::Close;
    }

    let title = task
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let description = task
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    task.get("completion_mode")
        .and_then(|value| value.as_str())
        .and_then(parse_task_completion_mode)
        .unwrap_or_else(|| infer_task_completion_mode(title, description))
}

pub(crate) fn read_task_recorded_commit(task_id: &str) -> Option<String> {
    read_task(task_id).and_then(|task| {
        task.get("latest_commit")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

pub(crate) fn detect_default_branch() -> Option<String> {
    if let Some(branch) = git_output(&["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(stripped) = branch.strip_prefix("refs/remotes/origin/") {
            return Some(stripped.to_string());
        }
    }

    for candidate in ["main", "master", "develop"] {
        if git_output(&["rev-parse", "--verify", &format!("refs/heads/{candidate}")]).is_some() {
            return Some(candidate.to_string());
        }
    }

    Some("main".to_string())
}

pub(crate) fn read_task_merge_target(task_id: &str) -> Option<String> {
    read_task(task_id).and_then(|task| {
        task.get("merge_target")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
    })
}

pub(crate) fn merge_target_requires_epic_integration(task_id: &str) -> bool {
    let Some(merge_target) = read_task_merge_target(task_id) else {
        return false;
    };
    let default_branch = detect_default_branch().unwrap_or_else(|| "main".to_string());
    merge_target != default_branch
}
