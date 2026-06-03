//! Review followup tracking: read, write, summarize, and container-level blocking.

use serde_json::Value;

use super::lifecycle::{direct_children, reconcile_dependency_states_with_task_lock};
use super::locking::acquire_task_lock;
use super::persistence::{read_all_tasks, read_task, write_task};

pub(super) fn read_followups_field(task: &serde_json::Map<String, Value>) -> Vec<Value> {
    task.get("review_followups")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
}

pub(super) fn followup_status(followup: &Value) -> &str {
    followup
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("open")
}

pub(super) fn is_open_followup(followup: &Value) -> bool {
    followup_status(followup) == "open"
}

pub(super) fn followup_id(followup: &Value) -> Option<&str> {
    followup.get("followup_id").and_then(|value| value.as_str())
}

pub(super) fn summarize_followups(task: &serde_json::Map<String, Value>) -> Option<Value> {
    let followups = read_followups_field(task);
    if followups.is_empty() {
        return None;
    }

    let mut open = 0usize;
    let mut tasked = 0usize;
    let mut waived = 0usize;
    let mut done = 0usize;
    let mut other = 0usize;

    for followup in &followups {
        match followup_status(followup) {
            "open" => open += 1,
            "tasked" => tasked += 1,
            "waived" => waived += 1,
            "done" => done += 1,
            _ => other += 1,
        }
    }

    Some(serde_json::json!({
        "total": followups.len(),
        "open": open,
        "tasked": tasked,
        "waived": waived,
        "done": done,
        "other": other
    }))
}

pub(super) fn write_followups_field(
    task: &mut serde_json::Map<String, Value>,
    followups: &[Value],
) {
    if followups.is_empty() {
        task.remove("review_followups");
    } else {
        task.insert("review_followups".into(), Value::Array(followups.to_vec()));
    }
}

pub(crate) async fn append_task_review_followups(
    task_id: &str,
    followups: &[Value],
) -> Result<(), String> {
    if followups.is_empty() {
        return Ok(());
    }

    let _lock = acquire_task_lock(task_id).await?;
    let Some(mut task) = read_task(task_id) else {
        return Err(format!("Task not found: {task_id}"));
    };

    let mut combined = read_followups_field(&task);
    combined.extend_from_slice(followups);
    write_followups_field(&mut task, &combined);
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );

    if !write_task(task_id, &task) {
        return Err(format!("Failed to write task {task_id}"));
    }
    reconcile_dependency_states_with_task_lock(task_id).await?;
    Ok(())
}

pub(crate) async fn resolve_promoted_followups_for_terminal_task(
    terminal_task_id: &str,
    terminal_task: &serde_json::Map<String, Value>,
) -> Result<bool, String> {
    if !terminal_task
        .get("review_followup_task")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return Ok(false);
    }

    let Some(source_task_id) = terminal_task
        .get("source_task_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(false);
    };

    let _lock = acquire_task_lock(source_task_id).await?;
    let Some(mut source_task) = read_task(source_task_id) else {
        return Err(format!(
            "Promoted followup task {terminal_task_id} references missing source task {source_task_id}"
        ));
    };

    let mut followups = read_followups_field(&source_task);
    let now = chrono::Utc::now().to_rfc3339();
    let mut changed = false;
    for followup in &mut followups {
        if followup
            .get("followup_task_id")
            .and_then(|value| value.as_str())
            != Some(terminal_task_id)
        {
            continue;
        }
        if matches!(followup_status(followup), "done" | "waived") {
            continue;
        }
        let Some(object) = followup.as_object_mut() else {
            continue;
        };
        object.insert("status".into(), Value::String("done".to_string()));
        object.insert("updated_at".into(), Value::String(now.clone()));
        object.insert("resolved_at".into(), Value::String(now.clone()));
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    write_followups_field(&mut source_task, &followups);
    source_task.insert("updated_at".into(), Value::String(now));
    if !write_task(source_task_id, &source_task) {
        return Err(format!(
            "Task {terminal_task_id} reached terminal state, but Brehon failed to update review followups on source task {source_task_id}"
        ));
    }

    Ok(true)
}

#[derive(Debug, Clone)]
pub(super) struct ContainerFollowupBlocker {
    pub(super) task_id: String,
    pub(super) title: String,
    pub(super) open_followups: Vec<Value>,
}

pub(super) fn collect_descendant_task_ids(
    all_tasks: &[serde_json::Map<String, Value>],
    parent_id: &str,
) -> Vec<String> {
    let mut collected = Vec::new();
    let mut stack = vec![parent_id.to_string()];

    while let Some(current) = stack.pop() {
        for child in direct_children(all_tasks, &current) {
            if let Some(task_id) = child.get("task_id").and_then(|value| value.as_str()) {
                collected.push(task_id.to_string());
                stack.push(task_id.to_string());
            }
        }
    }

    collected
}

pub(super) fn collect_container_open_followup_blockers(
    container_id: &str,
) -> Vec<ContainerFollowupBlocker> {
    let all_tasks = read_all_tasks();
    let descendant_ids = collect_descendant_task_ids(&all_tasks, container_id);

    descendant_ids
        .into_iter()
        .filter_map(|task_id| {
            let task = all_tasks.iter().find(|candidate| {
                candidate.get("task_id").and_then(|value| value.as_str()) == Some(task_id.as_str())
            })?;
            let open_followups: Vec<Value> = read_followups_field(task)
                .into_iter()
                .filter(is_open_followup)
                .collect();
            if open_followups.is_empty() {
                return None;
            }
            Some(ContainerFollowupBlocker {
                task_id,
                title: task
                    .get("title")
                    .and_then(|value| value.as_str())
                    .unwrap_or("(untitled)")
                    .to_string(),
                open_followups,
            })
        })
        .collect()
}
