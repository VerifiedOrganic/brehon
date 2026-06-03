//! Task JSON persistence: read, write, archive, and bulk operations.

use serde_json::{Map, Value};
use std::path::PathBuf;

use super::dependencies::{read_dependency_ids, read_string_list_field, write_string_list_field};
use super::lifecycle::reconcile_dependency_states;
use super::locking::acquire_task_lock;
use super::paths::{archive_dir, task_path, task_reviews_path, tasks_dir, unique_archive_path};
use crate::tools::stability::refresh_runtime_stability_counters;
use crate::tools::verification::release_panel_lease_for_task;

/// Read a single task JSON by ID.
pub(super) fn read_task(task_id: &str) -> Option<serde_json::Map<String, Value>> {
    let dir = tasks_dir()?;
    let path = dir.join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write a task JSON by ID (atomic: temp file + rename).
pub(super) fn write_task(task_id: &str, task: &serde_json::Map<String, Value>) -> bool {
    let Some(dir) = tasks_dir() else {
        return false;
    };
    let path = dir.join(format!("{task_id}.json"));
    let tmp = dir.join(format!(".{task_id}.tmp"));
    let Ok(data) = serde_json::to_string_pretty(&Value::Object(task.clone())) else {
        return false;
    };
    if std::fs::write(&tmp, &data).is_ok() {
        let ok = std::fs::rename(&tmp, &path).is_ok();
        if ok {
            refresh_runtime_stability_counters();
        }
        ok
    } else {
        false
    }
}

/// Read all tasks from the tasks directory.
pub(super) fn read_all_tasks() -> Vec<serde_json::Map<String, Value>> {
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

pub(super) fn archive_task_record(
    task_id: &str,
    task: &Map<String, Value>,
    reason: &str,
) -> Result<PathBuf, String> {
    let Some(dir) = archive_dir("tasks") else {
        return Err("No archive/tasks dir available".to_string());
    };
    let path = unique_archive_path(&dir, task_id, Some("json"));
    let mut archived = task.clone();
    archived.insert(
        "archived_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    archived.insert("archive_reason".into(), Value::String(reason.to_string()));
    let payload = serde_json::to_string_pretty(&Value::Object(archived))
        .map_err(|err| format!("Failed to serialize archived task {task_id}: {err}"))?;
    std::fs::write(&path, payload).map_err(|err| {
        format!(
            "Failed to archive task {task_id} to {}: {err}",
            path.display()
        )
    })?;
    Ok(path)
}

pub(super) fn archive_review_state(task_id: &str) -> Result<Option<PathBuf>, String> {
    let Some(review_path) = task_reviews_path(task_id) else {
        return Ok(None);
    };
    if !review_path.exists() {
        return Ok(None);
    }
    let Some(dir) = archive_dir("reviews") else {
        return Err("No archive/reviews dir available".to_string());
    };
    let destination = unique_archive_path(&dir, task_id, None);
    std::fs::rename(&review_path, &destination).map_err(|err| {
        format!(
            "Failed to archive review state for task {task_id} from {} to {}: {err}",
            review_path.display(),
            destination.display()
        )
    })?;
    Ok(Some(destination))
}

pub(super) fn collect_descendant_ids_postorder(
    all_tasks: &[Map<String, Value>],
    parent_id: &str,
    out: &mut Vec<String>,
) {
    let children: Vec<String> = all_tasks
        .iter()
        .filter_map(|task| {
            (task.get("parent_id").and_then(|v| v.as_str()) == Some(parent_id))
                .then(|| {
                    task.get("task_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .flatten()
        })
        .collect();

    for child_id in children {
        collect_descendant_ids_postorder(all_tasks, &child_id, out);
        out.push(child_id);
    }
}

pub(super) async fn strip_dependency_edges_from_survivors(
    archived_ids: &[String],
) -> Result<Vec<String>, String> {
    let archived: std::collections::HashSet<&str> =
        archived_ids.iter().map(String::as_str).collect();
    let tasks = read_all_tasks();
    let mut modified = Vec::new();

    for mut task in tasks {
        let Some(task_id) = task
            .get("task_id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        if archived.contains(task_id.as_str()) {
            continue;
        }

        let before_dependencies = read_dependency_ids(&task);
        let filtered_dependencies: Vec<String> = before_dependencies
            .iter()
            .filter(|dependency| !archived.contains(dependency.as_str()))
            .cloned()
            .collect();

        let before_blocked_by = read_string_list_field(&task, "blocked_by");
        let filtered_blocked_by: Vec<String> = before_blocked_by
            .iter()
            .filter(|dependency| !archived.contains(dependency.as_str()))
            .cloned()
            .collect();

        if filtered_dependencies != before_dependencies || filtered_blocked_by != before_blocked_by
        {
            write_string_list_field(&mut task, "dependencies", &filtered_dependencies);
            write_string_list_field(&mut task, "blocked_by", &filtered_blocked_by);
            task.insert(
                "updated_at".into(),
                Value::String(chrono::Utc::now().to_rfc3339()),
            );
            let _lock = acquire_task_lock(&task_id).await?;
            if !write_task(&task_id, &task) {
                return Err(format!(
                    "Failed to persist dependency cleanup after archiving task {task_id}"
                ));
            }
            modified.push(task_id);
        }
    }

    Ok(modified)
}

pub(super) async fn archive_task_runtime(
    task_id: &str,
    reason: &str,
    recursive: bool,
) -> Result<Value, String> {
    let all_tasks = read_all_tasks();
    let Some(_task) = all_tasks
        .iter()
        .find(|candidate| candidate.get("task_id").and_then(|v| v.as_str()) == Some(task_id))
        .cloned()
    else {
        return Err(format!("Task not found: {task_id}"));
    };

    let mut descendants = Vec::new();
    collect_descendant_ids_postorder(&all_tasks, task_id, &mut descendants);
    if !recursive && !descendants.is_empty() {
        return Err(format!(
            "Task {task_id} has {} descendant task(s). Re-run with recursive=true to archive the full subtree.",
            descendants.len()
        ));
    }

    let mut archive_order = descendants;
    archive_order.push(task_id.to_string());

    let mut archived_tasks = Vec::new();
    let mut archived_reviews = Vec::new();
    let mut released_panels = Vec::new();

    for archived_id in &archive_order {
        let _lock = acquire_task_lock(archived_id).await?;
        let Some(task) = read_task(archived_id) else {
            continue;
        };

        let archived_path = archive_task_record(archived_id, &task, reason)?;
        if let Some(panel_id) = release_panel_lease_for_task(archived_id)? {
            released_panels.push(panel_id);
        }
        if let Some(review_path) = archive_review_state(archived_id)? {
            archived_reviews.push(review_path.display().to_string());
        }
        if let Some(path) = task_path(archived_id) {
            std::fs::remove_file(&path).map_err(|err| {
                format!(
                    "Failed to remove live task {archived_id} at {}: {err}",
                    path.display()
                )
            })?;
        }
        archived_tasks.push(archived_path.display().to_string());
    }

    strip_dependency_edges_from_survivors(&archive_order).await?;
    reconcile_dependency_states().await?;
    refresh_runtime_stability_counters();

    Ok(serde_json::json!({
        "status": "ok",
        "action": "archive",
        "task_id": task_id,
        "archived_task_ids": archive_order,
        "archived_task_paths": archived_tasks,
        "archived_review_paths": archived_reviews,
        "released_panels": released_panels,
        "reason": reason,
    }))
}
