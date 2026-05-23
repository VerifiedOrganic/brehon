//! Handler for the "close" task action.

use serde_json::Value;

use brehon_types::{is_terminal_task_status, normalize_task_status, TaskCompletionMode};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::verification::{
    commits_refer_to_same_oid, delete_review_state, release_panel_lease_for_task,
};
use crate::tools::{error_result, text_result};

use super::epic::{
    check_child_completion, check_epic_integration_status,
    check_initiative_epic_integration_status, container_base_branch_for_task,
    container_target_worktree_for_task, ensure_container_integration_worktree,
    merge_container_branch_into_target_with_strategy, read_current_review_request,
    read_parent_task, verify_container_branch_ready, verify_merge_ready, ContainerMergeStrategy,
};
use super::followups::{
    collect_container_open_followup_blockers, resolve_promoted_followups_for_terminal_task,
};
use super::git_ops::{detect_default_branch, dirty_primary_checkout_terminal_blocker};
use super::lifecycle::{
    caller_name, caller_role, caller_supervisor, child_collection_label,
    clear_terminal_task_ownership, close_terminal_status, is_container_task, is_epic,
    is_initiative, reconcile_dependency_states_with_task_lock, task_completion_mode_from_task,
    validate_status_transition,
};
use super::locking::{acquire_repo_lock, acquire_task_lock};
use super::persistence::{read_task, write_task};
use super::review_gate::{
    archived_review_obligation_blockers_under, format_review_obligation_blockers,
};
use super::{enqueue_worker_session_recycle_surfacing, terminal_worker_recycle_candidate};

pub(super) async fn execute(args: &Value) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    let _lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(task_data) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };

    let task_type = task_data
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task")
        .to_string();
    let parent_id = task_data
        .get("parent_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    if let Some(err) = dirty_primary_checkout_terminal_blocker(&format!("close {task_type} {id}")) {
        return Ok(error_result(err));
    }

    if is_container_task(&task_type) {
        let blockers = archived_review_obligation_blockers_under(id);
        if !blockers.is_empty() {
            return Ok(error_result(format_review_obligation_blockers(
                &format!("close {task_type} {id}"),
                &blockers,
            )));
        }
    }

    // Prevent closing a container with open direct children.
    if is_container_task(&task_type) {
        let (total, closed, all_done) = check_child_completion(id);
        if total > 0 && !all_done {
            let child_label = child_collection_label(&task_type);
            return Ok(error_result(format!(
                "Cannot close {task_type} {id}: {closed}/{total} {child_label} closed. \
                 Close all {child_label} first."
            )));
        }

        // For feature epics (with integration_branch), verify all subtasks are integrated
        // The actual merge happens after supervisor check below
        let integration_branch = task_data.get("integration_branch").and_then(|v| v.as_str());
        if let Some(branch) = integration_branch {
            if !branch.is_empty() {
                let (total_children, integrated, missing) = if is_epic(&task_type) {
                    check_epic_integration_status(id)
                } else {
                    check_initiative_epic_integration_status(id)
                };
                if total_children > 0 {
                    if integrated < total_children {
                        let missing_label = if is_epic(&task_type) {
                            "Subtasks"
                        } else {
                            "Epics"
                        };
                        return Ok(error_result(format!(
                            "Cannot close {task_type} {id}: {integrated}/{total_children} children integrated. \
                             {missing_label} not yet integrated: {}.",
                            missing.join(", ")
                        )));
                    }

                    let base_branch = match container_base_branch_for_task(&task_data) {
                        Ok(branch) => branch,
                        Err(err) => return Ok(error_result(err)),
                    };
                    let integration_worktree = match ensure_container_integration_worktree(
                        id,
                        &task_type,
                        branch,
                        task_data
                            .get("integration_worktree")
                            .and_then(|v| v.as_str())
                            .filter(|v| !v.is_empty()),
                        false,
                        false,
                        Some(&base_branch),
                    )
                    .await
                    {
                        Ok(path) => path,
                        Err(err) => return Ok(error_result(err)),
                    };

                    if let Err(err) = verify_container_branch_ready(
                        id,
                        &task_type,
                        branch,
                        &integration_worktree,
                        &base_branch,
                    ) {
                        return Ok(error_result(err));
                    }
                }
            }
        }
    }

    let current_status = task_data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let completion_mode = task_completion_mode_from_task(&task_data);
    let has_merge_target = task_data
        .get("merge_target")
        .and_then(|v| v.as_str())
        .is_some_and(|value| !value.trim().is_empty());
    let has_latest_commit = task_data
        .get("latest_commit")
        .and_then(|v| v.as_str())
        .is_some_and(|value| !value.trim().is_empty());
    let is_phase_gate = task_data
        .get("plan_import")
        .and_then(|value| value.get("is_phase_gate"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let caller_role = caller_role(args);

    if !is_container_task(&task_type)
        && completion_mode == TaskCompletionMode::Close
        && has_merge_target
        && (has_latest_commit || is_phase_gate)
        && normalize_task_status(&current_status) != Some("closed")
    {
        return Ok(error_result(format!(
            "Task {id} is marked completion_mode='close' but still carries merge/integration state \
             (merge_target and {}{}). This is a corrupted task state, not a valid close-mode workflow. \
             Repair it back to completion_mode='merge' and run task action=integrate instead.",
            if has_latest_commit {
                "latest_commit"
            } else {
                "phase-gate metadata"
            },
            if has_latest_commit && is_phase_gate {
                " / phase-gate metadata"
            } else {
                ""
            }
        )));
    }

    // Supervisor role check - must happen BEFORE any merge operations
    if caller_role != "supervisor" {
        let title = task_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let agent_name = caller_name(args, "worker");
        let supervisor = caller_supervisor(args);
        let notify_msg = if normalize_task_status(&current_status) == Some("approved") {
            match completion_mode {
                TaskCompletionMode::Merge => {
                    let merge_target = task_data
                        .get("merge_target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let default_branch =
                        detect_default_branch().unwrap_or_else(|_| "main".to_string());
                    if !merge_target.is_empty() && merge_target != default_branch {
                        format!(
                            "Task {id} (\"{title}\") is approved and targets epic branch '{merge_target}'. \
                             {agent_name} attempted to close it, but only supervisors can \
                             perform the terminal integration step. Please run:\n  \
                             task action=integrate id={id}\n  \
                             This will integrate the reviewed commit into '{merge_target}' and close the subtask."
                        )
                    } else {
                        format!(
                            "Task {id} (\"{title}\") is approved and ready to merge. \
                             {agent_name} attempted to close it, but only supervisors can \
                             perform the terminal action. Please run:\n  \
                             task action=close id={id}\n  \
                             Completion mode: merge. This will mark it as merged."
                        )
                    }
                }
                TaskCompletionMode::Close => format!(
                    "Task {id} (\"{title}\") is approved and ready to close without a merge. \
                     {agent_name} attempted to close it, but only supervisors can \
                     perform the terminal action. Please run:\n  \
                     task action=close id={id}\n  \
                     Completion mode: close. This will mark it as closed, not merged."
                ),
            }
        } else {
            format!(
                "{agent_name} attempted to close task {id} (\"{title}\") from status \
                 '{current_status}', but only supervisors can close tasks. Please review."
            )
        };
        if supervisor.is_empty() {
            return Ok(error_result(
                "Only supervisors can close tasks. No live supervisor session could be resolved for notification.",
            ));
        }
        let _ = crate::tools::agent::try_deliver_message(&supervisor, &agent_name, &notify_msg);
        return Ok(error_result(
            "Only supervisors can close tasks. Your supervisor has been automatically notified.",
        ));
    }

    // For container tasks with integration branches, merge into their parent/base branch
    // before falling back to the generic close path below.
    if is_container_task(&task_type) {
        let followup_blockers = collect_container_open_followup_blockers(id);
        if !followup_blockers.is_empty() {
            let summary = followup_blockers
                .iter()
                .map(|blocker| {
                    format!(
                        "{} ({}) has {} open followup(s)",
                        blocker.task_id,
                        blocker.title,
                        blocker.open_followups.len()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(error_result(format!(
                "Cannot close {task_type} {id}: unresolved approved-review followups remain. \
                 Default action: inspect them with `task action=followups id=<task-id>` and then \
                 `task action=promote_followups id=<task-id>` to create real cleanup work. \
                 Use `waive_followups` only for explicit no-action-needed items, with specific IDs and reasons. \
                 Affected tasks: {summary}"
            )));
        }
        let integration_branch = task_data.get("integration_branch").and_then(|v| v.as_str());
        if let Some(branch) = integration_branch {
            if !branch.is_empty() {
                let (total_children, _, _) = if is_epic(&task_type) {
                    check_epic_integration_status(id)
                } else {
                    check_initiative_epic_integration_status(id)
                };
                if total_children > 0 {
                    let base_branch = match container_base_branch_for_task(&task_data) {
                        Ok(branch) => branch,
                        Err(err) => return Ok(error_result(err)),
                    };
                    let integration_worktree = match ensure_container_integration_worktree(
                        id,
                        &task_type,
                        branch,
                        task_data
                            .get("integration_worktree")
                            .and_then(|v| v.as_str())
                            .filter(|v| !v.is_empty()),
                        false,
                        false,
                        Some(&base_branch),
                    )
                    .await
                    {
                        Ok(path) => path,
                        Err(err) => return Ok(error_result(err)),
                    };
                    let target_worktree =
                        match container_target_worktree_for_task(&task_data, &base_branch).await {
                            Ok(path) => path,
                            Err(err) => return Ok(error_result(err)),
                        };
                    let _repo_lock = match acquire_repo_lock().await {
                        Ok(lock) => lock,
                        Err(err) => {
                            return Ok(error_result(format!(
                                "Failed to acquire repository integration lock: {err}"
                            )))
                        }
                    };

                    let default_branch =
                        detect_default_branch().unwrap_or_else(|_| "main".to_string());
                    // Final initiative close is the public-history boundary:
                    // preserve detailed commits on initiative/epic branches, but land one
                    // audited squash commit on the default branch.
                    let merge_strategy = if is_initiative(&task_type)
                        && parent_id.is_none()
                        && base_branch == default_branch
                    {
                        ContainerMergeStrategy::Squash
                    } else {
                        ContainerMergeStrategy::Merge
                    };
                    let container_title = task_data
                        .get("title")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let merge_outcome = match merge_container_branch_into_target_with_strategy(
                        id,
                        container_title,
                        &task_type,
                        branch,
                        &base_branch,
                        &target_worktree,
                        merge_strategy,
                    ) {
                        Ok(outcome) => outcome,
                        Err(err) => return Ok(error_result(err)),
                    };
                    let merge_commit = merge_outcome.commit.clone();

                    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let closed_by = caller_name(args, "agent");
                    let parent_task = read_parent_task(&task_data);
                    let merged_into_parent_branch = is_epic(&task_type)
                        && parent_task
                            .as_ref()
                            .and_then(|parent| {
                                parent
                                    .get("task_type")
                                    .and_then(|v| v.as_str())
                                    .filter(|parent_type| is_initiative(parent_type))
                            })
                            .is_some()
                        && parent_task
                            .as_ref()
                            .and_then(|parent| {
                                parent.get("integration_branch").and_then(|v| v.as_str())
                            })
                            .is_some_and(|parent_branch| parent_branch == base_branch);
                    let terminal_status = if merged_into_parent_branch {
                        "closed"
                    } else {
                        "merged"
                    };

                    let mut task = task_data.clone();
                    // Snapshot the worker identity BEFORE clearing ownership so container closes
                    // can enqueue the recycle request after the task is persisted. Without this,
                    // `clear_terminal_task_ownership` nulls `assignee`/`review_owner` and we lose
                    // the worker whose context needs clearing.
                    let recycle_worker = terminal_worker_recycle_candidate(&task);
                    clear_terminal_task_ownership(&mut task);
                    task.insert("status".into(), Value::String(terminal_status.to_string()));
                    task.insert("closed_at".into(), Value::String(now.clone()));
                    task.insert("updated_at".into(), Value::String(now.clone()));
                    task.insert("closed_by".into(), Value::String(closed_by.clone()));
                    task.insert("closed_role".into(), Value::String(caller_role.clone()));
                    task.insert("completion_mode".into(), Value::String("merge".to_string()));
                    task.insert("merged_branch".into(), Value::String(base_branch.clone()));
                    task.insert("merged_commit".into(), Value::String(merge_commit.clone()));
                    task.insert(
                        "merge_strategy".into(),
                        Value::String(merge_outcome.strategy.as_str().to_string()),
                    );
                    if let Some(ref source_tip) = merge_outcome.squash_source_tip {
                        task.insert(
                            "squash_source_tip".into(),
                            Value::String(source_tip.clone()),
                        );
                    }
                    task.insert(
                        "integration_worktree".into(),
                        Value::String(integration_worktree.to_string_lossy().to_string()),
                    );
                    if is_epic(&task_type) {
                        task.insert(
                            "epic_branch_merged".into(),
                            Value::String(branch.to_string()),
                        );
                        if merged_into_parent_branch {
                            task.insert(
                                "integration_status".into(),
                                Value::String("integrated".to_string()),
                            );
                        }
                    }

                    if !write_task(id, &task) {
                        return Ok(error_result("Failed to persist container task after merge"));
                    }
                    let recycle_outcome = enqueue_worker_session_recycle_surfacing(
                        id,
                        recycle_worker.as_deref(),
                        "container close",
                    );
                    let worker_recycle_queued = recycle_outcome.queued;
                    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
                        return Ok(error_result(err));
                    }
                    if let Err(err) = delete_review_state(id) {
                        return Ok(error_result(format!(
                            "Failed to clear persisted review state for container {id}: {err}"
                        )));
                    }
                    if let Err(err) = resolve_promoted_followups_for_terminal_task(id, &task).await
                    {
                        return Ok(error_result(err));
                    }

                    let action = if terminal_status == "merged" {
                        "merged"
                    } else {
                        "closed"
                    };
                    let mut result = serde_json::json!({
                        "status": "ok",
                        "task_id": id,
                        "action": action,
                        "completion_mode": "merge",
                        "merged_branch": base_branch,
                        "integration_branch": branch,
                        "integration_worktree": integration_worktree.to_string_lossy().to_string(),
                        "merged_commit": merge_commit,
                        "merge_strategy": merge_outcome.strategy.as_str(),
                        "closed_by": closed_by,
                        "closed_role": caller_role,
                        "closed_at": now,
                        "worker_recycle_queued": worker_recycle_queued,
                    });
                    if let Some(ref source_tip) = merge_outcome.squash_source_tip {
                        result["squash_source_tip"] = Value::String(source_tip.clone());
                    }
                    if terminal_status == "closed" {
                        result["integration_status"] = Value::String("integrated".to_string());
                    }
                    if let Some(warning) = recycle_outcome.warning {
                        result["warnings"] = Value::Array(vec![warning]);
                    }
                    let base_message = if is_initiative(&task_type)
                        && merge_outcome.strategy == ContainerMergeStrategy::Squash
                    {
                        let source_tip = merge_outcome
                            .squash_source_tip
                            .as_deref()
                            .unwrap_or("unknown");
                        format!(
                            "Initiative {} squash-merged into {} from integration branch '{}' at source tip {} with commit {}.",
                            id, default_branch, branch, source_tip, merge_commit
                        )
                    } else if is_initiative(&task_type) {
                        format!(
                            "Initiative {} merged into {} from integration branch '{}' with commit {}.",
                            id, default_branch, branch, merge_commit
                        )
                    } else if merged_into_parent_branch {
                        format!(
                            "Epic {} integrated into initiative branch '{}' from '{}' with commit {}.",
                            id, base_branch, branch, merge_commit
                        )
                    } else {
                        format!(
                            "Epic {} merged into {} from integration branch '{}' with commit {}.",
                            id, default_branch, branch, merge_commit
                        )
                    };
                    result["message"] = Value::String(base_message.clone());
                    if let Some(ref parent_task_id) = parent_id {
                        let (total, closed, all_done) = check_child_completion(parent_task_id);
                        let parent_type = parent_task
                            .as_ref()
                            .and_then(|task| task.get("task_type").and_then(|v| v.as_str()))
                            .unwrap_or("initiative");
                        let child_label = child_collection_label(parent_type);
                        result["parent"] = serde_json::json!({
                            "task_id": parent_task_id,
                            "task_type": parent_type,
                            "children_total": total,
                            "children_closed": closed,
                            "remaining": total - closed,
                            "all_complete": all_done
                        });
                        result["message"] = Value::String(if all_done {
                            format!(
                                "{base_message} All {total} {child_label} of {parent_type} {parent_task_id} are now closed. The parent is ready for supervisor close-out."
                            )
                        } else {
                            format!(
                                "{base_message} {closed}/{total} {child_label} of {parent_type} {parent_task_id} closed. {} remaining.",
                                total - closed
                            )
                        });
                    }
                    return Ok(text_result(
                        serde_json::to_string_pretty(&result)
                            .map_err(|e| McpError::Serialization(e.to_string()))?,
                    ));
                }
            }
        }
    }

    let current_status = task_data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let completion_mode = task_completion_mode_from_task(&task_data);

    if is_container_task(&task_type) {
        let followup_blockers = collect_container_open_followup_blockers(id);
        if !followup_blockers.is_empty() {
            let summary = followup_blockers
                .iter()
                .map(|blocker| {
                    format!(
                        "{} ({}) has {} open followup(s)",
                        blocker.task_id,
                        blocker.title,
                        blocker.open_followups.len()
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(error_result(format!(
                "Cannot close {task_type} {id}: unresolved approved-review followups remain. \
                 Default action: inspect them with `task action=followups id=<task-id>` and then \
                 `task action=promote_followups id=<task-id>` to create real cleanup work. \
                 Use `waive_followups` only for explicit no-action-needed items, with specific IDs and reasons. \
                 Affected tasks: {summary}"
            )));
        }
    }

    let terminal_status = close_terminal_status(&current_status, &task_type, completion_mode);

    // Guard rail: prevent direct-to-main merge for subtasks targeting an epic branch
    // This check happens before verify_merge_ready to provide clear error message
    if terminal_status == "merged" {
        let merge_target = task_data
            .get("merge_target")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());

        // If merge_target is not the default branch (or fallback "main"), it's an epic branch
        if merge_target != default_branch && merge_target != "main" {
            return Ok(error_result(format!(
                "Task {} has merge_target='{}' (an epic branch) but the close operation would \
                 merge it into '{}'. Subtasks targeting an epic branch must be merged into the \
                 parent integration branch first, then the containing container continues the merge \
                 chain upward. Use the integration workflow: \
                 task action=integrate id={}.",
                id, merge_target, default_branch, id
            )));
        }
    }

    let merge_provenance = if terminal_status == "merged" {
        if let Some(request) = read_current_review_request(id) {
            if let Some(latest_commit) = task_data
                .get("latest_commit")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                let reviewed_commit = request.commit.trim();
                if !reviewed_commit.is_empty()
                    && !commits_refer_to_same_oid(latest_commit, reviewed_commit)
                {
                    let mut stale_task = task_data.clone();
                    let detected_at = chrono::Utc::now().to_rfc3339();
                    stale_task.insert("status".into(), Value::String("review_ready".to_string()));
                    stale_task.insert("updated_at".into(), Value::String(detected_at.clone()));
                    stale_task.insert(
                        "stale_review".into(),
                        serde_json::json!({
                            "reason": "latest_commit_changed_after_approval",
                            "approved_review_commit": reviewed_commit,
                            "latest_commit": latest_commit,
                            "detected_at": detected_at
                        }),
                    );
                    if !write_task(id, &stale_task) {
                        return Ok(error_result(format!(
                            "Cannot close task {id}: approved review commit {reviewed_commit} is stale because latest_commit is {latest_commit}, and Brehon failed to demote the task back to review_ready."
                        )));
                    }
                    let result = serde_json::json!({
                        "status": "error",
                        "task_id": id,
                        "action": "close",
                        "error_code": "stale_review_approval",
                        "current_status": current_status,
                        "new_task_status": "review_ready",
                        "reviewed_commit": reviewed_commit,
                        "latest_commit": latest_commit,
                        "message": format!(
                            "Cannot close task {id}: approved review commit {reviewed_commit} is stale because latest_commit is {latest_commit}. Task status was moved back to review_ready for a fresh review."
                        ),
                        "next_action": {
                            "kind": "request_review",
                            "tool": "verification",
                            "args": {
                                "action": "request_review",
                                "task_id": id
                            }
                        }
                    });
                    return Ok(error_result(
                        serde_json::to_string_pretty(&result)
                            .map_err(|e| McpError::Serialization(e.to_string()))?,
                    ));
                }
            }
        }
        match verify_merge_ready(id, Some(&task_data)) {
            Ok(provenance) => Some(provenance),
            Err(err) => return Ok(error_result(err)),
        }
    } else {
        None
    };

    if let Err(msg) = validate_status_transition(
        &current_status,
        terminal_status,
        &caller_role,
        &task_type,
        completion_mode,
    ) {
        return Ok(error_result(format!(
            "Cannot close task {id}: {msg}. To close from '{current_status}', move the \
             task through the proper gates first."
        )));
    }

    let mut task = task_data;
    let recycle_worker = if is_terminal_task_status(terminal_status) {
        terminal_worker_recycle_candidate(&task)
    } else {
        None
    };
    if is_terminal_task_status(terminal_status) {
        clear_terminal_task_ownership(&mut task);
    }
    task.insert("status".into(), Value::String(terminal_status.to_string()));
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let closed_by = caller_name(args, "agent");
    task.insert("closed_at".into(), Value::String(now.clone()));
    task.insert("updated_at".into(), Value::String(now));
    task.insert(
        "completion_mode".into(),
        Value::String(completion_mode.as_str().to_string()),
    );
    task.insert("closed_by".into(), Value::String(closed_by.clone()));
    task.insert("closed_role".into(), Value::String(caller_role.clone()));
    if let Some((ref merged_branch, ref merged_commit, _)) = merge_provenance {
        task.insert("merged_branch".into(), Value::String(merged_branch.clone()));
        task.insert("merged_commit".into(), Value::String(merged_commit.clone()));
    }

    // For epic integration flow: update integration_status when merged into epic branch
    if terminal_status == "merged" && parent_id.is_some() {
        task.insert(
            "integration_status".into(),
            Value::String("integrated".to_string()),
        );
    }

    if !write_task(id, &task) {
        return Ok(error_result(format!(
            "Failed to persist terminal task {id}"
        )));
    }
    let recycle_outcome =
        enqueue_worker_session_recycle_surfacing(id, recycle_worker.as_deref(), "terminal close");
    let worker_recycle_queued = recycle_outcome.queued;
    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }
    let released_panel = if is_terminal_task_status(terminal_status) {
        match release_panel_lease_for_task(id) {
            Ok(panel_id) => panel_id,
            Err(err) => return Ok(error_result(err)),
        }
    } else {
        None
    };
    if is_terminal_task_status(terminal_status) {
        if let Err(err) = delete_review_state(id) {
            return Ok(error_result(format!(
                "Task {id} reached terminal state '{terminal_status}', but failed to clear its persisted review state: {err}"
            )));
        }
        if let Err(err) = resolve_promoted_followups_for_terminal_task(id, &task).await {
            return Ok(error_result(err));
        }
    }

    let mut result = serde_json::json!({
        "status": "ok",
        "task_id": id,
        "action": terminal_status,
        "completion_mode": completion_mode.as_str(),
        "worker_recycle_queued": worker_recycle_queued,
        "closed_by": closed_by,
        "closed_role": caller_role
    });
    if let Some((ref merged_branch, ref merged_commit, ref merge_status)) = merge_provenance {
        result["merged_branch"] = Value::String(merged_branch.clone());
        result["merged_commit"] = Value::String(merged_commit.clone());
        result["merge_status"] = Value::String(merge_status.display().to_string());
    }

    // Get merge_target for this task (default to detected default branch)
    let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());
    let merge_target = task
        .get("merge_target")
        .and_then(|v| v.as_str())
        .unwrap_or(&default_branch)
        .to_string();
    result["merge_target"] = Value::String(merge_target.clone());

    // Get integration_status if present
    if let Some(int_status) = task.get("integration_status").and_then(|v| v.as_str()) {
        result["integration_status"] = Value::String(int_status.to_string());
    }
    if let Some(panel_id) = released_panel {
        result["released_panel"] = Value::String(panel_id);
    }
    if let Some(warning) = recycle_outcome.warning {
        result["warnings"] = Value::Array(vec![warning]);
    }

    let base_message = match terminal_status {
        "merged" => {
            let (merged_branch, merged_commit, merge_status) = merge_provenance
                .as_ref()
                .expect("merge provenance required for merged status");
            let status_str = merge_status.display();

            // Distinguish between merge to epic branch vs merge to default branch
            let target_desc = if merge_target != default_branch {
                format!(
                    "merged into epic branch '{}' ({} onto {})",
                    merge_target, status_str, merged_branch
                )
            } else {
                format!(
                    "{} (merged into {}). Verified reviewed commit {} on branch {}.",
                    status_str, merged_branch, merged_commit, merged_branch
                )
            };
            format!(
                "Task {} {}. Completion mode: merge. Merge target: {}.",
                id, target_desc, merge_target
            )
        }
        "closed" if normalize_task_status(&current_status) == Some("approved") => {
            format!(
                "Task {} closed after approval without a merge. Completion mode: close.",
                id
            )
        }
        _ => format!("Task {} closed.", id),
    };
    result["message"] = Value::String(base_message.clone());

    // If this task has a parent container, report parent progress.
    if let Some(ref parent_task_id) = parent_id {
        let parent_task = read_task(parent_task_id);
        let parent_type = parent_task
            .as_ref()
            .and_then(|task| task.get("task_type").and_then(|v| v.as_str()))
            .unwrap_or("epic");
        let child_label = child_collection_label(parent_type);
        let (total, closed, all_done) = check_child_completion(parent_task_id);
        result["parent"] = serde_json::json!({
            "task_id": parent_task_id,
            "task_type": parent_type,
            "children_total": total,
            "children_closed": closed,
            "remaining": total - closed,
            "all_complete": all_done
        });
        if all_done {
            result["message"] = Value::String(format!(
                "{base_message} All {total} {child_label} of {parent_type} {parent_task_id} are now closed. \
                 The parent is ready for supervisor close-out."
            ));
        } else {
            result["message"] = Value::String(format!(
                "{base_message} {closed}/{total} {child_label} of {parent_type} {parent_task_id} closed. \
                 {} remaining.",
                total - closed
            ));
        }
    }

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}
