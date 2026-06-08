use std::path::Path;

use serde_json::Value;

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::error_result;

use super::cherry_pick_sha_in;
use super::{IntegrationPhase, IntegrationState};

pub(super) fn conflict_response(
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

pub(super) fn integrate_state_reject_response(
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

#[allow(clippy::too_many_arguments)]
pub(super) fn integrate_error_response(
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

#[allow(clippy::too_many_arguments)]
pub(super) fn structured_integrate_response(
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
