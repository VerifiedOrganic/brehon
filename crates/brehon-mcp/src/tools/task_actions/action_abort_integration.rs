//! Handler for the "abort-integration" task action.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result};

use super::epic::{apply_integration_conflict_cleanup, read_parent_task};
use super::git_ops::{
    git_run_ok_in, git_status_porcelain_in, git_stdout_in, non_brehon_status_entries,
};
use super::integration_proof::{IntegrationAbortProof, IntegrationProofRecorder};
use super::integration_state::{
    read_integration_state, validate_raw_integration_phase, write_integration_state,
    IntegrationPhase, Resolution,
};
use super::lifecycle::{
    caller_name, caller_role, caller_supervisor, reconcile_dependency_states_with_task_lock,
};
use super::locking::{acquire_repo_lock, acquire_task_lock};
use super::paths::{ensure_brehon_worktree_path, resolve_project_path};
use super::persistence::{read_task, write_task};

pub(super) async fn execute(
    args: &Value,
    proof_recorder: &IntegrationProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };
    let reason = match args
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(reason) => reason,
        None => return Ok(error_result("Missing required parameter: reason")),
    };

    let _task_lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(task_data) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };

    let caller_role = caller_role(args);
    if caller_role != "supervisor" {
        let agent_name = caller_name(args, "worker");
        let supervisor = caller_supervisor(args);
        let title = task_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let notify_msg = format!(
            "Task {id} (\"{title}\") has integration state that requires supervisor intervention. \
             {agent_name} attempted task action=abort-integration, but only supervisors can abort \
             persistent epic-worktree integration state. Please run:\n  \
             task action=abort-integration id={id} reason=\"...\""
        );
        if !supervisor.is_empty() {
            let _ = crate::tools::agent::try_deliver_message(&supervisor, &agent_name, &notify_msg);
        }
        return Ok(error_result(
            "Only supervisors can abort integration in an epic worktree.",
        ));
    }

    if let Err(err) = validate_raw_integration_phase(id, &task_data) {
        return Ok(error_result(err));
    }

    let current_state = read_integration_state(&task_data);
    let current_phase = current_state.phase;
    let current_phase_str = current_phase.as_str();
    if matches!(
        current_phase,
        IntegrationPhase::Complete | IntegrationPhase::Aborted
    ) {
        let result = serde_json::json!({
            "status": "ok",
            "action": "abort-integration",
            "task_id": id,
            "noop": true,
            "integration_phase": current_phase_str,
            "task_status": task_data
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            "reason": reason,
            "message": format!(
                "Task {} integration phase is '{}' so abort-integration was a no-op.",
                id, current_phase_str
            )
        });
        return Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ));
    }

    let _repo_lock = match acquire_repo_lock().await {
        Ok(lock) => lock,
        Err(err) => {
            return Ok(error_result(format!(
                "Failed to acquire repository integration lock: {err}"
            )));
        }
    };

    let (merge_target, integration_worktree) = match resolve_integration_worktree(id, &task_data) {
        Ok(value) => value,
        Err(_err) if current_phase == IntegrationPhase::Null => {
            let result = serde_json::json!({
                "status": "ok",
                "action": "abort-integration",
                "task_id": id,
                "noop": true,
                "integration_phase": current_phase_str,
                "task_status": task_data
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
                "reason": reason,
                "message": format!(
                    "Task {} integration phase is '{}' so abort-integration was a no-op.",
                    id, current_phase_str
                )
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        }
        Err(err) => return Ok(error_result(err)),
    };
    let epic_branch_tip = match git_stdout_in(&integration_worktree, &["rev-parse", "HEAD"]) {
        Ok(value) => value,
        Err(err) => {
            return Ok(error_result(format!(
                "Failed to read current tip of integration branch '{merge_target}' in '{}': {err}",
                integration_worktree.display()
            )));
        }
    };

    let cleanup_action = match cleanup_integration_worktree(
        &integration_worktree,
        &merge_target,
        &epic_branch_tip,
    ) {
        Ok(action) => action,
        Err(err) => return Ok(error_result(err)),
    };

    if current_phase == IntegrationPhase::Null {
        if cleanup_action == "none" {
            let result = serde_json::json!({
                "status": "ok",
                "action": "abort-integration",
                "task_id": id,
                "noop": true,
                "integration_phase": current_phase_str,
                "task_status": task_data
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown"),
                "reason": reason,
                "message": format!(
                    "Task {} integration phase is '{}' so abort-integration was a no-op.",
                    id, current_phase_str
                )
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        }

        let result = serde_json::json!({
            "status": "ok",
            "action": "abort-integration",
            "task_id": id,
            "noop": false,
            "integration_phase": current_phase_str,
            "task_status": task_data
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            "merge_target": merge_target,
            "integration_worktree": integration_worktree.to_string_lossy().to_string(),
            "cleanup_action": cleanup_action,
            "epic_branch_tip": epic_branch_tip,
            "reason": reason,
            "message": format!(
                "Cleared stale integration worktree state for task {} without changing its null integration phase.",
                id
            )
        });
        return Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut task = task_data;
    apply_integration_conflict_cleanup(&mut task);
    task.insert("status".into(), Value::String("approved".to_string()));
    if task.get("integration_status").is_some() {
        task.insert(
            "integration_status".into(),
            Value::String("pending".to_string()),
        );
    }
    task.insert("updated_at".into(), Value::String(now.clone()));

    let mut aborted_state = read_integration_state(&task);
    let conflicts_before_clear = aborted_state.conflicting_files.clone();
    aborted_state.phase = IntegrationPhase::Aborted;
    aborted_state.epic_branch = merge_target.clone();
    aborted_state.worktree_path = integration_worktree.to_string_lossy().to_string();
    aborted_state.conflicting_files.clear();
    aborted_state.last_transition_at = now.clone();
    aborted_state.resolution = Some(Resolution {
        kind: "manual_abort".to_string(),
        reason: reason.to_string(),
        resolved_at: now.clone(),
    });
    write_integration_state(&mut task, &aborted_state);

    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Abort cleanup succeeded in '{}', but failed to persist task {id}.",
            integration_worktree.display()
        )));
    }
    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }

    let mut result = serde_json::json!({
        "status": "ok",
        "action": "abort-integration",
        "task_id": id,
        "noop": false,
        "integration_phase": IntegrationPhase::Aborted.as_str(),
        "task_status": "approved",
        "merge_target": merge_target,
        "integration_worktree": integration_worktree.to_string_lossy().to_string(),
        "cleanup_action": cleanup_action,
        "epic_branch_tip": epic_branch_tip,
        "reason": reason,
        "message": format!(
            "Aborted integration for task {} and restored it to approved state.",
            id
        )
    });

    let worktree_string = integration_worktree.to_string_lossy().to_string();
    let source_branch = task
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let proof_outcome = proof_recorder
        .record_abort(IntegrationAbortProof {
            task_id: id,
            source_branch,
            target_branch: Some(merge_target.as_str()),
            worktree_path: Some(worktree_string.as_str()),
            reason,
            conflicts: conflicts_before_clear,
        })
        .await;
    proof_outcome.attach_to_result(&mut result);

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn resolve_integration_worktree(
    task_id: &str,
    task_data: &serde_json::Map<String, Value>,
) -> Result<(String, PathBuf), String> {
    let merge_target = task_data
        .get("merge_target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "Task {task_id} has no merge_target; abort-integration requires a feature-epic subtask."
            )
        })?
        .to_string();
    let parent_task = read_parent_task(task_data).ok_or_else(|| {
        format!(
            "Task {task_id} is missing its parent epic metadata; abort-integration cannot locate the integration worktree."
        )
    })?;
    let recorded_path = parent_task
        .get("integration_worktree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "Task {task_id} cannot abort integration because no integration worktree path was recorded on its parent epic."
            )
        })?;
    let resolved_path = resolve_project_path(Path::new(recorded_path)).ok_or_else(|| {
        format!(
            "Task {task_id} cannot abort integration because the recorded integration worktree path '{}' could not be resolved.",
            recorded_path
        )
    })?;
    if !resolved_path.exists() {
        return Err(format!(
            "Task {task_id} cannot abort integration because integration worktree '{}' does not exist.",
            resolved_path.display()
        ));
    }
    let resolved_path = ensure_brehon_worktree_path(&resolved_path, "abort-integration worktree")?;
    Ok((merge_target, resolved_path))
}

fn cherry_pick_head_path(worktree: &Path) -> Result<PathBuf, String> {
    let path = git_stdout_in(worktree, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"])?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    })
}

fn cleanup_integration_worktree(
    integration_worktree: &Path,
    merge_target: &str,
    epic_branch_tip: &str,
) -> Result<String, String> {
    ensure_brehon_worktree_path(integration_worktree, "abort-integration cleanup worktree")?;
    let cherry_pick_head_exists = cherry_pick_head_path(integration_worktree)
        .map(|path| path.exists())
        .unwrap_or(false);
    let status_before = git_status_porcelain_in(integration_worktree).map_err(|err| {
        format!(
            "Failed to inspect integration worktree '{}': {err}",
            integration_worktree.display()
        )
    })?;
    let worktree_dirty = !non_brehon_status_entries(&status_before).is_empty();

    let cleanup_action = if cherry_pick_head_exists {
        git_run_ok_in(integration_worktree, &["cherry-pick", "--abort"]).map_err(|err| {
            format!(
                "Failed to abort cherry-pick in integration worktree '{}': {err}",
                integration_worktree.display()
            )
        })?;
        "git cherry-pick --abort".to_string()
    } else if worktree_dirty {
        git_run_ok_in(integration_worktree, &["reset", "--hard", epic_branch_tip]).map_err(
            |err| {
                format!(
                    "Failed to reset dirty integration worktree '{}' to branch tip {}: {err}",
                    integration_worktree.display(),
                    epic_branch_tip
                )
            },
        )?;
        format!("git reset --hard {epic_branch_tip}")
    } else {
        "none".to_string()
    };

    let status_after = git_status_porcelain_in(integration_worktree).map_err(|err| {
        format!(
            "Failed to inspect integration worktree '{}' after cleanup: {err}",
            integration_worktree.display()
        )
    })?;
    let remaining_changes = non_brehon_status_entries(&status_after);
    if !remaining_changes.is_empty() {
        return Err(format!(
            "Integration worktree '{}' is still dirty after abort-integration cleanup: {}",
            integration_worktree.display(),
            remaining_changes.join(", ")
        ));
    }
    if cherry_pick_head_path(integration_worktree)
        .map(|path| path.exists())
        .unwrap_or(false)
    {
        return Err(format!(
            "Integration worktree '{}' still has stale cherry-pick state after cleanup on branch '{}'.",
            integration_worktree.display(),
            merge_target
        ));
    }

    Ok(cleanup_action)
}
