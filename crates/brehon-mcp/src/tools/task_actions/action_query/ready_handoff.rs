use super::super::dependencies::{
    task_has_final_review_feedback, task_has_integrated_record,
    task_has_legacy_completed_worker_status, task_has_operator_directed_checkpoint_recovery,
    task_has_recoverable_blocked_review_checkpoint,
    task_has_recoverable_environment_limited_checkpoint,
    task_has_recoverable_worker_state_blocker_text, task_has_resolved_external_unblock_marker,
    task_review_feedback_outcome,
};
use super::*;

fn task_has_recorded_handoff_commit(task: &serde_json::Map<String, Value>) -> bool {
    task.get("latest_commit")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
}

pub(super) fn blocked_handoff_context(
    task: &serde_json::Map<String, Value>,
    all_tasks: &[serde_json::Map<String, Value>],
    config: Option<&brehon_types::BrehonConfig>,
) -> Option<Value> {
    let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let recoverable_blocked_handoff =
        status == "blocked" && task_has_recoverable_worker_state_blocker_text(task);
    let recoverable_review_checkpoint =
        status == "blocked" && task_has_recoverable_blocked_review_checkpoint(task);
    let recoverable_environment_checkpoint =
        status == "blocked" && task_has_recoverable_environment_limited_checkpoint(task);
    let operator_checkpoint_recovery =
        status == "blocked" && task_has_operator_directed_checkpoint_recovery(task);
    let resolved_external_unblock =
        status == "blocked" && task_has_resolved_external_unblock_marker(task, None);
    let legacy_completed = task_has_legacy_completed_worker_status(task);
    if task_type != "task"
        || (!recoverable_blocked_handoff
            && !recoverable_review_checkpoint
            && !recoverable_environment_checkpoint
            && !operator_checkpoint_recovery
            && !resolved_external_unblock
            && !legacy_completed)
    {
        return None;
    }

    let has_commit = task_has_recorded_handoff_commit(task);
    let closed_parent = ancestor_chain_has_closed_parent(all_tasks, task);
    let scope_issue = control_plane_scope_issue_for_task(task);
    let integrated_record = task_has_integrated_record(task);
    let final_review_feedback = task_has_final_review_feedback(task);
    let safe_repair = !resolved_external_unblock
        && has_commit
        && !closed_parent
        && scope_issue.is_none()
        && !integrated_record
        && !final_review_feedback;
    let mut value = ready_queue_task(task, config);
    let task_id = queued_task_id(&value).unwrap_or("").to_string();
    value["safe_repair"] = Value::Bool(safe_repair);
    value["recovery_kind"] = Value::String(if recoverable_review_checkpoint {
        "review_checkpoint".to_string()
    } else if recoverable_environment_checkpoint {
        "environment_limited_checkpoint".to_string()
    } else if operator_checkpoint_recovery {
        "operator_checkpoint_recovery".to_string()
    } else if resolved_external_unblock {
        "resolved_external_unblock".to_string()
    } else if recoverable_blocked_handoff {
        "worker_handoff".to_string()
    } else {
        "legacy_completed".to_string()
    });
    value["repair_action"] = if resolved_external_unblock {
        serde_json::json!({
            "kind": "unblock",
            "tool": "task",
            "args": {
                "action": "unblock",
                "id": task_id,
                "reason": "External blocker is recorded as resolved; return task to worker frontier."
            }
        })
    } else if safe_repair {
        serde_json::json!({
            "kind": "recover_handoff",
            "tool": "task",
            "args": {
                "action": "recover_handoff",
                "id": task_id
            }
        })
    } else if !has_commit {
        serde_json::json!({
            "kind": "wait_for_worker_checkpoint_or_reassign",
            "tool": "task",
            "args": {
                "action": "ready"
            }
        })
    } else {
        serde_json::json!({
            "kind": "inspect_task",
            "tool": "task",
            "args": {
                "action": "list",
                "status": "blocked"
            }
        })
    };
    value["repair_blocker"] = if safe_repair || resolved_external_unblock {
        Value::Null
    } else if !has_commit {
        Value::String("latest_commit is missing".to_string())
    } else if integrated_record {
        Value::String(
            "task already records integration_status=integrated; reconcile closure instead of re-reviewing"
                .to_string(),
        )
    } else if let Some(outcome) = task_review_feedback_outcome(task) {
        Value::String(format!(
            "task has final review_feedback outcome={outcome}; do not requeue the same commit"
        ))
    } else if legacy_completed {
        Value::String("legacy completed handoff state is not safe to repair".to_string())
    } else if closed_parent {
        Value::String("task has a closed ancestor".to_string())
    } else {
        Value::String(scope_issue.unwrap_or_else(|| "unsafe handoff state".to_string()))
    };
    Some(value)
}
