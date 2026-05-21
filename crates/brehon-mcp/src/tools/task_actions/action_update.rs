//! Handlers for state-change task actions: update, progress, checkpoint, archive.

use serde_json::Value;
use std::path::PathBuf;

use brehon_types::{
    is_terminal_task_status, normalize_task_status, parse_task_completion_mode, TaskCompletionMode,
};

use crate::error::McpError;
use crate::server::{ContentBlock, ToolResult};
use crate::tools::verification::{
    clear_obsolete_review_state_for_resumed_work, release_panel_lease_for_task,
};
use crate::tools::{error_result, text_result};

use super::dependencies::{
    parse_task_id_list_arg, task_has_recoverable_worker_state_blocker_text, write_string_list_field,
};
use super::epic::{
    apply_integration_conflict_cleanup, container_base_branch_for_parent,
    ensure_container_integration_worktree, read_parent_task, remove_container_integration_worktree,
    task_has_integration_conflict_recovery_marker,
};
use super::followups::resolve_promoted_followups_for_terminal_task;
use super::git_ops::{
    commit_workspace_checkpoint, current_git_head, current_workspace_root,
    ensure_worker_branch_safe_for_task,
};
use super::lifecycle::{
    caller_name, caller_role, caller_supervisor, is_container_task, is_valid_subtask,
    reconcile_dependency_states_with_task_lock, task_completion_mode_from_task,
    task_has_merge_flow_state, task_has_started_worker_execution, validate_status_transition,
};
use super::locking::acquire_task_lock;
use super::persistence::{archive_task_runtime, read_task, write_task};
use super::proof::{copy_proof_result, WorkerProofRecorder};
use super::review_gate::{
    archive_review_obligation_blockers, format_review_obligation_blockers,
    live_review_obligation_blocker,
};
use super::structured_spec::control_plane_scope_issue_for_task;

pub(super) async fn execute_archive(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(value) if !value.is_empty() => value,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let caller_role = caller_role(args);
    if caller_role != "supervisor" {
        return Ok(error_result(
            "Only supervisors can archive tasks out of the active graph.",
        ));
    }
    let recursive = args
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Archived by supervisor request");
    let blockers = archive_review_obligation_blockers(id, recursive);
    if !blockers.is_empty() {
        return Ok(error_result(format_review_obligation_blockers(
            &format!("archive task {id}"),
            &blockers,
        )));
    }
    let result = match archive_task_runtime(id, reason, recursive).await {
        Ok(result) => result,
        Err(err) => return Ok(error_result(err)),
    };
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_checkpoint(
    args: &Value,
    proof_recorder: &WorkerProofRecorder,
) -> Result<ToolResult, McpError> {
    execute_checkpoint_inner(args, true, proof_recorder).await
}

async fn execute_checkpoint_inner(
    args: &Value,
    reject_handoff_claims: bool,
    proof_recorder: &WorkerProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let message = match args.get("message").and_then(|v| v.as_str()) {
        Some(message) if !message.trim().is_empty() => message.trim(),
        _ => return Ok(error_result("Missing required parameter: message")),
    };

    let caller_role = caller_role(args);
    if caller_role != "worker" {
        return Ok(error_result(
            "Only workers can create task checkpoints. Use task action=checkpoint from the assigned worker pane.",
        ));
    }

    // Hallucinated-handoff guard. A worker whose checkpoint message asserts
    // the task is complete or in review is describing a transition that
    // never happened — `checkpoint` records a mid-task snapshot and leaves
    // status as `in_progress`. The failure mode (observed in production):
    // worker runs its plan, writes a checkpoint with "task is now complete"
    // in the message, then goes idle waiting for the supervisor to notice,
    // producing a silent deadlock until the 15-minute idle recycle fires.
    // Mirror the guard already in place for `action=progress` so the
    // wrong-verb case is caught at the MCP boundary instead of surfacing
    // as a handoff gap.
    if reject_handoff_claims {
        if let Some(phrase) = detect_hallucinated_handoff_phrase(message) {
            return Ok(error_result(format!(
            "Rejected `task action=checkpoint id={id}`: your message claims \"{phrase}\" but checkpoint does not transition task status — it only records a mid-task snapshot and leaves the task in `in_progress`. \
             If the task is actually ready for review, call `task action=complete id={id} notes=\"<summary>\" activity=testing` instead — this is the ONLY call that creates the final commit, flips status to `review_ready`, and notifies the supervisor. \
             If you are still working, remove the completion-implying language from the message (use neutral wording like \"tests passing\", \"WIP snapshot\", \"refactor complete, writing tests next\") and resubmit."
        )));
        }
    }

    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(mut task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };

    if let Some(scope_issue) = control_plane_scope_issue_for_task(&task) {
        return Ok(error_result(format!(
            "Task {id} targets live Brehon control-plane state and cannot be executed from a worker pane. {scope_issue} Ask the supervisor to handle this repair directly."
        )));
    }

    let assignee = task
        .get("assignee")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty());
    let caller_name = caller_name(args, "worker");
    if assignee != Some(caller_name.as_str()) {
        return Ok(error_result(format!(
            "Task {id} is assigned to '{}' not '{}'. Only the assigned worker can checkpoint this task.",
            assignee.unwrap_or(""),
            caller_name
        )));
    }

    let current_status = task
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    if is_terminal_task_status(&current_status) {
        return Ok(error_result(format!(
            "Task {id} is already terminal ({current_status}). Stop work, notify the supervisor if needed, and do not create a plain git commit for this task."
        )));
    }

    if let Err(err) = ensure_worker_branch_safe_for_task(id, &task) {
        return Ok(error_result(err));
    }

    let workspace = match current_workspace_root() {
        Ok(path) => path,
        Err(err) => return Ok(error_result(err)),
    };
    let (commit, created_commit) = match commit_workspace_checkpoint(&workspace, message) {
        Ok(result) => result,
        Err(err) => return Ok(error_result(err)),
    };

    task.insert("latest_commit".into(), Value::String(commit.clone()));
    task.insert(
        "updated_at".into(),
        Value::String(chrono::Utc::now().to_rfc3339()),
    );
    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Checkpoint created for task {id}, but failed to persist latest_commit."
        )));
    }

    let mut result = serde_json::json!({
        "action": "checkpointed",
        "task_id": id,
        "latest_commit": commit,
        "created_commit": created_commit,
        "message": if created_commit {
            format!("Checkpoint commit recorded for task {id}.")
        } else {
            format!("Task {id} already had a clean worktree; recorded existing HEAD.")
        }
    });
    proof_recorder
        .record_checkpoint(id, &workspace, &commit, created_commit, message)
        .await
        .attach_to_result(&mut result);

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn tool_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn parse_tool_result_json(action: &str, result: &ToolResult) -> Result<Value, String> {
    serde_json::from_str(&tool_result_text(result))
        .map_err(|err| format!("Internal error: task action={action} returned invalid JSON: {err}"))
}

fn complete_checkpoint_message(args: &Value, id: &str) -> String {
    args.get("message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            args.get("notes")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| format!("Complete {id}"))
}

fn complete_notes(args: &Value, _checkpoint_message: &str) -> String {
    args.get("notes")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            args.get("message")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "Implementation complete".to_string())
}

fn caller_owns_started_pending_task(args: &Value, task: &serde_json::Map<String, Value>) -> bool {
    let Some(assignee) = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    assignee == caller_name(args, "worker") && task_has_started_worker_execution(task)
}

fn task_has_recorded_worker_handoff_commit(task: &serde_json::Map<String, Value>) -> bool {
    task.get("latest_commit")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
}

fn supervisor_worker_state_review_recovery_allowed(
    caller_role: &str,
    current_status: &str,
    normalized_target: &str,
    task: &serde_json::Map<String, Value>,
) -> bool {
    caller_role == "supervisor"
        && normalize_task_status(current_status) == Some("blocked")
        && normalized_target == "review_ready"
        && task_has_recoverable_worker_state_blocker_text(task)
        && task_has_recorded_worker_handoff_commit(task)
}

fn apply_worker_state_review_recovery_cleanup(task: &mut serde_json::Map<String, Value>) {
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
}

pub(super) async fn execute_complete(
    args: &Value,
    proof_recorder: &WorkerProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    if caller_role(args) != "worker" {
        return Ok(error_result(
            "Only workers can complete tasks. Use task action=complete from the assigned worker pane.",
        ));
    }

    let checkpoint_message = complete_checkpoint_message(args, id);
    let notes = complete_notes(args, &checkpoint_message);
    let activity = args
        .get("activity")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("testing");

    let mut checkpoint_args = args.clone();
    let checkpoint_obj = checkpoint_args
        .as_object_mut()
        .expect("task action arguments should be an object");
    checkpoint_obj.insert("action".into(), Value::String("checkpoint".to_string()));
    checkpoint_obj.insert("message".into(), Value::String(checkpoint_message.clone()));

    let checkpoint_result =
        execute_checkpoint_inner(&checkpoint_args, false, proof_recorder).await?;
    if checkpoint_result.is_error == Some(true) {
        return Ok(checkpoint_result);
    }
    let checkpoint_json = match parse_tool_result_json("checkpoint", &checkpoint_result) {
        Ok(value) => value,
        Err(err) => return Ok(error_result(err)),
    };

    let mut progress_args = args.clone();
    let progress_obj = progress_args
        .as_object_mut()
        .expect("task action arguments should be an object");
    progress_obj.insert("action".into(), Value::String("progress".to_string()));
    progress_obj.insert("percent".into(), Value::Number(100.into()));
    progress_obj.insert("notes".into(), Value::String(notes.clone()));
    progress_obj.insert("activity".into(), Value::String(activity.to_string()));

    let progress_result = execute_progress(&progress_args, proof_recorder).await?;
    if progress_result.is_error == Some(true) {
        if let Some(mut task) = read_task(id) {
            let current_status = task
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string();
            if normalize_task_status(&current_status) == Some("blocked")
                && task_has_recoverable_worker_state_blocker_text(&task)
                && task_has_recorded_worker_handoff_commit(&task)
            {
                apply_worker_state_review_recovery_cleanup(&mut task);
                task.insert("status".into(), Value::String("review_ready".to_string()));
                task.insert(
                    "updated_at".into(),
                    Value::String(chrono::Utc::now().to_rfc3339()),
                );
                if !write_task(id, &task) {
                    return Ok(error_result(format!(
                        "Checkpoint succeeded for task {id}, but task action=complete could not persist recovered review_ready state after progress failed: {}",
                        tool_result_text(&progress_result)
                    )));
                }

                let mut result = serde_json::json!({
                    "status": "ok",
                    "action": "complete",
                    "task_id": id,
                    "task_status": "review_ready",
                    "auto_review": true,
                    "recovered_handoff": true,
                    "notes": notes,
                    "message": format!(
                        "Task {id} checkpointed and recovered from blocked handoff to review_ready."
                    ),
                });
                if let Some(value) = checkpoint_json.get("created_commit") {
                    result["created_commit"] = value.clone();
                }
                if let Some(value) = checkpoint_json
                    .get("latest_commit")
                    .or_else(|| task.get("latest_commit"))
                {
                    result["latest_commit"] = value.clone();
                }
                copy_proof_result(&mut result, &Value::Null, &checkpoint_json);
                result["worktree_cleanup"] =
                    super::build_artifact_cleanup::cleanup_current_worker_build_artifacts(
                        "after_task_complete",
                    );
                return Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ));
            }
        }
        return Ok(error_result(format!(
            "Checkpoint succeeded for task {id}, but task action=complete could not move it to review_ready: {}",
            tool_result_text(&progress_result)
        )));
    }
    let progress_json = match parse_tool_result_json("progress", &progress_result) {
        Ok(value) => value,
        Err(err) => return Ok(error_result(err)),
    };
    let final_status = progress_json
        .get("task_status")
        .and_then(|value| value.as_str())
        .unwrap_or("review_ready");
    let already_handed_off = progress_json
        .get("already_handed_off")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let message = match final_status {
        "review_ready" if already_handed_off => {
            format!("Task {id} checkpointed. It was already ready for review.")
        }
        "in_review" => format!("Task {id} checkpointed. Review is already in progress."),
        _ => format!(
            "Task {id} checkpointed and moved to review_ready. The supervisor will be notified."
        ),
    };

    let mut result = serde_json::json!({
        "status": "ok",
        "action": "complete",
        "task_id": id,
        "task_status": final_status,
        "auto_review": progress_json
            .get("auto_review")
            .and_then(|value| value.as_bool())
            .unwrap_or(final_status == "review_ready"),
        "notes": notes,
        "message": message,
    });

    if let Some(value) = checkpoint_json.get("created_commit") {
        result["created_commit"] = value.clone();
    }
    if let Some(value) = checkpoint_json
        .get("latest_commit")
        .or_else(|| progress_json.get("latest_commit"))
    {
        result["latest_commit"] = value.clone();
    }
    if let Some(value) = progress_json.get("warning") {
        result["warning"] = value.clone();
    }
    copy_proof_result(&mut result, &progress_json, &checkpoint_json);
    if already_handed_off {
        result["already_handed_off"] = Value::Bool(true);
    }
    result["worktree_cleanup"] =
        super::build_artifact_cleanup::cleanup_current_worker_build_artifacts(
            "after_task_complete",
        );

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_progress(
    args: &Value,
    proof_recorder: &WorkerProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(mut task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };

    if caller_role(args) == "worker" {
        if let Some(scope_issue) = control_plane_scope_issue_for_task(&task) {
            return Ok(error_result(format!(
                "Task {id} targets live Brehon control-plane state and cannot be executed from a worker pane. {scope_issue} Ask the supervisor to handle this repair directly."
            )));
        }
    }

    let current_status = task
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task")
        .to_string();
    let completion_mode = task_completion_mode_from_task(&task);
    let percent = args.get("percent").and_then(|v| v.as_i64()).unwrap_or(0);
    let caller_role = caller_role(args);
    let normalized_status = normalize_task_status(&current_status);
    let resume_started_pending_task = caller_role == "worker"
        && task_type == "task"
        && matches!(normalized_status, Some("pending"))
        && caller_owns_started_pending_task(args, &task);
    let effective_current_status = if resume_started_pending_task {
        "assigned"
    } else {
        current_status.as_str()
    };
    let effective_normalized_status = normalize_task_status(effective_current_status);
    let raw_notes = args.get("notes").and_then(|v| v.as_str()).unwrap_or("");

    // Hallucinated-handoff guard. A worker reporting <100% whose notes claim
    // the task is complete or in review is describing a transition that never
    // happened — the old failure mode where a worker typed "task is now in
    // review" into progress notes and then went idle waiting for a verdict on
    // a task still persisted as `changes_requested`/`in_progress`. Reject
    // loudly and point at the action that actually performs the handoff.
    if caller_role == "worker" && task_type == "task" && percent < 100 {
        if let Some(phrase) = detect_hallucinated_handoff_phrase(raw_notes) {
            return Ok(error_result(format!(
                "Rejected `task action=progress id={id} percent={percent}`: your notes claim \"{phrase}\" but percent is {percent}. Progress notes do not transition task status. \
                 If the task is actually ready for review, call `task action=complete id={id} notes=\"<summary>\" activity=testing` — this is the ONLY call that checkpoints, flips status to review_ready, and notifies the supervisor. \
                 If you are still working, remove the completion-implying language from your notes and resubmit with percent={percent}."
            )));
        }
    }

    if caller_role == "worker"
        && percent >= 100
        && task_type == "task"
        && matches!(normalized_status, Some("review_ready" | "in_review"))
    {
        let mut result = serde_json::json!({
            "status": "ok",
            "task_id": id,
            "percent": percent,
            "task_status": current_status,
            "already_handed_off": true,
            "message": match normalized_status {
                Some("review_ready") =>
                    format!("Task {id} is already ready for review. Keeping status review_ready."),
                Some("in_review") =>
                    format!("Task {id} is already in review. Keeping status in_review."),
                _ => unreachable!("status already matched review handoff states"),
            }
        });
        if matches!(normalized_status, Some("review_ready")) {
            result["auto_review"] = Value::Bool(true);
        }
        if let Some(value) = task.get("latest_commit") {
            result["latest_commit"] = value.clone();
        }
        proof_recorder
            .record_progress(
                args,
                id,
                percent,
                task.get("latest_commit").and_then(|value| value.as_str()),
                true,
            )
            .await
            .attach_to_result(&mut result);
        result["worktree_cleanup"] =
            super::build_artifact_cleanup::cleanup_current_worker_build_artifacts(
                "after_task_complete",
            );
        return Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ));
    }

    // Validate transition to in_progress (allowed from assigned, in_progress,
    // changes_requested)
    if effective_current_status != "in_progress" {
        if let Err(msg) = validate_status_transition(
            effective_current_status,
            "in_progress",
            &caller_role,
            &task_type,
            completion_mode,
        ) {
            return Ok(error_result(format!("Task {id}: {msg}")));
        }
    }

    task.insert("percent".into(), Value::Number(percent.into()));
    if let Some(activity) = args.get("activity").and_then(|v| v.as_str()) {
        task.insert("activity".into(), Value::String(activity.to_string()));
    }
    if let Some(notes) = args.get("notes").and_then(|v| v.as_str()) {
        task.insert("notes".into(), Value::String(notes.to_string()));
    }
    let current_commit = if caller_role == "worker" {
        if let Err(err) = ensure_worker_branch_safe_for_task(id, &task) {
            return Ok(error_result(err));
        }
        current_git_head()
    } else {
        None
    };
    if let Some(ref commit) = current_commit {
        task.insert("latest_commit".into(), Value::String(commit.clone()));
    }

    // Auto-transition: a worker reporting 100% from an active work gate
    // should enter review in the same call. Without this, a rereview fix
    // can strand at 100% in_progress after changes_requested.
    if caller_role == "worker"
        && matches!(
            normalize_task_status(&current_status),
            Some("changes_requested")
        )
    {
        if let Err(err) = clear_obsolete_review_state_for_resumed_work(id) {
            return Ok(error_result(format!(
                "Failed to clear obsolete review state for task {id}: {err}"
            )));
        }
    }
    let review_ready_gate = matches!(
        effective_normalized_status,
        Some("assigned" | "in_progress" | "changes_requested")
    );
    let auto_review =
        percent >= 100 && caller_role == "worker" && task_type == "task" && review_ready_gate;

    if auto_review {
        let review_owner = task
            .get("assignee")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .or_else(|| {
                let caller = caller_name(args, "worker");
                (!caller.trim().is_empty()).then_some(caller)
            });
        task.insert("status".into(), Value::String("review_ready".to_string()));
        if let Some(owner) = review_owner {
            task.insert("assignee".into(), Value::String(owner.clone()));
            task.insert("review_owner".into(), Value::String(owner));
        }
    } else {
        task.insert("status".into(), Value::String("in_progress".to_string()));
    }
    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Failed to persist progress update for task {id}"
        )));
    }

    let mut result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "percent": percent
    });
    if let Some(ref commit) = current_commit {
        result["latest_commit"] = Value::String(commit.clone());
    }

    if auto_review {
        result["auto_review"] = Value::Bool(true);
        result["task_status"] = Value::String("review_ready".to_string());
        result["message"] = Value::String(format!(
            "Task {id} is automatically marked ready for review (100% complete). \
             You do not need to take further action — the supervisor will be notified."
        ));

        // Best-effort notify supervisor
        let agent_name = caller_name(args, "worker");
        let supervisor = caller_supervisor(args);
        let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("?");
        let commit_line = current_commit
            .as_ref()
            .map(|commit| format!(" Commit: {commit}."))
            .unwrap_or_default();
        let msg = format!(
            "Task {id} (\"{title}\") is complete and ready for review. \
             Worker: {agent_name}. Status auto-transitioned to review_ready.{commit_line}"
        );
        if supervisor.is_empty() {
            result["warning"] = Value::String(
                "Task moved to review_ready, but no live supervisor session could be resolved for notification."
                    .to_string(),
            );
        } else {
            let _ = crate::tools::agent::try_deliver_message(&supervisor, &agent_name, &msg);
        }
        result["worktree_cleanup"] =
            super::build_artifact_cleanup::cleanup_current_worker_build_artifacts(
                "after_task_complete",
            );
    }
    if caller_role == "worker" && task_type == "task" {
        proof_recorder
            .record_progress(args, id, percent, current_commit.as_deref(), auto_review)
            .await
            .attach_to_result(&mut result);
    }

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_update(
    args: &Value,
    proof_recorder: &WorkerProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(mut task) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };
    let caller_role = caller_role(args);

    if let Some(raw_mode) = args.get("completion_mode").and_then(|v| v.as_str()) {
        if caller_role != "supervisor" {
            return Ok(error_result("Only supervisors can change completion_mode."));
        }
        let Some(parsed_mode) = parse_task_completion_mode(raw_mode) else {
            return Ok(error_result(
                "Invalid completion_mode. Expected one of: merge, close",
            ));
        };
        let task_type = task
            .get("task_type")
            .and_then(|v| v.as_str())
            .unwrap_or("task");
        let normalized_mode = if is_container_task(task_type) {
            TaskCompletionMode::Close
        } else {
            parsed_mode
        };
        let current_mode = task_completion_mode_from_task(&task);
        if current_mode == TaskCompletionMode::Merge
            && normalized_mode == TaskCompletionMode::Close
            && task_has_merge_flow_state(&task)
        {
            return Ok(error_result(format!(
                "Task {id} already has merge-flow state (merge_target, integration, or commit metadata). \
                 It cannot be switched from completion_mode='merge' to 'close'. Resolve it through the review/integration workflow instead."
            )));
        }
        task.insert(
            "completion_mode".into(),
            Value::String(normalized_mode.as_str().to_string()),
        );
    }
    let completion_mode = task_completion_mode_from_task(&task);

    // If status is being changed, enforce the state machine
    let mut worker_state_review_recovery = false;
    let normalized_status = if let Some(new_status) = args.get("status").and_then(|v| v.as_str()) {
        let current_status = task
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let task_type = task
            .get("task_type")
            .and_then(|v| v.as_str())
            .unwrap_or("task");
        let Some(normalized) = normalize_task_status(new_status) else {
            return Ok(error_result(format!("Invalid status value: {new_status}")));
        };
        if normalized == "assigned" && !is_container_task(task_type) {
            return Ok(error_result(format!(
                "Task {id} cannot be set to 'assigned' via task action=update. \
                 Use factory action=assign_workers instead."
            )));
        }
        if normalized == "in_review" && !is_container_task(task_type) {
            return Ok(error_result(format!(
                "Task {id} cannot be set to 'in_review' via task action=update. \
                 Use verification action=request_review instead."
            )));
        }
        if normalized == "approved" && !is_container_task(task_type) {
            return Ok(error_result(format!(
                "Task {id} cannot be set to 'approved' via task action=update. \
                 Use verification action=request_review and wait for reviewer approvals."
            )));
        }
        if normalized == "pending" && !is_container_task(task_type) {
            if let Some(blocker) = live_review_obligation_blocker(&task) {
                return Ok(error_result(format!(
                    "Task {id} cannot be reset to 'pending' because it has review obligations: {}. \
                     Resolve review through reviewer submissions, changes_requested rework, reset_rounds, or a negative override.",
                    blocker.summary()
                )));
            }
        }
        if normalized == "merged"
            && !is_container_task(task_type)
            && task_has_merge_flow_state(&task)
        {
            return Ok(error_result(format!(
                "Task {id} has merge-flow state and cannot be set to 'merged' via task action=update. \
                 Use task action=integrate for epic-bound subtasks, or task action=close only for direct-to-main merge flow."
            )));
        }

        let integration_conflict_review_recovery = caller_role == "supervisor"
            && normalize_task_status(current_status) == Some("blocked")
            && normalized == "review_ready"
            && task_has_integration_conflict_recovery_marker(&task);
        worker_state_review_recovery = supervisor_worker_state_review_recovery_allowed(
            &caller_role,
            current_status,
            &normalized,
            &task,
        );
        if normalize_task_status(current_status) == Some("blocked")
            && normalized == "review_ready"
            && task_has_recoverable_worker_state_blocker_text(&task)
            && !worker_state_review_recovery
        {
            if caller_role != "supervisor" {
                return Ok(error_result(format!(
                    "Task {id} has a recoverable blocked worker handoff, but only the supervisor may recover it. Supervisor next action: task action=recover_handoff id={id}."
                )));
            }
            if !task_has_recorded_worker_handoff_commit(&task) {
                return Ok(error_result(format!(
                    "Task {id} has a recoverable blocked worker handoff marker, but Brehon cannot safely move it to review_ready because latest_commit is empty. Inspect the worker pane/proof, rerun or reassign the worker to create a checkpoint commit, then call task action=ready again."
                )));
            }
        }
        if !integration_conflict_review_recovery && !worker_state_review_recovery {
            if let Err(msg) = validate_status_transition(
                current_status,
                new_status,
                &caller_role,
                task_type,
                completion_mode,
            ) {
                return Ok(error_result(format!("Task {id}: {msg}")));
            }
        }
        Some(normalized.to_string())
    } else {
        None
    };

    let mut updated_fields = Vec::new();
    let new_status_value = if let Some(status) = normalized_status {
        if status == "review_ready" && task_has_integration_conflict_recovery_marker(&task) {
            apply_integration_conflict_cleanup(&mut task);
            updated_fields.push("integration_conflict");
        }
        if worker_state_review_recovery {
            apply_worker_state_review_recovery_cleanup(&mut task);
            updated_fields.push("blockers");
            updated_fields.push("percent");
            updated_fields.push("activity");
            updated_fields.push("assignee");
            updated_fields.push("review_owner");
            updated_fields.push("inbox_delivered");
        }
        task.insert("status".into(), Value::String(status.clone()));
        updated_fields.push("status");
        Some(status)
    } else {
        None
    };

    // Handle integration_branch for container tasks, treat empty as unset
    if let Some(branch) = args.get("integration_branch").and_then(|v| v.as_str()) {
        let task_type = task
            .get("task_type")
            .and_then(|v| v.as_str())
            .unwrap_or("task");
        if !is_container_task(task_type) {
            return Ok(error_result(
                "integration_branch can only be set on initiatives or epics",
            ));
        }
        let parent_task = read_parent_task(&task);
        let base_branch = match container_base_branch_for_parent(task_type, parent_task.as_ref()) {
            Ok(branch) => branch,
            Err(err) => return Ok(error_result(err)),
        };
        let existing_worktree = task
            .get("integration_worktree")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let existing_branch = task
            .get("integration_branch")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        if branch.is_empty() {
            if let Some(path) = existing_worktree.as_deref() {
                if let Err(err) = remove_container_integration_worktree(path).await {
                    return Ok(error_result(err));
                }
            }
            task.remove("integration_branch");
            task.remove("integration_worktree");
        } else {
            if existing_branch.as_deref() != Some(branch) {
                if let Some(path) = existing_worktree.as_deref() {
                    if let Err(err) = remove_container_integration_worktree(path).await {
                        return Ok(error_result(err));
                    }
                }
            }
            let worktree = match ensure_container_integration_worktree(
                id,
                task_type,
                branch,
                existing_worktree.as_deref().and_then(|p| p.to_str()),
                true,
                false,
                Some(&base_branch),
            )
            .await
            {
                Ok(path) => path,
                Err(err) => return Ok(error_result(err)),
            };
            task.insert(
                "integration_branch".into(),
                Value::String(branch.to_string()),
            );
            task.insert(
                "integration_worktree".into(),
                Value::String(worktree.to_string_lossy().to_string()),
            );
        }
        updated_fields.push("integration_branch");
        updated_fields.push("integration_worktree");
    }

    // Handle merge_target - only for valid subtasks (parent_id references existing epic)
    if let Some(target) = args.get("merge_target").and_then(|v| v.as_str()) {
        if !is_valid_subtask(&task) {
            return Ok(error_result(
                "merge_target can only be set on subtasks with valid parent_id",
            ));
        }
        if target.is_empty() {
            task.remove("merge_target");
        } else {
            task.insert("merge_target".into(), Value::String(target.to_string()));
        }
        updated_fields.push("merge_target");
    }

    // Handle integration_status - only for valid subtasks, validate values
    if let Some(status) = args.get("integration_status").and_then(|v| v.as_str()) {
        if !is_valid_subtask(&task) {
            return Ok(error_result(
                "integration_status can only be set on subtasks with valid parent_id",
            ));
        }
        let valid_status = matches!(status, "pending" | "integrated" | "not_applicable");
        if !valid_status {
            return Ok(error_result(format!(
                "Invalid integration_status '{}'. Must be one of: pending, integrated, not_applicable",
                status
            )));
        }
        task.insert(
            "integration_status".into(),
            Value::String(status.to_string()),
        );
        updated_fields.push("integration_status");
    }

    for field in &[
        "title",
        "description",
        "priority",
        "notes",
        "activity",
        "blockers",
        "assignee",
        "parent_id",
    ] {
        if let Some(val) = args.get(*field).and_then(|v| v.as_str()) {
            task.insert((*field).to_string(), Value::String(val.to_string()));
            updated_fields.push(*field);
        }
    }
    if args.get("completion_mode").is_some() {
        updated_fields.push("completion_mode");
    }
    if let Some(policy) = args.get("execution_policy") {
        if !policy.is_object() {
            return Ok(error_result(
                "Invalid execution_policy. Expected an object.",
            ));
        }
        task.insert("execution_policy".to_string(), policy.clone());
        updated_fields.push("execution_policy");
    }
    if args.get("dependencies").is_some() {
        let dependencies = match parse_task_id_list_arg(args, "dependencies") {
            Ok(items) => items,
            Err(err) => return Ok(error_result(err)),
        };
        write_string_list_field(&mut task, "dependencies", &dependencies);
        updated_fields.push("dependencies");
    }
    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Failed to persist task update for {id}"
        )));
    }
    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }
    let released_panel = if new_status_value
        .as_deref()
        .is_some_and(is_terminal_task_status)
    {
        match release_panel_lease_for_task(id) {
            Ok(panel_id) => panel_id,
            Err(err) => return Ok(error_result(err)),
        }
    } else {
        None
    };
    if new_status_value
        .as_deref()
        .is_some_and(is_terminal_task_status)
    {
        if let Err(err) = resolve_promoted_followups_for_terminal_task(id, &task).await {
            return Ok(error_result(err));
        }
    }

    // Notify supervisor on critical status transitions
    if let Some(ref new_status) = new_status_value {
        let needs_notification = matches!(
            new_status.as_str(),
            "in_review" | "blocked" | "merged" | "closed"
        );
        if needs_notification && caller_role != "supervisor" {
            let agent_name = caller_name(args, "worker");
            let supervisor = caller_supervisor(args);
            let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let msg =
                format!("Task {id} (\"{title}\") status changed to {new_status} by {agent_name}.");
            if !supervisor.is_empty() {
                let _ = crate::tools::agent::try_deliver_message(&supervisor, &agent_name, &msg);
            }
        }
    }

    let proof_outcome = if caller_role == "worker" {
        Some(
            proof_recorder
                .record_update(args, id, &updated_fields)
                .await,
        )
    } else {
        None
    };
    let mut result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "updated_fields": updated_fields,
        "released_panel": released_panel
    });
    if let Some(outcome) = proof_outcome {
        outcome.attach_to_result(&mut result);
    }

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

/// Detect progress-note prose that claims the task has been handed off to
/// review or completed, when the caller has not actually reported percent=100.
/// Returns the matched phrase so the rejection message can quote it back.
///
/// The phrase set is intentionally narrow — we match only language that
/// unambiguously asserts *current* completion or review-readiness. Matches
/// that are immediately preceded by future-tense / negation / conditional
/// markers ("will be", "once", "when", "not", "if", "no", etc.) are treated
/// as aspirational and ignored, so "will be ready for review after tests
/// pass" does not trigger the rejection.
pub(super) fn detect_hallucinated_handoff_phrase(notes: &str) -> Option<&'static str> {
    if notes.is_empty() {
        return None;
    }
    let lower = notes.to_ascii_lowercase();
    const PHRASES: &[&str] = &[
        "task is now in review",
        "task is in review",
        "now in review",
        "is now in review",
        "ready for review",
        "ready for re-review",
        "ready for rereview",
        "task is now complete",
        "task is complete",
        "task complete",
        "is now complete",
    ];
    for phrase in PHRASES {
        if let Some(idx) = lower.find(phrase) {
            if !match_is_negated_or_aspirational(&lower, idx) {
                return Some(phrase);
            }
        }
    }
    None
}

/// Return `true` when the phrase match at `phrase_start` is preceded by
/// language that reframes it as aspirational, conditional, or negated.
/// Scans back up to ~40 characters — enough to catch "will be ready",
/// "once tests pass and ready for review", "not yet ready for review",
/// "if ready for review", etc., while cheap to evaluate.
fn match_is_negated_or_aspirational(lower: &str, phrase_start: usize) -> bool {
    const LOOKBACK_CHARS: usize = 40;
    const MARKERS: &[&str] = &[
        "will be ",
        "will ",
        "'ll be ",
        "'ll ",
        "once ",
        "when ",
        "after ",
        "before ",
        "until ",
        "if ",
        "unless ",
        "not ",
        "n't ",
        "no ",
        "never ",
        "gonna ",
        "going to ",
        "soon to be ",
        "about to be ",
        "almost ",
        "nearly ",
    ];
    let start = phrase_start.saturating_sub(LOOKBACK_CHARS);
    let window = &lower[start..phrase_start];
    MARKERS.iter().any(|marker| window.contains(marker))
}
