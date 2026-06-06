use super::*;
use crate::tools::task_actions::integration_state::{read_integration_state, IntegrationPhase};

pub(super) fn integrated_closeout_tasks(
    all_tasks: &[serde_json::Map<String, Value>],
    config: Option<&brehon_types::BrehonConfig>,
) -> Vec<Value> {
    all_tasks
        .iter()
        .filter(|task| !task_has_supervisor_integration_conflict(task))
        .filter(|task| !ancestor_chain_has_closed_parent(all_tasks, task))
        .filter(|task| control_plane_scope_issue_for_task(task).is_none())
        .filter_map(|task| integrated_closeout_task(task, config))
        .collect()
}

pub(super) fn first_next_action(tasks: &[Value]) -> Option<Value> {
    tasks
        .first()
        .and_then(|task| task.get("next_action"))
        .cloned()
}

fn integrated_closeout_task(
    task: &serde_json::Map<String, Value>,
    config: Option<&brehon_types::BrehonConfig>,
) -> Option<Value> {
    if !is_integrated_closeout_candidate(task) {
        return None;
    }

    let integration_status = task
        .get("integration_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let integration_state = read_integration_state(task);
    let mut value = ready_queue_task(task, config);
    let task_id = queued_task_id(&value).unwrap_or("").to_string();
    value["integration_status"] = Value::String(integration_status.to_string());
    value["integration_phase"] = Value::String(integration_state.phase.as_str().to_string());
    value["next_action"] = serde_json::json!({
        "kind": "integrate_closeout",
        "tool": "task",
        "args": {
            "action": "integrate",
            "id": task_id
        }
    });
    Some(value)
}

pub(super) fn is_integrated_closeout_candidate(task: &serde_json::Map<String, Value>) -> bool {
    let status = task.get("status").and_then(Value::as_str).unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(Value::as_str)
        .unwrap_or("task");
    let completion_mode = task
        .get("completion_mode")
        .and_then(Value::as_str)
        .unwrap_or("");
    if normalize_task_status(status) != Some("approved")
        || task_type != "task"
        || completion_mode != "merge"
    {
        return false;
    }

    let integration_status = task
        .get("integration_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let integration_state = read_integration_state(task);
    integration_status == "integrated" || integration_state.phase == IntegrationPhase::Aborted
}
