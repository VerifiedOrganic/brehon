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
    commits_refer_to_same_oid, delete_review_state, release_panel_lease_for_task,
};
use crate::tools::{error_result, text_result};

mod integration_closeout;

use integration_closeout::{
    integration_commit_metadata, reviewed_commits_with_cherry_pick_trailers,
};

use super::integration_proof::{IntegrationProofRecorder, IntegrationSuccessProof};

use super::epic::{
    check_epic_completion, continue_cherry_pick, ensure_epic_integration_worktree,
    read_current_review_request, start_cherry_pick, verify_applied,
};
use super::followups::resolve_promoted_followups_for_terminal_task;
use super::git_ops::{
    cherry_pick_in_progress_in, cherry_pick_sha_in, detect_default_branch,
    dirty_primary_checkout_terminal_blocker, git_commit_is_ancestor_in, git_run_ok_in,
    git_status_porcelain_in, is_patch_equivalent_in_window_in, tree_matches_after,
    tree_matches_after_limited, unmerged_files,
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

const CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT: usize = 100;

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

    if let Some(err) = dirty_primary_checkout_terminal_blocker(&format!("integrate task {id}")) {
        return Ok(error_result(err));
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
    let allow_latest_commit_fallback = current_integration_status == "integrated"
        || persisted_state.phase == IntegrationPhase::Aborted;
    let (reviewed_commit, reviewed_commit_set, resolved_empty_commit_set, from_review_request) =
        integration_commit_metadata(
            &task_data,
            review_request.as_ref(),
            allow_latest_commit_fallback,
        );

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
        if from_review_request && !commits_refer_to_same_oid(latest_commit, &reviewed_commit) {
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
    let mut skip_next_probe_after_force_reset = false;
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

        let probes = if force && state.phase == IntegrationPhase::Aborted {
            // Aborted + force=true always transitions to ForceReset. Running
            // full patch-equivalence probes first can spawn hundreds of git
            // processes for multi-commit integrations before doing work the
            // transition table will discard anyway.
            GitProbeResult::default()
        } else if skip_next_probe_after_force_reset && state.phase == IntegrationPhase::Null {
            skip_next_probe_after_force_reset = false;
            GitProbeResult::default()
        } else {
            run_git_probes(&integration_worktree, &state, &merge_target)?
        };
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
                skip_next_probe_after_force_reset = true;
            }

            Action::Finalize => {
                if prior_phase == IntegrationPhase::Aborted {
                    already_integrated = true;
                }
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
                match execute_cherry_picks(
                    &integration_worktree,
                    &state.reviewed_commits,
                    &state.cherry_pick_base_head,
                ) {
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
                        match execute_cherry_picks(
                            &integration_worktree,
                            &state.reviewed_commits,
                            &state.cherry_pick_base_head,
                        ) {
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
    let cherry_pick_branch_advanced =
        cherry_pick_branch_advanced_since_base(worktree, &state.cherry_pick_base_head);
    if state.phase == IntegrationPhase::CherryPicking && cherry_pick_in_progress {
        return Ok(GitProbeResult {
            cherry_pick_in_progress,
            cherry_pick_sha,
            unmerged_files,
            cherry_pick_branch_advanced,
            ..GitProbeResult::default()
        });
    }

    // Empty set: vacuously true — no reviewed commits to probe means the
    // whole-set conditions already hold.
    let mut is_ancestor = true;
    let mut is_patch_equivalent = true;
    let mut has_reviewed_cherry_pick_trailers = !state.reviewed_commits.is_empty();
    let mut reviewed_commits_applied = true;
    let mut tree_matches_after_all = true;
    let probe_tree_matches = state.phase == IntegrationPhase::Resolved;
    let probe_cleared_cherry_pick_tree_matches = matches!(
        state.phase,
        IntegrationPhase::CherryPicking | IntegrationPhase::Aborted
    ) && !cherry_pick_in_progress
        && unmerged_files.is_empty()
        && git_status_porcelain_in(worktree)
            .map(|status| status.trim().is_empty())
            .unwrap_or(false);
    let reviewed_commits_with_trailers = reviewed_commits_with_cherry_pick_trailers(
        worktree,
        branch,
        &state.cherry_pick_base_head,
        &state.reviewed_commits,
    );

    for commit in &state.reviewed_commits {
        let commit_is_ancestor =
            git_commit_is_ancestor_in(worktree, commit, branch).unwrap_or(false);
        let commit_is_patch_equivalent = commit_is_ancestor
            || is_patch_equivalent_in_window_in(worktree, commit, branch, 50).unwrap_or(false);
        let commit_has_cherry_pick_trailer = reviewed_commits_with_trailers.contains(commit);
        let commit_has_cheap_proof = commit_is_patch_equivalent || commit_has_cherry_pick_trailer;
        let commit_tree_matches = if commit_has_cheap_proof {
            true
        } else if probe_tree_matches {
            tree_matches_after(worktree, commit, branch).unwrap_or(false)
        } else if probe_cleared_cherry_pick_tree_matches {
            tree_matches_after_limited(
                worktree,
                commit,
                branch,
                CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT,
            )
            .unwrap_or(None)
            .unwrap_or(false)
        } else {
            false
        };
        let commit_is_applied = commit_has_cheap_proof || commit_tree_matches;

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
        cherry_pick_branch_advanced,
        is_ancestor,
        is_patch_equivalent,
        has_reviewed_cherry_pick_trailers,
        reviewed_commits_applied,
        tree_matches_after: tree_matches_after_all,
    })
}

fn cherry_pick_branch_advanced_since_base(worktree: &Path, base_head: &str) -> bool {
    if base_head.is_empty() {
        return false;
    }
    let Ok(head) = git_stdout_in(worktree, &["rev-parse", "HEAD"]) else {
        return false;
    };
    head != base_head && git_commit_is_ancestor_in(worktree, base_head, "HEAD").unwrap_or(false)
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

#[derive(Debug)]
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
    cherry_pick_base_head: &str,
) -> Result<(), CherryPickError> {
    let commits_with_trailers = reviewed_commits_with_cherry_pick_trailers(
        worktree,
        "HEAD",
        cherry_pick_base_head,
        reviewed_commits,
    );
    for commit in reviewed_commits {
        if commit_already_represented_for_pick_with_trailers(
            worktree,
            commit,
            &commits_with_trailers,
        )
        .map_err(CherryPickError::Other)?
        {
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

fn commit_already_represented_for_pick_with_trailers(
    worktree: &Path,
    commit: &str,
    commits_with_trailers: &HashSet<String>,
) -> Result<bool, String> {
    if commits_with_trailers.contains(commit) {
        return Ok(true);
    }
    if verify_applied(worktree, commit, "HEAD")? {
        return Ok(true);
    }
    if is_patch_equivalent_in_window_in(worktree, commit, "HEAD", 50).unwrap_or(false) {
        return Ok(true);
    }
    Ok(tree_matches_after_limited(
        worktree,
        commit,
        "HEAD",
        CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT,
    )?
    .unwrap_or(false))
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
    final_state.resolution = None;
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
    result["worktree_cleanup"] =
        super::build_artifact_cleanup::cleanup_brehon_worktree_allowlisted_artifacts(
            "after_task_integrated",
            integration_worktree,
        );

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
    } else if reason
        == "explicitly aborted and reviewed commit set is not present on merge target; use force=true to retry"
    {
        (
            "integration_aborted",
            "The supervisor explicitly aborted this integration attempt, and the reviewed commit set is not already present on the merge target.",
            serde_json::json!({
                "kind": "force_retry",
                "description": "Use force=true only if the supervisor intends to start a new integration attempt from the aborted state."
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Output};

    fn run_git(cwd: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git {}: {err}", args.join(" ")))
    }

    fn run_git_ok(cwd: &Path, args: &[&str]) -> String {
        let output = run_git(cwd, args);
        assert!(
            output.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn cherry_picking_probe_short_circuits_unmerged_conflict() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cwd = repo.path();
        run_git_ok(cwd, &["init", "-b", "main"]);
        run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
        run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

        std::fs::write(cwd.join("conflict.txt"), "base\n").unwrap();
        run_git_ok(cwd, &["add", "conflict.txt"]);
        run_git_ok(cwd, &["commit", "-m", "base"]);
        run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
        std::fs::write(cwd.join("conflict.txt"), "reviewed\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "reviewed change"]);
        let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "main"]);
        std::fs::write(cwd.join("conflict.txt"), "main\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "main change"]);
        let cherry_pick = run_git(cwd, &["cherry-pick", &reviewed_sha]);
        assert!(
            !cherry_pick.status.success(),
            "cherry-pick should create a conflict"
        );

        let state = IntegrationState {
            phase: IntegrationPhase::CherryPicking,
            ..IntegrationState::default()
        };
        let probes =
            run_git_probes(cwd, &state, "missing-branch").expect("cheap cherry-pick probes");

        assert!(probes.cherry_pick_in_progress);
        assert_eq!(
            probes.cherry_pick_sha.as_deref(),
            Some(reviewed_sha.as_str())
        );
        assert_eq!(probes.unmerged_files, vec!["conflict.txt"]);
        assert!(!probes.is_ancestor);
        assert!(!probes.is_patch_equivalent);
    }

    #[test]
    fn cleared_cherry_picking_probe_accepts_bounded_tree_match_fallback() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cwd = repo.path();
        run_git_ok(cwd, &["init", "-b", "main"]);
        run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
        run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

        std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
        run_git_ok(cwd, &["add", "needle.txt"]);
        run_git_ok(cwd, &["commit", "-m", "base"]);
        let base_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
        std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "reviewed change"]);
        let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "main"]);
        std::fs::write(cwd.join("needle.txt"), "intermediate\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "main intermediate"]);
        std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "main same final content"]);

        let state = IntegrationState {
            phase: IntegrationPhase::CherryPicking,
            reviewed_commits: vec![reviewed_sha.clone()],
            cherry_pick_base_head: base_sha,
            ..IntegrationState::default()
        };
        let probes = run_git_probes(cwd, &state, "main").expect("stale cherry-pick probes");

        assert!(!probes.cherry_pick_in_progress);
        assert!(probes.cherry_pick_branch_advanced);
        assert!(!probes.is_ancestor);
        assert!(!probes.is_patch_equivalent);
        assert!(!probes.has_reviewed_cherry_pick_trailers);
        assert!(probes.reviewed_commits_applied);
        assert!(
            probes.tree_matches_after,
            "clean cleared cherry-picking state should use bounded tree matching"
        );

        let resolved_state = IntegrationState {
            phase: IntegrationPhase::Resolved,
            reviewed_commits: vec![reviewed_sha],
            ..IntegrationState::default()
        };
        let resolved_probes =
            run_git_probes(cwd, &resolved_state, "main").expect("resolved probes");
        assert!(
            resolved_probes.tree_matches_after,
            "resolved verification still uses the tree fallback"
        );
    }

    #[test]
    fn cleared_cherry_picking_probe_caps_tree_match_fallback() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cwd = repo.path();
        run_git_ok(cwd, &["init", "-b", "main"]);
        run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
        run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

        std::fs::write(cwd.join("README.md"), "base\n").unwrap();
        run_git_ok(cwd, &["add", "README.md"]);
        run_git_ok(cwd, &["commit", "-m", "base"]);

        run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
        for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
            std::fs::write(cwd.join(format!("file-{idx}.txt")), "target\n").unwrap();
        }
        run_git_ok(cwd, &["add", "."]);
        run_git_ok(cwd, &["commit", "-m", "reviewed large change"]);
        let reviewed_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "main"]);
        for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
            std::fs::write(cwd.join(format!("file-{idx}.txt")), "intermediate\n").unwrap();
        }
        run_git_ok(cwd, &["add", "."]);
        run_git_ok(cwd, &["commit", "-m", "main intermediate files"]);
        for idx in 0..=CLEARED_CHERRY_PICK_TREE_MATCH_FILE_LIMIT {
            std::fs::write(cwd.join(format!("file-{idx}.txt")), "target\n").unwrap();
        }
        run_git_ok(cwd, &["commit", "-am", "main same large final content"]);

        let state = IntegrationState {
            phase: IntegrationPhase::CherryPicking,
            reviewed_commits: vec![reviewed_sha],
            ..IntegrationState::default()
        };
        let probes = run_git_probes(cwd, &state, "main").expect("capped cherry-pick probes");

        assert!(!probes.cherry_pick_in_progress);
        assert!(!probes.is_ancestor);
        assert!(!probes.is_patch_equivalent);
        assert!(!probes.has_reviewed_cherry_pick_trailers);
        assert!(
            !probes.reviewed_commits_applied,
            "large fallback must not count as applied without cheap proof"
        );
        assert!(
            !probes.tree_matches_after,
            "large fallback must stay capped to avoid expensive retry probes"
        );
    }

    #[test]
    fn execute_cherry_picks_skips_represented_commit_and_continues_remaining() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cwd = repo.path();
        run_git_ok(cwd, &["init", "-b", "main"]);
        run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
        run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

        std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
        run_git_ok(cwd, &["add", "needle.txt"]);
        run_git_ok(cwd, &["commit", "-m", "base"]);

        run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
        std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "reviewed first"]);
        let first_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);
        std::fs::write(cwd.join("second.txt"), "second\n").unwrap();
        run_git_ok(cwd, &["add", "second.txt"]);
        run_git_ok(cwd, &["commit", "-m", "reviewed second"]);
        let second_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "main"]);
        std::fs::write(cwd.join("needle.txt"), "target\nextra\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "resolved first with extra context"]);

        execute_cherry_picks(cwd, &[first_sha.clone(), second_sha.clone()], "")
            .expect("should skip represented first commit and continue second");

        assert_eq!(
            run_git_ok(cwd, &["show", "HEAD:needle.txt"]),
            "target\nextra"
        );
        assert_eq!(run_git_ok(cwd, &["show", "HEAD:second.txt"]), "second");
        assert!(
            !git_commit_is_ancestor_in(cwd, &first_sha, "HEAD").unwrap(),
            "first commit should be represented without re-picking its SHA"
        );
        let latest_message = run_git_ok(cwd, &["log", "-1", "--format=%B"]);
        assert!(
            latest_message.contains(&second_sha),
            "second commit should be cherry-picked with provenance trailer"
        );
    }

    #[test]
    fn execute_cherry_picks_skips_commit_with_trailer_and_continues_remaining() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cwd = repo.path();
        run_git_ok(cwd, &["init", "-b", "main"]);
        run_git_ok(cwd, &["config", "user.email", "brehon@example.invalid"]);
        run_git_ok(cwd, &["config", "user.name", "Brehon Test"]);

        std::fs::write(cwd.join("needle.txt"), "base\n").unwrap();
        run_git_ok(cwd, &["add", "needle.txt"]);
        run_git_ok(cwd, &["commit", "-m", "base"]);
        let base_head = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "-b", "reviewed"]);
        std::fs::write(cwd.join("needle.txt"), "target\n").unwrap();
        run_git_ok(cwd, &["commit", "-am", "reviewed first"]);
        let first_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);
        std::fs::write(cwd.join("second.txt"), "second\n").unwrap();
        run_git_ok(cwd, &["add", "second.txt"]);
        run_git_ok(cwd, &["commit", "-m", "reviewed second"]);
        let second_sha = run_git_ok(cwd, &["rev-parse", "HEAD"]);

        run_git_ok(cwd, &["checkout", "main"]);
        std::fs::write(cwd.join("needle.txt"), "manual resolution\n").unwrap();
        let trailer = format!("(cherry picked from commit {first_sha})");
        run_git_ok(
            cwd,
            &[
                "commit",
                "-am",
                "manual first resolution with trailer",
                "-m",
                trailer.as_str(),
            ],
        );

        execute_cherry_picks(cwd, &[first_sha.clone(), second_sha.clone()], &base_head)
            .expect("should skip trailer-proven first commit and continue second");

        assert_eq!(
            run_git_ok(cwd, &["show", "HEAD:needle.txt"]),
            "manual resolution"
        );
        assert_eq!(run_git_ok(cwd, &["show", "HEAD:second.txt"]), "second");
        assert!(
            !git_commit_is_ancestor_in(cwd, &first_sha, "HEAD").unwrap(),
            "first commit should be represented by trailer without re-picking its SHA"
        );
        let latest_message = run_git_ok(cwd, &["log", "-1", "--format=%B"]);
        assert!(
            latest_message.contains(&second_sha),
            "second commit should still be cherry-picked after skipping trailer-proven first"
        );
    }
}
