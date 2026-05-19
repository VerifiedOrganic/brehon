//! Handler for the "integrate" task action — driven by the integration state machine.
//!
//! Reads `IntegrationState` from task JSON, runs git probes, calls `next_state`,
//! executes the returned `Action`, and writes the new state back.
//!
//! The old `supervisor_conflict_resume` inference branch is gone. Resume is now
//! the normal `cherry_picking → resolved` path detected by authoritative git
//! probes (CHERRY_PICK_HEAD, unmerged files, ancestry, patch-equivalence).

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use brehon_types::{normalize_task_status, TaskCompletionMode};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::verification::{
    commits_refer_to_same_oid, delete_review_state, release_panel_lease_for_task, reviewed_commits,
};
use crate::tools::{error_result, text_result};

use super::integration_proof::{IntegrationProofRecorder, IntegrationSuccessProof};

use super::epic::{
    check_epic_completion, continue_cherry_pick, ensure_epic_integration_worktree,
    read_current_review_request, start_cherry_pick, verify_applied,
};
use super::followups::resolve_promoted_followups_for_terminal_task;
use super::git_ops::{
    cherry_pick_in_progress_in, cherry_pick_sha_in, detect_default_branch,
    git_commit_is_ancestor_in, git_run_ok_in, is_patch_equivalent_in_window_in, tree_matches_after,
    unmerged_files,
};
use super::integration_state::{
    next_state, read_integration_state, validate_raw_integration_phase, write_integration_state,
    Action, GitProbeResult, IntegrationPhase, IntegrationState, OperatorIntent,
};
use super::lifecycle::{
    caller_name, caller_role, caller_supervisor, clear_terminal_task_ownership,
    reconcile_dependency_states_with_task_lock, task_completion_mode_from_task,
};
use super::locking::{acquire_repo_lock, acquire_task_lock};
use super::paths::ensure_brehon_worktree_path;
use super::persistence::{read_task, write_task};
use super::{enqueue_worker_session_recycle_surfacing, terminal_worker_recycle_candidate};

pub(super) async fn execute(
    args: &Value,
    proof_recorder: &IntegrationProofRecorder,
) -> Result<ToolResult, McpError> {
    let id = match args.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(error_result("Missing required parameter: id")),
    };

    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let intent = if force {
        OperatorIntent::ForceIntegrate
    } else {
        OperatorIntent::Integrate
    };

    let _task_lock = match acquire_task_lock(id).await {
        Ok(lock) => lock,
        Err(err) => return Ok(error_result(format!("Failed to lock task {id}: {err}"))),
    };

    let Some(task_data) = read_task(id) else {
        return Ok(error_result(format!("Task not found: {id}")));
    };
    if let Err(err) = validate_raw_integration_phase(id, &task_data) {
        return Ok(error_result(err));
    }

    let caller_role = caller_role(args);
    if caller_role != "supervisor" {
        let agent_name = caller_name(args, "worker");
        let supervisor = caller_supervisor(args);
        let title = task_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let notify_msg = format!(
            "Task {id} (\"{title}\") requires epic-branch integration after approval. \
             {agent_name} attempted task action=integrate, but only supervisors can perform \
             post-review integration. Please run:\n  \
             task action=integrate id={id}"
        );
        if !supervisor.is_empty() {
            let _ = crate::tools::agent::try_deliver_message(&supervisor, &agent_name, &notify_msg);
        }
        let current_status = task_data
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let merge_target = task_data.get("merge_target").and_then(|v| v.as_str());
        return integrate_error_response(
            id,
            IntegrationPhase::Null,
            current_status,
            merge_target,
            &[],
            "",
            &[],
            "supervisor_only",
            "Only supervisors can integrate approved subtasks into epic branches.",
            Some("Workers cannot run post-review epic-branch integration."),
            serde_json::json!({
                "kind": "notify_supervisor",
                "description": "Ask the supervisor to run integrate for this approved subtask.",
                "command": format!("mcp_brehon_task action=integrate id={id}")
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Wait for a supervisor to perform the integration step."
            }),
        );
    }

    let current_status = task_data
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let current_normalized = normalize_task_status(&current_status);
    let current_integration_status = task_data
        .get("integration_status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let persisted_state = read_integration_state(&task_data);

    // Idempotent early exit for tasks that were already integrated via the
    // legacy path (integration_status + closed) or the new state machine.
    if current_normalized == Some("closed") && current_integration_status == "integrated" {
        if force {
            let state = read_integration_state(&task_data);
            let worktree_path = task_data
                .get("integration_worktree")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    (!state.worktree_path.is_empty()).then_some(state.worktree_path.as_str())
                })
                .unwrap_or("")
                .to_string();
            let merge_target = task_data.get("merge_target").and_then(|v| v.as_str());
            return integrate_error_response(
                id,
                if state.phase == IntegrationPhase::Null {
                    IntegrationPhase::Complete
                } else {
                    state.phase
                },
                &current_status,
                merge_target,
                &state.conflicting_files,
                &worktree_path,
                &state.reviewed_commits,
                "integration_already_completed",
                &format!(
                    "Task {id} integration already completed; manual revert required before force=true retry."
                ),
                Some("force=true cannot restart a completed integration without a manual revert."),
                serde_json::json!({
                    "kind": "manual_revert_required",
                    "description": "Manually revert the completed integration before retrying force=true."
                }),
                serde_json::json!({
                    "kind": "none",
                    "description": "No further integrate action is available until the completed merge is reverted."
                }),
            );
        }
        let merge_target = task_data
            .get("merge_target")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let merged_commit = task_data
            .get("merged_commit")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let state = read_integration_state(&task_data);
        let worktree_path = task_data
            .get("integration_worktree")
            .and_then(|v| v.as_str())
            .or_else(|| (!state.worktree_path.is_empty()).then_some(state.worktree_path.as_str()))
            .unwrap_or("")
            .to_string();
        let mut result = structured_integrate_response(
            id,
            if state.phase == IntegrationPhase::Null {
                IntegrationPhase::Complete
            } else {
                state.phase
            },
            "already_integrated",
            &state.conflicting_files,
            &worktree_path,
            &state.reviewed_commits,
            None,
            serde_json::json!({
                "kind": "none",
                "description": "No manual supervisor action is required."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "No further integrate action is required."
            }),
        );
        result["action"] = Value::String("integrated".to_string());
        result["terminal_status"] = Value::String("closed".to_string());
        result["merge_target"] = Value::String(merge_target.to_string());
        result["merged_branch"] = Value::String(merge_target.to_string());
        result["merged_commit"] = Value::String(merged_commit.to_string());
        result["integration_status"] = Value::String("integrated".to_string());
        result["already_integrated"] = Value::Bool(true);
        result["message"] = Value::String(format!(
            "Task {} was already integrated into epic branch '{}' and is already closed.",
            id, merge_target
        ));
        if !worktree_path.is_empty() {
            result["integration_worktree"] = Value::String(worktree_path);
        }
        return Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ));
    }

    let completion_mode = task_completion_mode_from_task(&task_data);
    if completion_mode != TaskCompletionMode::Merge {
        return Ok(error_result(format!(
            "Task {id} has completion_mode='{}'. Use task action=close instead of integrate.",
            completion_mode.as_str()
        )));
    }

    let parent_id = match task_data.get("parent_id").and_then(|v| v.as_str()) {
        Some(parent) if !parent.is_empty() => parent.to_string(),
        _ => {
            return Ok(error_result(format!(
                "Task {id} is not linked to an epic. Only feature-epic subtasks can use integrate."
            )));
        }
    };

    if persisted_state.phase == IntegrationPhase::Aborted && !force {
        return Ok(error_result("explicitly aborted; use force=true to retry"));
    }

    let _repo_lock = match acquire_repo_lock().await {
        Ok(lock) => lock,
        Err(err) => {
            return Ok(error_result(format!(
                "Failed to acquire repository integration lock: {err}"
            )));
        }
    };

    // --- Review metadata ---
    let review_request = read_current_review_request(id);
    let reviewed_commit = review_request
        .as_ref()
        .map(|request| request.commit.trim().to_string())
        .unwrap_or_default();
    let resolved_empty_commit_set = review_request
        .as_ref()
        .is_some_and(|request| request.resolved_empty_commit_set);
    let reviewed_commit_set = review_request
        .as_ref()
        .map(reviewed_commits)
        .unwrap_or_default();

    if reviewed_commit.is_empty() || (reviewed_commit_set.is_empty() && !resolved_empty_commit_set)
    {
        return Ok(error_result(format!(
            "Cannot integrate task {id}: the approved review recorded no commit."
        )));
    }

    if let Some(latest_commit) = task_data
        .get("latest_commit")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !commits_refer_to_same_oid(latest_commit, &reviewed_commit) {
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
                    "Cannot integrate task {id}: approved review commit {reviewed_commit} is stale because latest_commit is {latest_commit}, and Brehon failed to demote the task back to review_ready."
                )));
            }
            return integrate_error_response(
                id,
                IntegrationPhase::Null,
                &current_status,
                task_data.get("merge_target").and_then(|value| value.as_str()),
                &[],
                "",
                &reviewed_commit_set,
                "stale_review_approval",
                &format!(
                    "Cannot integrate task {id}: approved review commit {reviewed_commit} is stale because latest_commit is {latest_commit}. Task status was moved back to review_ready for a fresh review."
                ),
                Some("Approval only applies to the exact task latest_commit that was reviewed."),
                serde_json::json!({
                    "kind": "request_review",
                    "tool": "verification",
                    "args": {
                        "action": "request_review",
                        "task_id": id
                    }
                }),
                serde_json::json!({
                    "kind": "none",
                    "description": "Stale approval was invalidated by moving the task back to review_ready."
                }),
            );
        }
    }

    // --- Merge target & worktree ---
    let merge_target = task_data
        .get("merge_target")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(String::from)
        .unwrap_or_else(|| detect_default_branch().unwrap_or_else(|_| "main".to_string()));
    let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());
    if merge_target == default_branch {
        return Ok(error_result(format!(
            "Task {id} targets default branch '{default_branch}'. Use task action=close for direct-to-{default_branch} merge flow."
        )));
    }

    let parent_task = match read_task(&parent_id) {
        Some(p) => p,
        None => {
            return Ok(error_result(format!(
                "Task {id} references missing parent epic {parent_id}. Cannot integrate."
            )));
        }
    };

    let integration_worktree = match ensure_epic_integration_worktree(
        &parent_id,
        &merge_target,
        parent_task
            .get("integration_worktree")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty()),
        false,
        force,
    )
    .await
    {
        Ok(path) => path,
        Err(err) => return Ok(error_result(err)),
    };

    // --- State machine ---
    let mut state = read_integration_state(&task_data);

    // Ensure state is initialised with current metadata.
    state.epic_branch = merge_target.clone();
    state.worktree_path = integration_worktree.to_string_lossy().to_string();
    state.reviewed_commits = if reviewed_commit_set.is_empty() {
        vec![reviewed_commit.clone()]
    } else {
        reviewed_commit_set.clone()
    };
    if state.started_at.is_empty() {
        state.started_at = chrono::Utc::now().to_rfc3339();
    }

    // Empty commit set: nothing to cherry-pick, but only approved tasks may
    // take the no-op finalize path.
    if resolved_empty_commit_set && current_normalized == Some("approved") {
        state.phase = IntegrationPhase::Complete;
    } else if resolved_empty_commit_set {
        return integrate_error_response(
            id,
            state.phase,
            &current_status,
            Some(&merge_target),
            &state.conflicting_files,
            &state.worktree_path,
            &state.reviewed_commits,
            "integration_requires_approved_status",
            &format!(
                "Cannot integrate from status '{}'. Only approved subtasks can be integrated into an epic branch.",
                current_status
            ),
            Some("The empty reviewed-commit-set shortcut only applies to approved subtasks."),
            serde_json::json!({
                "kind": "approve_first",
                "description": "Move the task to approved before attempting integration."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Integrate will keep rejecting until the task reaches approved status."
            }),
        );
    }

    let mut task = task_data.clone();
    let mut already_integrated = resolved_empty_commit_set;

    // Main loop: advance at most a few steps per call.
    let mut loop_count = 0;
    const MAX_LOOPS: usize = 5;

    while loop_count < MAX_LOOPS {
        loop_count += 1;

        // If we reached Complete via a prior iteration, run Finalize.
        if state.phase == IntegrationPhase::Complete && !force {
            return finalize_integration(
                id,
                &mut task,
                &state,
                args,
                &merge_target,
                &integration_worktree,
                &reviewed_commit,
                resolved_empty_commit_set,
                already_integrated,
                proof_recorder,
            )
            .await;
        }

        let probes = run_git_probes(&integration_worktree, &state, &merge_target)?;
        let prior_phase = state.phase;
        let transition = next_state(
            state.phase,
            &current_status,
            &probes,
            &state.reviewed_commits,
            intent,
        );

        match transition.action {
            Action::Idempotent => {
                let integrated_commit =
                    git_stdout_in(&integration_worktree, &["rev-parse", "HEAD"])
                        .map_err(McpError::Internal)?;
                return idempotent_response(
                    id,
                    &state,
                    &task,
                    &merge_target,
                    &integration_worktree,
                    &integrated_commit,
                );
            }

            Action::Reject(ref reason) => {
                // "Still unresolved" is a waiting state, not a hard error.
                if state.phase == IntegrationPhase::CherryPicking
                    && reason.starts_with("still unresolved")
                {
                    state.conflicting_files = probes.unmerged_files;
                    state.last_transition_at = chrono::Utc::now().to_rfc3339();
                    if let Err(tool_result) =
                        persist_conflict_and_cleanup(id, &mut task, &state).await
                    {
                        return Ok(error_result(tool_result));
                    }
                    return conflict_response(id, &state, reason, &merge_target);
                }
                return integrate_state_reject_response(
                    id,
                    &state,
                    &current_status,
                    &merge_target,
                    reason,
                );
            }

            Action::ForceReset(ref reason) => {
                let prior_state = state.clone();
                reset_state_for_force_retry(
                    id,
                    &current_status,
                    &prior_state,
                    &mut state,
                    &integration_worktree,
                    &merge_target,
                    reason,
                )?;
            }

            Action::Finalize => {
                state.phase = transition.new_phase;
                state.last_transition_at = chrono::Utc::now().to_rfc3339();
                return finalize_integration(
                    id,
                    &mut task,
                    &state,
                    args,
                    &merge_target,
                    &integration_worktree,
                    &reviewed_commit,
                    resolved_empty_commit_set,
                    already_integrated,
                    proof_recorder,
                )
                .await;
            }

            Action::Verify => {
                if matches!(
                    prior_phase,
                    IntegrationPhase::Null | IntegrationPhase::CherryPicking
                ) {
                    already_integrated = true;
                }
                state.phase = transition.new_phase;
                state.last_transition_at = chrono::Utc::now().to_rfc3339();
                // Continue loop — Verify is transient and should lead to Finalize.
            }

            Action::StartCherryPick => {
                if state.cherry_pick_base_head.is_empty() {
                    state.cherry_pick_base_head =
                        git_stdout_in(&integration_worktree, &["rev-parse", "HEAD"])
                            .map_err(McpError::Internal)?;
                }
                match execute_cherry_picks(&integration_worktree, &state.reviewed_commits) {
                    Ok(()) => {
                        state.phase = IntegrationPhase::Resolved;
                        state.last_transition_at = chrono::Utc::now().to_rfc3339();
                        // Continue loop to move Resolved → Complete.
                    }
                    Err(CherryPickError::Conflict { details }) => {
                        state.phase = IntegrationPhase::CherryPicking;
                        state.conflicting_files =
                            unmerged_files(&integration_worktree).unwrap_or_default();
                        state.attempts += 1;
                        state.last_transition_at = chrono::Utc::now().to_rfc3339();
                        if let Err(tool_result) =
                            persist_conflict_and_cleanup(id, &mut task, &state).await
                        {
                            return Ok(error_result(tool_result));
                        }
                        return conflict_response(id, &state, &details, &merge_target);
                    }
                    Err(CherryPickError::Other(msg)) => {
                        return Ok(error_result(msg));
                    }
                }
            }

            Action::ContinueCherryPick => {
                match continue_cherry_pick(&integration_worktree) {
                    Ok(()) => {
                        // There may be more commits to pick after continuing.
                        match execute_cherry_picks(&integration_worktree, &state.reviewed_commits) {
                            Ok(()) => {
                                state.phase = IntegrationPhase::Resolved;
                                state.last_transition_at = chrono::Utc::now().to_rfc3339();
                            }
                            Err(CherryPickError::Conflict { details }) => {
                                state.phase = IntegrationPhase::CherryPicking;
                                state.conflicting_files =
                                    unmerged_files(&integration_worktree).unwrap_or_default();
                                state.attempts += 1;
                                state.last_transition_at = chrono::Utc::now().to_rfc3339();
                                if let Err(tool_result) =
                                    persist_conflict_and_cleanup(id, &mut task, &state).await
                                {
                                    return Ok(error_result(tool_result));
                                }
                                return conflict_response(id, &state, &details, &merge_target);
                            }
                            Err(CherryPickError::Other(msg)) => {
                                return Ok(error_result(msg));
                            }
                        }
                    }
                    Err(err) => {
                        state.conflicting_files =
                            unmerged_files(&integration_worktree).unwrap_or_default();
                        state.last_transition_at = chrono::Utc::now().to_rfc3339();
                        if let Err(tool_result) =
                            persist_conflict_and_cleanup(id, &mut task, &state).await
                        {
                            return Ok(error_result(tool_result));
                        }
                        return conflict_response(id, &state, &err, &merge_target);
                    }
                }
            }
        }
    }

    // Loop exited without reaching a terminal action — this should not happen.
    Ok(error_result(format!(
        "Integration state machine for task {id} did not reach a terminal state. Current phase: {:?}.",
        state.phase
    )))
}

// ---------------------------------------------------------------------------
// Git probes
// ---------------------------------------------------------------------------

fn run_git_probes(
    worktree: &Path,
    state: &IntegrationState,
    branch: &str,
) -> Result<GitProbeResult, McpError> {
    let cherry_pick_in_progress = cherry_pick_in_progress_in(worktree);
    let cherry_pick_sha = cherry_pick_sha_in(worktree);
    let unmerged_files = unmerged_files(worktree).unwrap_or_default();
    // Empty set: vacuously true — no reviewed commits to probe means the
    // whole-set conditions already hold.
    let mut is_ancestor = true;
    let mut is_patch_equivalent = true;
    let mut has_reviewed_cherry_pick_trailers =
        !state.reviewed_commits.is_empty() && !state.cherry_pick_base_head.is_empty();
    let mut reviewed_commits_applied = true;
    let mut tree_matches_after_all = true;
    let reviewed_commits_with_trailers =
        if state.reviewed_commits.is_empty() || state.cherry_pick_base_head.is_empty() {
            HashSet::new()
        } else {
            reviewed_commits_with_cherry_pick_trailers_since(
                worktree,
                &state.cherry_pick_base_head,
                &state.reviewed_commits,
            )
            .unwrap_or_default()
        };

    for commit in &state.reviewed_commits {
        let commit_is_ancestor =
            git_commit_is_ancestor_in(worktree, commit, branch).unwrap_or(false);
        let commit_is_patch_equivalent = commit_is_ancestor
            || is_patch_equivalent_in_window_in(worktree, commit, branch, 50).unwrap_or(false);
        let commit_has_cherry_pick_trailer = reviewed_commits_with_trailers.contains(commit);
        let commit_is_applied = commit_is_patch_equivalent || commit_has_cherry_pick_trailer;
        let commit_tree_matches = commit_is_patch_equivalent
            || tree_matches_after(worktree, commit, branch).unwrap_or(false);

        is_ancestor &= commit_is_ancestor;
        is_patch_equivalent &= commit_is_patch_equivalent;
        has_reviewed_cherry_pick_trailers &= commit_has_cherry_pick_trailer;
        reviewed_commits_applied &= commit_is_applied;
        tree_matches_after_all &= commit_tree_matches;
    }

    Ok(GitProbeResult {
        cherry_pick_in_progress,
        cherry_pick_sha,
        unmerged_files,
        is_ancestor,
        is_patch_equivalent,
        has_reviewed_cherry_pick_trailers,
        reviewed_commits_applied,
        tree_matches_after: tree_matches_after_all,
    })
}

fn reviewed_commits_with_cherry_pick_trailers_since(
    worktree: &Path,
    base_head: &str,
    reviewed_commits: &[String],
) -> Result<HashSet<String>, String> {
    let range = format!("{base_head}..HEAD");
    let output = crate::git_exec::run_git(worktree, &["log", "--format=%H%x1e%B%x1f", &range])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git log --format=%B%x1e {range} exited with status {}",
                output.status
            )
        } else {
            stderr
        });
    }

    let history = String::from_utf8_lossy(&output.stdout);
    let commit_bodies: Vec<&str> = history
        .split('\u{1f}')
        .filter_map(|entry| entry.split_once('\u{1e}').map(|(_, body)| body))
        .collect();
    let mut matches = HashSet::new();
    for reviewed_commit in reviewed_commits {
        let needle = format!("(cherry picked from commit {reviewed_commit})");
        if commit_bodies.iter().any(|body| body.contains(&needle)) {
            matches.insert(reviewed_commit.clone());
        }
    }
    Ok(matches)
}

fn reset_state_for_force_retry(
    task_id: &str,
    task_status: &str,
    prior_state: &IntegrationState,
    state: &mut IntegrationState,
    integration_worktree: &Path,
    merge_target: &str,
    reason: &str,
) -> Result<(), McpError> {
    let destructive = prior_state.phase == IntegrationPhase::CherryPicking;
    if destructive {
        reset_irrecoverable_worktree(integration_worktree, merge_target)
            .map_err(McpError::Internal)?;
    }

    tracing::warn!(
        task_id = %task_id,
        force_integrate = true,
        prior_phase = ?prior_state.phase,
        prior_task_status = %task_status,
        prior_state = ?prior_state,
        destructive,
        reason = %reason,
        "Force-resetting integration state before retry"
    );

    let now = chrono::Utc::now().to_rfc3339();
    let cherry_pick_base_head =
        git_stdout_in(integration_worktree, &["rev-parse", "HEAD"]).map_err(McpError::Internal)?;
    *state = IntegrationState {
        phase: IntegrationPhase::Null,
        epic_branch: merge_target.to_string(),
        worktree_path: integration_worktree.to_string_lossy().to_string(),
        cherry_pick_base_head,
        reviewed_commits: prior_state.reviewed_commits.clone(),
        started_at: now.clone(),
        last_transition_at: now,
        conflicting_files: Vec::new(),
        attempts: 0,
        resolution: None,
    };
    Ok(())
}

fn reset_irrecoverable_worktree(worktree: &Path, merge_target: &str) -> Result<(), String> {
    ensure_brehon_worktree_path(worktree, "force integrate cleanup worktree")?;
    let _ = git_run_ok_in(worktree, &["cherry-pick", "--quit"]);
    git_run_ok_in(worktree, &["reset", "--hard", merge_target])?;
    git_run_ok_in(worktree, &["clean", "-fd"])?;

    if cherry_pick_in_progress_in(worktree) {
        return Err(format!(
            "force=true cleanup for '{}' failed: CHERRY_PICK_HEAD is still present",
            worktree.display()
        ));
    }

    let remaining_unmerged = unmerged_files(worktree).unwrap_or_default();
    if !remaining_unmerged.is_empty() {
        return Err(format!(
            "force=true cleanup for '{}' failed: unmerged files remain: {}",
            worktree.display(),
            remaining_unmerged.join(", ")
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Cherry-pick execution
// ---------------------------------------------------------------------------

enum CherryPickError {
    Conflict { details: String },
    Other(String),
}

/// Persist conflict state, reconcile dependencies, release panel, and clear review state.
async fn persist_conflict_and_cleanup(
    id: &str,
    task: &mut serde_json::Map<String, Value>,
    state: &IntegrationState,
) -> Result<(), String> {
    write_integration_state(task, state);
    if !write_task(id, task) {
        return Err(format!(
            "Task {id} hit an integration conflict but Brehon failed to persist state."
        ));
    }
    reconcile_dependency_states_with_task_lock(id).await?;
    release_panel_lease_for_task(id)?;
    delete_review_state(id).map_err(|e| {
        format!(
            "Task {id} hit an integration conflict, but Brehon failed to clear the stale review state: {e}"
        )
    })?;
    Ok(())
}

fn execute_cherry_picks(
    worktree: &Path,
    reviewed_commits: &[String],
) -> Result<(), CherryPickError> {
    for commit in reviewed_commits {
        if verify_applied(worktree, commit, "HEAD").map_err(CherryPickError::Other)? {
            continue;
        }
        if let Err(err) = start_cherry_pick(worktree, commit) {
            if can_skip_failed_cherry_pick_as_empty(worktree, &err) {
                continue_cherry_pick(worktree)
                    .map_err(|e| CherryPickError::Other(format!("Empty pick skip failed: {e}")))?;
                continue;
            }
            return Err(CherryPickError::Conflict { details: err });
        }
    }
    Ok(())
}

// Re-exported from the git_cherry_pick tool so we can detect empty picks.
fn can_skip_failed_cherry_pick_as_empty(worktree: &Path, stderr: &str) -> bool {
    crate::tools::git_cherry_pick::can_skip_failed_cherry_pick_as_empty(worktree, stderr)
}

// ---------------------------------------------------------------------------
// Helpers reused from git_ops (pub(super) in that module)
// ---------------------------------------------------------------------------

use super::git_ops::git_stdout_in;

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

async fn finalize_integration(
    id: &str,
    task: &mut serde_json::Map<String, Value>,
    state: &IntegrationState,
    args: &Value,
    merge_target: &str,
    integration_worktree: &Path,
    reviewed_commit: &str,
    resolved_empty_commit_set: bool,
    already_integrated: bool,
    proof_recorder: &IntegrationProofRecorder,
) -> Result<ToolResult, McpError> {
    let integrated_commit =
        git_stdout_in(integration_worktree, &["rev-parse", "HEAD"]).map_err(McpError::Internal)?;

    let recycle_worker = terminal_worker_recycle_candidate(task);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let closed_by = caller_name(args, "agent");
    let caller_role = caller_role(args);

    // Remove legacy conflict markers if present (migration compatibility).
    task.remove("integration_conflict");
    task.remove("activity");
    if task
        .get("blockers")
        .and_then(|value| value.as_str())
        .is_some_and(|value| value.contains(super::epic::INTEGRATION_CONFLICT_BLOCKER_PREFIX))
    {
        task.remove("blockers");
    }
    clear_terminal_task_ownership(task);

    task.insert("status".into(), Value::String("closed".to_string()));
    task.insert(
        "completion_mode".into(),
        Value::String(TaskCompletionMode::Merge.as_str().to_string()),
    );
    task.insert(
        "integration_status".into(),
        Value::String("integrated".to_string()),
    );
    task.insert("closed_at".into(), Value::String(now.clone()));
    task.insert("updated_at".into(), Value::String(now.clone()));
    task.insert("closed_by".into(), Value::String(closed_by.clone()));
    task.insert("closed_role".into(), Value::String(caller_role.clone()));
    task.insert(
        "merged_branch".into(),
        Value::String(merge_target.to_string()),
    );
    task.insert(
        "merged_commit".into(),
        Value::String(integrated_commit.clone()),
    );

    // Write the new state machine state as Complete.
    let mut final_state = state.clone();
    final_state.phase = IntegrationPhase::Complete;
    final_state.last_transition_at = now.clone();
    final_state.conflicting_files.clear();
    write_integration_state(task, &final_state);

    if !write_task(id, task) {
        return Ok(error_result(format!(
            "Integrated reviewed commit into '{merge_target}', but failed to persist task {id}. \
             Re-run task action=integrate id={id} to record the integrated state."
        )));
    }

    let recycle_outcome = enqueue_worker_session_recycle_surfacing(
        id,
        recycle_worker.as_deref(),
        "integration close",
    );
    let worker_recycle_queued = recycle_outcome.queued;

    if let Err(err) = reconcile_dependency_states_with_task_lock(id).await {
        return Ok(error_result(err));
    }
    let released_panel = match release_panel_lease_for_task(id) {
        Ok(panel_id) => panel_id,
        Err(err) => return Ok(error_result(err)),
    };
    if let Err(err) = delete_review_state(id) {
        return Ok(error_result(format!(
            "Integrated task {id}, but failed to clear its persisted review state: {err}"
        )));
    }
    if let Err(err) = resolve_promoted_followups_for_terminal_task(id, task).await {
        return Ok(error_result(err));
    }

    let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());
    let worktree_path = integration_worktree.to_string_lossy().to_string();
    let mut result = structured_integrate_response(
        id,
        IntegrationPhase::Complete,
        "integrated",
        &final_state.conflicting_files,
        &worktree_path,
        &final_state.reviewed_commits,
        None,
        serde_json::json!({
            "kind": "none",
            "description": "No manual supervisor action is required."
        }),
        serde_json::json!({
            "kind": "none",
            "description": "No further integrate action is required."
        }),
    );
    result["action"] = Value::String("integrated".to_string());
    result["terminal_status"] = Value::String("closed".to_string());
    result["completion_mode"] = Value::String("merge".to_string());
    result["merge_target"] = Value::String(merge_target.to_string());
    result["merged_branch"] = Value::String(merge_target.to_string());
    result["merged_commit"] = Value::String(integrated_commit.clone());
    result["integration_worktree"] = Value::String(worktree_path);
    result["reviewed_commit"] = Value::String(reviewed_commit.to_string());
    result["integration_status"] = Value::String("integrated".to_string());
    result["worker_recycle_queued"] = Value::Bool(worker_recycle_queued);
    if let Some(warning) = recycle_outcome.warning {
        result["warnings"] = Value::Array(vec![warning]);
    }
    result["released_panel"] = released_panel.map(Value::String).unwrap_or(Value::Null);
    result["closed_by"] = Value::String(closed_by);
    result["closed_role"] = Value::String(caller_role);
    result["closed_at"] = Value::String(now.clone());
    result["already_integrated"] = Value::Bool(already_integrated);
    result["message"] = Value::String(format!(
        "Task {} integrated into merge-target worktree '{}' on branch '{}' and closed. {} This task now stops at its parent integration branch. Only a top-level container close may merge to {}.",
        id,
        integration_worktree.display(),
        task.get("merge_target")
            .and_then(|v| v.as_str())
            .unwrap_or("epic branch"),
        if resolved_empty_commit_set {
            format!(
                "Approved review for {} resolved to an empty reviewed set, so integration was a no-op and branch HEAD remains {}.",
                reviewed_commit, integrated_commit
            )
        } else if already_integrated {
            format!(
                "Reviewed commit {} was already present on the branch.",
                reviewed_commit
            )
        } else {
            format!(
                "Cherry-picked reviewed commit {}; branch HEAD is now {}.",
                reviewed_commit, integrated_commit
            )
        },
        default_branch
    ));

    let parent_id = task
        .get("parent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !parent_id.is_empty() {
        let (total, closed, all_done) = check_epic_completion(&parent_id);
        result["parent_epic"] = serde_json::json!({
            "epic_id": parent_id,
            "subtasks_total": total,
            "subtasks_closed": closed,
            "remaining": total - closed,
            "all_complete": all_done
        });
    }

    let worktree_string = integration_worktree.to_string_lossy().to_string();
    let source_branch = task
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let proof_outcome = proof_recorder
        .record_success(IntegrationSuccessProof {
            task_id: id,
            status: "integrated",
            source_branch,
            target_branch: Some(merge_target),
            worktree_path: Some(worktree_string.as_str()),
            commit: Some(integrated_commit.as_str()),
            summary: Some(format!(
                "Integrated reviewed commit {reviewed_commit} into branch '{merge_target}' at HEAD {integrated_commit}."
            )),
        })
        .await;
    proof_outcome.attach_to_result(&mut result);

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn idempotent_response(
    id: &str,
    state: &IntegrationState,
    task: &serde_json::Map<String, Value>,
    merge_target: &str,
    integration_worktree: &Path,
    integrated_commit: &str,
) -> Result<ToolResult, McpError> {
    let merge_target = task
        .get("merge_target")
        .and_then(|v| v.as_str())
        .unwrap_or(merge_target);
    let merged_commit = task
        .get("merged_commit")
        .and_then(|v| v.as_str())
        .unwrap_or(integrated_commit);
    let worktree_path = integration_worktree.to_string_lossy().to_string();
    let mut result = structured_integrate_response(
        id,
        state.phase,
        "already_integrated",
        &state.conflicting_files,
        &worktree_path,
        &state.reviewed_commits,
        None,
        serde_json::json!({
            "kind": "none",
            "description": "No manual supervisor action is required."
        }),
        serde_json::json!({
            "kind": "none",
            "description": "No further integrate action is required."
        }),
    );
    let message = format!(
        "Task {} was already integrated into epic branch '{}'.",
        id, merge_target
    );
    result["action"] = Value::String("integrated".to_string());
    result["terminal_status"] = Value::String("closed".to_string());
    result["merge_target"] = Value::String(merge_target.to_string());
    result["merged_branch"] = Value::String(merge_target.to_string());
    result["merged_commit"] = Value::String(merged_commit.to_string());
    result["integration_worktree"] = Value::String(worktree_path);
    result["already_integrated"] = Value::Bool(true);
    result["message"] = Value::String(message);
    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn conflict_response(
    id: &str,
    state: &IntegrationState,
    details: &str,
    merge_target: &str,
) -> Result<ToolResult, McpError> {
    let conflicting_files = if state.conflicting_files.is_empty() {
        Vec::new()
    } else {
        state.conflicting_files.clone()
    };
    let add_targets = if conflicting_files.is_empty() {
        ".".to_string()
    } else {
        conflicting_files.join(" ")
    };
    let edit_hint = if conflicting_files.is_empty() {
        "# edit conflicting files".to_string()
    } else {
        format!("# edit: {}", conflicting_files.join(" "))
    };
    let result = structured_integrate_response(
        id,
        IntegrationPhase::CherryPicking,
        "waiting_for_supervisor",
        &conflicting_files,
        &state.worktree_path,
        &state.reviewed_commits,
        cherry_pick_sha_in(Path::new(&state.worktree_path)),
        serde_json::json!({
            "kind": "resolve_and_continue",
            "description": "Resolve conflicts in listed files, then continue the cherry-pick, then rerun integrate.",
            "commands": [
                format!("cd {}", state.worktree_path),
                edit_hint,
                format!("git add {}", add_targets),
                "git cherry-pick --continue".to_string(),
                "# return to brehon:".to_string(),
                format!("mcp_brehon_task action=integrate id={id}")
            ],
            "alternative": {
                "kind": "abort",
                "description": "If this conflict cannot be resolved, explicitly abort.",
                "command": format!("mcp_brehon_task action=abort-integration id={id} reason='...'")
            }
        }),
        serde_json::json!({
            "kind": "detect_on_retry",
            "description": "On next integrate call, tool will check CHERRY_PICK_HEAD absence + patch-equivalence. Either path transitions to `resolved`."
        }),
    );
    let mut result = result;
    result["attempts"] = Value::from(state.attempts);
    result["message"] = Value::String(format!(
        "Task {id} hit an integration conflict against '{merge_target}'. \
         Conflicting files: {}. \
         Resolve conflicts in the epic worktree, then rerun task action=integrate id={id}.",
        if state.conflicting_files.is_empty() {
            "unknown files".to_string()
        } else {
            state.conflicting_files.join(", ")
        }
    ));
    result["details"] = Value::String(details.to_string());
    Ok(error_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn integrate_state_reject_response(
    id: &str,
    state: &IntegrationState,
    current_status: &str,
    merge_target: &str,
    reason: &str,
) -> Result<ToolResult, McpError> {
    let (error_code, details, next_action_for_supervisor, next_action_for_brehon) = if reason
        .starts_with("stale cherry-pick for")
    {
        (
            "stale_cherry_pick",
            "An unexpected cherry-pick is blocking this integration attempt.",
            serde_json::json!({
                "kind": "abort_then_retry",
                "description": "Abort the stale cherry-pick state before retrying integrate.",
                "command": format!("mcp_brehon_task action=abort-integration id={id} reason='stale cherry-pick'")
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Wait for the supervisor to clear the stale cherry-pick state."
            }),
        )
    } else if reason == "cherry-pick in progress but SHA could not be read" {
        (
            "missing_cherry_pick_head",
            "Git reports an in-progress cherry-pick, but CHERRY_PICK_HEAD could not be read.",
            serde_json::json!({
                "kind": "inspect_worktree",
                "description": "Inspect the integration worktree and repair or abort the broken cherry-pick state."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Integrate cannot continue until the supervisor repairs the cherry-pick state."
            }),
        )
    } else if reason
        == "cherry-pick was cleared but commit not applied; abort-integration or resolve"
    {
        (
            "cleared_cherry_pick_not_applied",
            "The cherry-pick metadata disappeared before the reviewed commit set was applied.",
            serde_json::json!({
                "kind": "abort_or_resolve",
                "description": "Abort the abandoned integration attempt or manually resolve/apply the reviewed commits before retrying."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Wait for the supervisor to reconcile the cleared cherry-pick state."
            }),
        )
    } else if reason.starts_with("verification failed") {
        (
            "verification_failed",
            "The reviewed tree still does not match the epic branch after cherry-pick resolution.",
            serde_json::json!({
                "kind": "abort_or_fix",
                "description": "Abort integration or repair the epic worktree so the reviewed tree matches before retrying."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Integrate will keep rejecting until the reviewed tree matches the epic branch."
            }),
        )
    } else if reason
        == "integration already completed; manual revert required before force=true retry"
    {
        (
            "integration_already_completed",
            "A completed integration cannot be retried destructively without a manual revert.",
            serde_json::json!({
                "kind": "manual_revert_required",
                "description": "Manually revert the completed integration before retrying force=true."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "No further integrate action is available until the completed merge is reverted."
            }),
        )
    } else if reason == "explicitly aborted; use force=true to retry" {
        (
            "integration_aborted",
            "The supervisor explicitly aborted this integration attempt.",
            serde_json::json!({
                "kind": "force_retry",
                "description": "Use force=true to start a new integration attempt from the aborted state."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "Wait for an explicit force=true integrate request."
            }),
        )
    } else {
        (
            "integration_rejected",
            "Integrate rejected the current state machine transition.",
            serde_json::json!({
                "kind": "inspect_state",
                "description": "Inspect the integration state and retry once the task is in a supported state."
            }),
            serde_json::json!({
                "kind": "none",
                "description": "No automatic Brehon retry is available for this rejection."
            }),
        )
    };

    integrate_error_response(
        id,
        state.phase,
        current_status,
        Some(merge_target),
        &state.conflicting_files,
        &state.worktree_path,
        &state.reviewed_commits,
        error_code,
        reason,
        Some(details),
        next_action_for_supervisor,
        next_action_for_brehon,
    )
}

fn integrate_error_response(
    id: &str,
    integration_phase: IntegrationPhase,
    current_status: &str,
    merge_target: Option<&str>,
    conflicting_files: &[String],
    worktree_path: &str,
    reviewed_commits: &[String],
    error_code: &str,
    message: &str,
    details: Option<&str>,
    next_action_for_supervisor: Value,
    next_action_for_brehon: Value,
) -> Result<ToolResult, McpError> {
    let mut result = structured_integrate_response(
        id,
        integration_phase,
        "error",
        conflicting_files,
        worktree_path,
        reviewed_commits,
        (!worktree_path.is_empty())
            .then(|| cherry_pick_sha_in(Path::new(worktree_path)))
            .flatten(),
        next_action_for_supervisor,
        next_action_for_brehon,
    );
    result["action"] = Value::String("integrate".to_string());
    result["error_code"] = Value::String(error_code.to_string());
    result["current_status"] = Value::String(current_status.to_string());
    result["message"] = Value::String(message.to_string());
    if let Some(merge_target) = merge_target {
        result["merge_target"] = Value::String(merge_target.to_string());
    }
    if let Some(details) = details {
        result["details"] = Value::String(details.to_string());
    }
    Ok(error_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn structured_integrate_response(
    id: &str,
    integration_phase: IntegrationPhase,
    status: &str,
    conflicting_files: &[String],
    worktree_path: &str,
    reviewed_commits: &[String],
    cherry_pick_head: Option<String>,
    next_action_for_supervisor: Value,
    next_action_for_brehon: Value,
) -> Value {
    serde_json::json!({
        "schema_version": 1,
        "task_id": id,
        "integration_phase": integration_phase,
        "status": status,
        "conflicting_files": conflicting_files,
        "worktree_path": worktree_path,
        "reviewed_commits": reviewed_commits,
        "cherry_pick_head": cherry_pick_head,
        "next_action_for_supervisor": next_action_for_supervisor,
        "next_action_for_brehon": next_action_for_brehon
    })
}
