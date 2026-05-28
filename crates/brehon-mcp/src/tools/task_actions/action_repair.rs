//! Deterministic supervisor repair actions for recoverable task-frontier drift.

use serde_json::Value;

use brehon_types::{is_terminal_task_status, normalize_task_status};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{structured_error_result, text_result};

use super::dependencies::{
    task_has_final_review_feedback, task_has_integrated_record,
    task_has_legacy_completed_worker_status, task_has_recoverable_worker_state_blocker_text,
    task_review_feedback_outcome,
};
use super::lifecycle::{ancestor_chain_has_closed_parent, caller_role};
use super::locking::acquire_task_lock;
use super::persistence::{read_all_tasks, read_task, write_task};
use super::structured_spec::control_plane_scope_issue_for_task;

#[derive(Debug, Clone)]
struct RecoverOutcome {
    task_id: String,
    from_status: String,
    to_status: String,
    latest_commit: String,
    already_recovered: bool,
}

fn task_id_from_task(task: &serde_json::Map<String, Value>) -> Option<String> {
    task.get("task_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn latest_commit(task: &serde_json::Map<String, Value>) -> Option<String> {
    task.get("latest_commit")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn repair_current_state(id: &str, task: Option<&serde_json::Map<String, Value>>) -> Value {
    let Some(task) = task else {
        return serde_json::json!({
            "task_id": id,
            "exists": false,
        });
    };
    serde_json::json!({
        "task_id": task_id_from_task(task).unwrap_or_else(|| id.to_string()),
        "exists": true,
        "status": task.get("status").cloned().unwrap_or(Value::Null),
        "task_type": task.get("task_type").cloned().unwrap_or(Value::Null),
        "assignee": task.get("assignee").cloned().unwrap_or(Value::Null),
        "review_owner": task.get("review_owner").cloned().unwrap_or(Value::Null),
        "latest_commit": task.get("latest_commit").cloned().unwrap_or(Value::Null),
        "integration_status": task.get("integration_status").cloned().unwrap_or(Value::Null),
        "review_feedback_outcome": task_review_feedback_outcome(task)
            .map(Value::String)
            .unwrap_or(Value::Null),
        "blockers": task.get("blockers").cloned().unwrap_or(Value::Null),
    })
}

fn ready_next_action() -> Value {
    serde_json::json!({
        "kind": "refresh_frontier",
        "tool": "task",
        "args": { "action": "ready" }
    })
}

fn recover_handoff_next_action(id: &str) -> Value {
    serde_json::json!({
        "kind": "recover_handoff",
        "tool": "task",
        "args": { "action": "recover_handoff", "id": id }
    })
}

fn request_review_next_action(id: &str) -> Value {
    serde_json::json!({
        "kind": "request_review",
        "tool": "verification",
        "args": { "action": "request_review", "task_id": id }
    })
}

fn structured_repair_error(
    code: &str,
    message: impl Into<String>,
    retryable: bool,
    current_state: Value,
    allowed_next_actions: Vec<Value>,
    next_action: Value,
) -> ToolResult {
    structured_error_result(
        code,
        message,
        retryable,
        current_state,
        allowed_next_actions,
        next_action,
    )
}

fn task_is_safe_handoff_repair_candidate(
    task: &serde_json::Map<String, Value>,
    all_tasks: &[serde_json::Map<String, Value>],
) -> bool {
    let status = task
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");
    task_type == "task"
        && latest_commit(task).is_some()
        && !task_has_integrated_record(task)
        && !task_has_final_review_feedback(task)
        && (normalize_task_status(status) == Some("blocked")
            && task_has_recoverable_worker_state_blocker_text(task)
            || task_has_legacy_completed_worker_status(task))
        && !ancestor_chain_has_closed_parent(all_tasks, task)
        && control_plane_scope_issue_for_task(task).is_none()
}

fn apply_recover_handoff(task: &mut serde_json::Map<String, Value>) {
    task.remove("blockers");
    task.insert(
        "percent".into(),
        Value::Number(serde_json::Number::from(100_u64)),
    );
    task.insert(
        "activity".into(),
        Value::String("awaiting_review".to_string()),
    );
    task.insert("assignee".into(), Value::Null);
    task.insert("review_owner".into(), Value::Null);
    task.insert("inbox_delivered".into(), Value::Bool(false));
    task.insert("status".into(), Value::String("review_ready".to_string()));
    task.insert(
        "recovery_note".into(),
        Value::String(
            "Recovered blocked worker handoff from recorded latest_commit; ready for review."
                .to_string(),
        ),
    );
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
}

async fn recover_handoff_by_id(id: &str) -> Result<RecoverOutcome, ToolResult> {
    let _lock = acquire_task_lock(id).await.map_err(|err| {
        structured_repair_error(
            "task_lock_failed",
            format!("Failed to lock task {id}: {err}"),
            true,
            serde_json::json!({ "task_id": id }),
            vec![ready_next_action()],
            ready_next_action(),
        )
    })?;

    let Some(mut task) = read_task(id) else {
        return Err(structured_repair_error(
            "task_not_found",
            format!("Task not found: {id}"),
            false,
            repair_current_state(id, None),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    };

    let current_status = task
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let normalized_status = normalize_task_status(&current_status);
    let legacy_completed = task_has_legacy_completed_worker_status(&task);
    let task_type = task
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");

    if is_terminal_task_status(&current_status) {
        return Err(structured_repair_error(
            "task_terminal",
            format!("Task {id} is terminal ({current_status}) and cannot be handoff-repaired."),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    let Some(commit) = latest_commit(&task) else {
        return Err(structured_repair_error(
            "handoff_missing_latest_commit",
            format!(
                "Task {id} has no latest_commit, so Brehon cannot safely recover it to review_ready."
            ),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    };

    if task_has_integrated_record(&task) {
        return Err(structured_repair_error(
            "handoff_already_integrated",
            format!(
                "Task {id} already records integration_status=integrated; recover_handoff must not move it back to review_ready."
            ),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    if let Some(outcome) = task_review_feedback_outcome(&task) {
        if matches!(outcome.as_str(), "approved" | "rejected") {
            return Err(structured_repair_error(
                "handoff_final_review_state",
                format!(
                    "Task {id} has final review_feedback outcome={outcome}; recover_handoff must not requeue the recorded latest_commit as a worker handoff."
                ),
                false,
                repair_current_state(id, Some(&task)),
                vec![ready_next_action()],
                ready_next_action(),
            ));
        }
    }

    if !legacy_completed && matches!(normalized_status, Some("review_ready" | "in_review")) {
        return Ok(RecoverOutcome {
            task_id: id.to_string(),
            from_status: current_status.clone(),
            to_status: current_status,
            latest_commit: commit,
            already_recovered: true,
        });
    }

    if task_type != "task" {
        return Err(structured_repair_error(
            "handoff_not_worker_task",
            format!(
                "Task {id} is task_type={task_type}; recover_handoff only repairs worker tasks."
            ),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    if normalized_status != Some("blocked") && !legacy_completed {
        return Err(structured_repair_error(
            "handoff_wrong_status",
            format!(
                "Task {id} is {current_status}; recover_handoff only repairs blocked tasks or legacy completed worker handoffs."
            ),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    if normalized_status == Some("blocked")
        && !task_has_recoverable_worker_state_blocker_text(&task)
    {
        return Err(structured_repair_error(
            "handoff_not_recoverable",
            format!(
                "Task {id} is blocked, but its blocker text is not a recognized recoverable worker handoff."
            ),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    let all_tasks = read_all_tasks();
    if ancestor_chain_has_closed_parent(&all_tasks, &task) {
        return Err(structured_repair_error(
            "handoff_closed_parent",
            format!("Task {id} has a closed ancestor and cannot be moved back to review_ready."),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    if let Some(scope_issue) = control_plane_scope_issue_for_task(&task) {
        return Err(structured_repair_error(
            "handoff_control_plane_scope",
            format!("Task {id} targets live Brehon control-plane state: {scope_issue}"),
            false,
            repair_current_state(id, Some(&task)),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    apply_recover_handoff(&mut task);
    if !write_task(id, &task) {
        return Err(structured_repair_error(
            "task_write_failed",
            format!("Failed to persist handoff repair for task {id}."),
            true,
            repair_current_state(id, Some(&task)),
            vec![recover_handoff_next_action(id), ready_next_action()],
            recover_handoff_next_action(id),
        ));
    }

    Ok(RecoverOutcome {
        task_id: id.to_string(),
        from_status: current_status,
        to_status: "review_ready".to_string(),
        latest_commit: commit,
        already_recovered: false,
    })
}

pub(super) async fn execute_recover_handoff(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|value| value.as_str()) {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => {
            return Ok(structured_repair_error(
                "missing_task_id",
                "Missing required parameter: id",
                false,
                Value::Null,
                vec![ready_next_action()],
                ready_next_action(),
            ));
        }
    };

    if caller_role(args) != "supervisor" {
        return Ok(structured_repair_error(
            "supervisor_required",
            "Only supervisors can recover blocked worker handoffs.",
            false,
            serde_json::json!({ "task_id": id, "caller_role": caller_role(args) }),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    match recover_handoff_by_id(id).await {
        Ok(outcome) => {
            let next_action = if outcome.to_status == "review_ready" {
                request_review_next_action(id)
            } else {
                ready_next_action()
            };
            let result = serde_json::json!({
                "status": "ok",
                "action": "recover_handoff",
                "task_id": outcome.task_id,
                "from_status": outcome.from_status,
                "to_status": outcome.to_status,
                "latest_commit": outcome.latest_commit,
                "already_recovered": outcome.already_recovered,
                "next_action": next_action,
            });
            Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|err| McpError::Serialization(err.to_string()))?,
            ))
        }
        Err(result) => Ok(result),
    }
}

pub(super) async fn execute_repair_frontier(args: &Value) -> Result<ToolResult, McpError> {
    if caller_role(args) != "supervisor" {
        return Ok(structured_repair_error(
            "supervisor_required",
            "Only supervisors can repair the task frontier.",
            false,
            serde_json::json!({ "caller_role": caller_role(args) }),
            vec![ready_next_action()],
            ready_next_action(),
        ));
    }

    let all_tasks = read_all_tasks();
    let mut candidate_ids: Vec<String> = all_tasks
        .iter()
        .filter(|task| task_is_safe_handoff_repair_candidate(task, &all_tasks))
        .filter_map(task_id_from_task)
        .collect();
    candidate_ids.sort();
    candidate_ids.dedup();

    if let Some(id) = args
        .get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        candidate_ids.retain(|candidate| candidate == id);
        if candidate_ids.is_empty() {
            candidate_ids.push(id.to_string());
        }
    }

    let mut repaired = Vec::new();
    let mut skipped = Vec::new();
    for id in candidate_ids {
        match recover_handoff_by_id(&id).await {
            Ok(outcome) => repaired.push(serde_json::json!({
                "task_id": outcome.task_id,
                "from_status": outcome.from_status,
                "to_status": outcome.to_status,
                "latest_commit": outcome.latest_commit,
                "already_recovered": outcome.already_recovered,
            })),
            Err(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        crate::server::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let parsed = serde_json::from_str::<Value>(&text)
                    .unwrap_or_else(|_| serde_json::json!({ "message": text }));
                skipped.push(parsed);
            }
        }
    }

    let next_action = ready_next_action();
    let result = serde_json::json!({
        "status": "ok",
        "action": "repair_frontier",
        "repaired_count": repaired.len(),
        "skipped_count": skipped.len(),
        "repaired": repaired,
        "skipped": skipped,
        "next_action": next_action,
    });
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|err| McpError::Serialization(err.to_string()))?,
    ))
}
