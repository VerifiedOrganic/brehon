//! Review gate helpers that prevent supervisors from bypassing review debt.

use serde_json::{Map, Value};
use std::collections::HashMap;

use brehon_types::{is_terminal_task_status, normalize_task_status};

use super::lifecycle::is_container_task;
use super::paths::{archive_dir, task_reviews_path};
use super::persistence::{collect_descendant_ids_postorder, read_all_tasks};

#[derive(Debug, Clone)]
pub(super) struct ReviewObligationBlocker {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub reason: String,
}

impl ReviewObligationBlocker {
    fn from_task(task: &Map<String, Value>, reason: impl Into<String>) -> Self {
        Self {
            task_id: task_id(task).unwrap_or_else(|| "<unknown>".to_string()),
            title: task_title(task),
            status: task_status(task),
            reason: reason.into(),
        }
    }

    pub(super) fn summary(&self) -> String {
        format!(
            "{} ({}) status={} - {}",
            self.task_id, self.title, self.status, self.reason
        )
    }
}

pub(super) fn archive_review_obligation_blockers(
    task_id: &str,
    recursive: bool,
) -> Vec<ReviewObligationBlocker> {
    let all_tasks = read_all_tasks();
    let mut target_ids = Vec::new();
    if recursive {
        collect_descendant_ids_postorder(&all_tasks, task_id, &mut target_ids);
    }
    target_ids.push(task_id.to_string());

    target_ids
        .iter()
        .filter_map(|id| {
            all_tasks
                .iter()
                .find(|task| task.get("task_id").and_then(Value::as_str) == Some(id.as_str()))
                .and_then(|task| review_obligation_for_live_task_archive(task, &all_tasks))
        })
        .collect()
}

pub(super) fn archived_review_obligation_blockers_under(
    container_id: &str,
) -> Vec<ReviewObligationBlocker> {
    let live_tasks = read_all_tasks();
    let archived_tasks = read_archived_tasks();
    let task_index = task_index(live_tasks.iter().chain(archived_tasks.iter()));

    archived_tasks
        .iter()
        .filter(|task| task_is_descendant_of(task, container_id, &task_index))
        .filter_map(review_obligation_for_archived_task)
        .collect()
}

pub(super) fn format_review_obligation_blockers(
    action: &str,
    blockers: &[ReviewObligationBlocker],
) -> String {
    let summary = blockers
        .iter()
        .map(ReviewObligationBlocker::summary)
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "Cannot {action}: review obligations would be bypassed. \
         These tasks must go through the review pipeline, be reset/reworked, or have replacement review tasks created before the graph can advance: {summary}"
    )
}

fn review_obligation_for_live_task_archive(
    task: &Map<String, Value>,
    all_tasks: &[Map<String, Value>],
) -> Option<ReviewObligationBlocker> {
    concrete_review_obligation_reason_for_archive(task, all_tasks)
        .map(|reason| ReviewObligationBlocker::from_task(task, reason))
}

pub(super) fn live_review_obligation_blocker(
    task: &Map<String, Value>,
) -> Option<ReviewObligationBlocker> {
    concrete_review_obligation_reason(task)
        .map(|reason| ReviewObligationBlocker::from_task(task, reason))
}

fn review_obligation_for_archived_task(
    task: &Map<String, Value>,
) -> Option<ReviewObligationBlocker> {
    concrete_review_obligation_reason(task).map(|reason| {
        ReviewObligationBlocker::from_task(
            task,
            format!("archived before review/integration completed; {reason}"),
        )
    })
}

fn concrete_review_obligation_reason(task: &Map<String, Value>) -> Option<String> {
    concrete_review_obligation_reason_inner(task, None)
}

fn concrete_review_obligation_reason_for_archive(
    task: &Map<String, Value>,
    all_tasks: &[Map<String, Value>],
) -> Option<String> {
    concrete_review_obligation_reason_inner(task, Some(all_tasks))
}

fn concrete_review_obligation_reason_inner(
    task: &Map<String, Value>,
    archive_context: Option<&[Map<String, Value>]>,
) -> Option<String> {
    let task_type = task
        .get("task_type")
        .and_then(Value::as_str)
        .unwrap_or("task");
    if is_container_task(task_type) {
        return None;
    }

    let status = task_status(task);
    if is_terminal_task_status(&status) {
        return None;
    }
    let normalized = normalize_task_status(&status).unwrap_or(status.as_str());
    match normalized {
        "review_ready" => {
            return Some(
                "task is ready for review and cannot be removed from the graph".to_string(),
            )
        }
        "in_review" => {
            return Some(
                "task has an active review and cannot be removed from the graph".to_string(),
            )
        }
        "approved" => {
            return Some("task is approved but not terminal; close or integrate it".to_string())
        }
        "changes_requested" => {
            return Some(
                "task has review changes requested and must be revised/re-reviewed".to_string(),
            )
        }
        _ => {}
    }

    if task
        .get("latest_commit")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        let latest_commit = task
            .get("latest_commit")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if !archive_context.is_some_and(|all_tasks| {
            latest_commit_has_distinct_terminal_owner(task, latest_commit, all_tasks)
        }) {
            return Some(
                "task has a checkpoint/latest_commit that must be reviewed or explicitly reworked"
                    .to_string(),
            );
        }
    }

    if task_id(task)
        .as_deref()
        .is_some_and(|id| task_reviews_path(id).is_some_and(|path| review_state_has_entries(&path)))
    {
        return Some("task has persisted review state".to_string());
    }

    None
}

fn review_state_has_entries(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

fn latest_commit_has_distinct_terminal_owner(
    task: &Map<String, Value>,
    latest_commit: &str,
    all_tasks: &[Map<String, Value>],
) -> bool {
    let Some(current_id) = task_id(task) else {
        return false;
    };
    all_tasks.iter().any(|candidate| {
        task_id(candidate).is_some_and(|candidate_id| candidate_id != current_id)
            && candidate
                .get("latest_commit")
                .and_then(Value::as_str)
                .is_some_and(|candidate_commit| candidate_commit.trim() == latest_commit)
            && candidate
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(is_terminal_task_status)
    })
}

fn read_archived_tasks() -> Vec<Map<String, Value>> {
    let Some(dir) = archive_dir("tasks") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "json")
                && !entry.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(|entry| {
            let content = std::fs::read_to_string(entry.path()).ok()?;
            serde_json::from_str::<Map<String, Value>>(&content).ok()
        })
        .collect()
}

fn task_index<'a>(
    tasks: impl Iterator<Item = &'a Map<String, Value>>,
) -> HashMap<String, &'a Map<String, Value>> {
    let mut index = HashMap::new();
    for task in tasks {
        if let Some(id) = task_id(task) {
            index.entry(id).or_insert(task);
        }
    }
    index
}

fn task_is_descendant_of(
    task: &Map<String, Value>,
    container_id: &str,
    task_index: &HashMap<String, &Map<String, Value>>,
) -> bool {
    let mut parent_id = task
        .get("parent_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    while let Some(parent) = parent_id {
        if parent == container_id {
            return true;
        }
        parent_id = task_index
            .get(parent.as_str())
            .and_then(|task| task.get("parent_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    false
}

fn task_id(task: &Map<String, Value>) -> Option<String> {
    task.get("task_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn task_title(task: &Map<String, Value>) -> String {
    task.get("title")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("?")
        .to_string()
}

fn task_status(task: &Map<String, Value>) -> String {
    task.get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown")
        .to_string()
}
