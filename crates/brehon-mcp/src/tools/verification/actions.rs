//! Review action handlers — one method per MCP `action` value.
//!
//! Each handler is an `async fn` on `VerificationTool` that validates inputs,
//! mutates review / task state, fires events, and returns a `ToolResult`.

use std::collections::HashMap;

use serde_json::Value;

use brehon_types::{is_terminal_task_status, normalize_task_status, EventKind, TaskCompletionMode};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::{error_result, text_result};

use super::commits::{preview_commit_integration_conflicts, resolve_review_commit_set};
use super::helpers::{
    commits_refer_to_same_oid, current_git_head, git_output_in, reviews_dir, workspace_root,
};
use super::notifications::{
    enqueue_reviewer_session_reset, notify_agent, notify_review_stakeholders,
};
use super::panel::{
    delete_panel_lease, find_panel_lease_by_task, write_panel_lease, PanelLeaseState,
};
use super::review_prompt::{build_review_request_prompt, ReviewRequestPromptInput};
use super::scoring::{
    build_override_feedback, build_task_review_feedback, build_task_review_followups,
    format_worker_feedback_message, is_supported_review_verdict, task_status_for_review_outcome,
    unsupported_negative_review_reason,
};
use super::state::{
    acquire_review_lock, current_review_cycle_round, current_review_epoch_round,
    delete_review_state, find_review_request_by_id, highest_round_on_disk,
    next_review_round_would_exceed_total_limit, read_review_state, read_round_request,
    read_round_submissions, review_cycle_round, review_epoch_round, total_review_round_limit,
    total_review_rounds_exhausted, write_review_state, write_round_request, write_submission,
    ReviewRequestFile, ReviewState, StoredFinding, StoredSubmission,
};
use super::tasks::{
    detect_default_branch, merge_target_requires_epic_integration, read_task, read_task_assignee,
    read_task_completion_mode, read_task_merge_target, read_task_recorded_commit, read_task_status,
};
use super::tool::{RejectionReason, VerificationTool};

use crate::tools::task_actions::{
    append_task_review_followups, clear_task_supervisor_integration_conflict,
    mark_task_supervisor_integration_conflict, release_task_worker_to_review,
    restore_task_worker_from_review_owner, set_task_review_feedback,
    task_has_integration_conflict_recovery_marker, update_task_status_atomic,
};

use super::panel::release_panel_lease_for_task;

const HANDOFF_SUPERVISOR_CONTEXT_MAX_CHARS: usize = 2_400;
const HANDOFF_FREEFORM_MAX_CHARS: usize = 1_800;
const HANDOFF_METADATA_VALUE_MAX_CHARS: usize = 320;
const HANDOFF_BULLET_ITEM_MAX_CHARS: usize = 360;
const HANDOFF_FINDING_ITEM_MAX_CHARS: usize = 640;
const HANDOFF_TOTAL_MAX_CHARS: usize = 12_000;

fn task_status_closes_review(status: Option<&str>) -> bool {
    status.is_some_and(|status| {
        is_terminal_task_status(status) || matches!(status.trim(), "archived" | "Archived")
    })
}

fn review_livelock_guard_message(
    task_id: &str,
    consumed_epoch_rounds: u32,
    attempted_round: u32,
    max_rounds: u8,
) -> String {
    let total_limit = total_review_round_limit(max_rounds);
    format!(
        "Task {task_id} has consumed {consumed_epoch_rounds}/{total_limit} review rounds in the current review epoch. \
         Refusing to start/reset review round {attempted_round}; this is review livelock, not a normal re-review cycle. \
         Block the task for supervisor/manual intervention and fix the task before requesting another round. \
         A new worker checkpoint/commit starts a fresh review epoch automatically; for non-commit work, a supervisor may run reset_rounds with force_new_epoch=true and an explicit reason."
    )
}

fn enqueue_reviewer_reset_with_logging(task_id: &str, review_id: &str, reviewer: &str) -> bool {
    match enqueue_reviewer_session_reset(task_id, review_id, reviewer) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(
                task_id = %task_id,
                review_id = %review_id,
                reviewer = %reviewer,
                error = %err,
                "Failed to enqueue reviewer session reset"
            );
            false
        }
    }
}

fn parse_review_findings_arg(value: Option<&Value>) -> Result<Vec<StoredFinding>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    if value.is_array() {
        return serde_json::from_value(value.clone())
            .map_err(|err| format!("Invalid findings array: {err}"));
    }

    if let Some(raw) = value.as_str() {
        return serde_json::from_str(raw)
            .map_err(|err| format!("Invalid findings JSON string: {err}"));
    }

    Err("findings must be a JSON array or a JSON string containing an array".to_string())
}

fn task_str<'a>(task: &'a Value, key: &str) -> Option<&'a str> {
    task.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn task_string_list(task: &Value, key: &str) -> Vec<String> {
    task.get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn truncate_handoff_text(value: &str, max_chars: usize) -> (String, usize) {
    let value = value.trim();
    let total = value.chars().count();
    if total <= max_chars {
        return (value.to_string(), 0);
    }

    let keep = max_chars.saturating_sub(3);
    let mut truncated: String = value.chars().take(keep).collect();
    truncated.push_str("...");
    (truncated, total.saturating_sub(keep))
}

fn append_text_section(out: &mut String, title: &str, value: &str, max_chars: usize) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }

    let (value, omitted_chars) = truncate_handoff_text(value, max_chars);
    out.push_str(title);
    out.push_str(":\n");
    out.push_str(&value);
    if omitted_chars > 0 {
        out.push_str(&format!(
            "\n[... {omitted_chars} chars omitted from handoff; inspect task metadata for full detail]"
        ));
    }
    out.push_str("\n\n");
}

fn append_bullet_section(
    out: &mut String,
    title: &str,
    items: &[String],
    max_items: usize,
    max_item_chars: usize,
) {
    if items.is_empty() {
        return;
    }
    out.push_str(title);
    out.push_str(":\n");
    for item in items.iter().take(max_items) {
        let (item, omitted_chars) = truncate_handoff_text(item, max_item_chars);
        out.push_str("- ");
        out.push_str(&item);
        if omitted_chars > 0 {
            out.push_str(&format!(" [... {omitted_chars} chars omitted]"));
        }
        out.push('\n');
    }
    let omitted_items = items.len().saturating_sub(max_items);
    if omitted_items > 0 {
        out.push_str(&format!(
            "- ... {omitted_items} more omitted from handoff; inspect task metadata for full detail\n"
        ));
    }
    out.push('\n');
}

fn finding_line(finding: &Value) -> Option<String> {
    let description = finding
        .get("description")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let severity = finding
        .get("severity")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("finding");
    let location = match (
        finding.get("file").and_then(|value| value.as_str()),
        finding.get("line").and_then(|value| value.as_u64()),
    ) {
        (Some(file), Some(line)) if !file.trim().is_empty() => {
            format!(" [{}:{line}]", file.trim())
        }
        (Some(file), None) if !file.trim().is_empty() => format!(" [{}]", file.trim()),
        _ => String::new(),
    };
    let suggestion = finding
        .get("suggestion")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!(" Suggestion: {value}"))
        .unwrap_or_default();
    Some(format!("{severity}{location}: {description}{suggestion}"))
}

fn review_feedback_finding_lines(feedback: &Value, key: &str) -> Vec<String> {
    feedback
        .get(key)
        .and_then(|value| value.as_array())
        .map(|findings| findings.iter().filter_map(finding_line).collect())
        .unwrap_or_default()
}

fn append_previous_review_feedback(out: &mut String, feedback: &Value) {
    out.push_str("Previous review feedback to verify:\n");
    out.push_str(
        "- review_scope_note: these are historical claims, not current evidence. \
         Re-check each claim against the exact review commit only. Do not copy findings from \
         local staged, unstaged, or uncommitted working-tree state.\n",
    );
    for (label, key) in [
        ("review_id", "review_id"),
        ("round", "round"),
        ("outcome", "outcome"),
        ("threshold_result", "threshold_result"),
        ("threshold_reason", "threshold_reason"),
    ] {
        if let Some(value) = feedback.get(key) {
            if let Some(text) = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                out.push_str("- ");
                out.push_str(label);
                out.push_str(": ");
                let (text, omitted_chars) =
                    truncate_handoff_text(text, HANDOFF_METADATA_VALUE_MAX_CHARS);
                out.push_str(&text);
                if omitted_chars > 0 {
                    out.push_str(&format!(" [... {omitted_chars} chars omitted]"));
                }
                out.push('\n');
            } else if let Some(number) = value.as_u64() {
                out.push_str("- ");
                out.push_str(label);
                out.push_str(": ");
                out.push_str(&number.to_string());
                out.push('\n');
            }
        }
    }
    out.push('\n');
    append_bullet_section(
        out,
        "Prior blocking findings",
        &review_feedback_finding_lines(feedback, "blocking"),
        24,
        HANDOFF_FINDING_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        out,
        "Prior suggestions",
        &review_feedback_finding_lines(feedback, "suggestions"),
        16,
        HANDOFF_FINDING_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        out,
        "Prior nitpicks",
        &review_feedback_finding_lines(feedback, "nitpicks"),
        8,
        HANDOFF_FINDING_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        out,
        "Prior dissent",
        &task_string_list(feedback, "dissent"),
        8,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );
}

fn finalize_handoff_context(out: String) -> String {
    let (context, omitted_chars) = truncate_handoff_text(out.trim(), HANDOFF_TOTAL_MAX_CHARS);
    if omitted_chars == 0 {
        context
    } else {
        format!(
            "{context}\n\n[... {omitted_chars} chars omitted from review handoff; inspect task metadata/review artifacts for full detail]"
        )
    }
}

fn build_review_handoff_context(task: &Value, requested_context: &str) -> String {
    let mut out = String::new();
    let requested_context = requested_context.trim();
    if !requested_context.is_empty() {
        append_text_section(
            &mut out,
            "Supervisor context",
            requested_context,
            HANDOFF_SUPERVISOR_CONTEXT_MAX_CHARS,
        );
    }

    let mut metadata = Vec::new();
    for (label, key) in [
        ("task_status", "status"),
        ("completion_mode", "completion_mode"),
        ("worker", "review_owner"),
        ("assignee", "assignee"),
        ("branch", "branch"),
        ("merge_target", "merge_target"),
        ("latest_commit", "latest_commit"),
        ("activity", "activity"),
    ] {
        if let Some(value) = task_str(task, key) {
            let (value, omitted_chars) =
                truncate_handoff_text(value, HANDOFF_METADATA_VALUE_MAX_CHARS);
            let suffix = if omitted_chars > 0 {
                format!(" [... {omitted_chars} chars omitted]")
            } else {
                String::new()
            };
            metadata.push(format!("{label}: {value}{suffix}"));
        }
    }
    if !metadata.is_empty() {
        out.push_str("Task handoff snapshot:\n");
        for line in metadata {
            out.push_str("- ");
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }

    if let Some(feedback) = task.get("review_feedback") {
        append_previous_review_feedback(&mut out, feedback);
    }

    if let Some(notes) = task_str(task, "notes") {
        append_text_section(
            &mut out,
            "Worker completion notes",
            notes,
            HANDOFF_FREEFORM_MAX_CHARS,
        );
    }

    append_bullet_section(
        &mut out,
        "Acceptance criteria",
        &task_string_list(task, "acceptance_criteria"),
        16,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        &mut out,
        "File hints",
        &task_string_list(task, "file_hints"),
        32,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        &mut out,
        "Test requirements",
        &task_string_list(task, "test_requirements"),
        16,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        &mut out,
        "Plan",
        &task_string_list(task, "plan_steps"),
        20,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );
    append_bullet_section(
        &mut out,
        "Dependencies",
        &task_string_list(task, "dependencies"),
        16,
        HANDOFF_BULLET_ITEM_MAX_CHARS,
    );

    if let Some(notes) = task_str(task, "implementation_notes") {
        append_text_section(
            &mut out,
            "Implementation notes",
            notes,
            HANDOFF_FREEFORM_MAX_CHARS,
        );
    }

    finalize_handoff_context(out)
}

async fn cleanup_review_artifacts_for_supervisor_conflict(
    task_id: &str,
) -> Result<Option<String>, String> {
    let _lock = acquire_review_lock(task_id)
        .await
        .map_err(|err| format!("Failed to lock review state for task {task_id}: {err}"))?;
    let released_panel = release_panel_lease_for_task(task_id)?;
    delete_review_state(task_id)
        .map_err(|err| format!("Failed to clear stale review state for task {task_id}: {err}"))?;
    Ok(released_panel)
}

fn stable_fnv1a64_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn diff_stat_fingerprint(base_commit: &str, review_commit: &str) -> (usize, Value) {
    let base_commit = base_commit.trim();
    let review_commit = review_commit.trim();
    if base_commit.is_empty() || review_commit.is_empty() {
        return (0, Value::Null);
    }
    let Some(root) = workspace_root() else {
        return (0, Value::Null);
    };
    let diff_stat = match git_output_in(
        &root,
        &[
            "diff",
            "--numstat",
            "--find-renames",
            base_commit,
            review_commit,
        ],
    ) {
        Ok(output) => output,
        Err(err) => {
            tracing::warn!(
                base_commit = %base_commit,
                review_commit = %review_commit,
                error = %err,
                "Failed to compute review diff-stat fingerprint"
            );
            return (0, Value::Null);
        }
    };
    let file_count = diff_stat
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    (file_count, Value::String(stable_fnv1a64_hex(&diff_stat)))
}

fn build_review_fingerprint(
    task_id: &str,
    review_id: &str,
    round: u32,
    review_commit: &str,
    reviewed_commit_set: &super::commits::ResolvedReviewCommitSet,
    resolved_empty_commit_set: bool,
) -> Value {
    let diff_base = if !reviewed_commit_set.base_commit.trim().is_empty() {
        reviewed_commit_set.base_commit.as_str()
    } else {
        reviewed_commit_set.merge_target_head.as_str()
    };
    let (diff_file_count, diff_stat_hash) = diff_stat_fingerprint(diff_base, review_commit);
    serde_json::json!({
        "task_id": task_id,
        "review_id": review_id,
        "review_round": round,
        "review_commit": review_commit,
        "base_commit": &reviewed_commit_set.base_commit,
        "merge_target_head": &reviewed_commit_set.merge_target_head,
        "reviewed_commits": &reviewed_commit_set.commits,
        "reviewed_commit_count": reviewed_commit_set.commits.len(),
        "resolved_empty_commit_set": resolved_empty_commit_set,
        "diff_file_count": diff_file_count,
        "diff_stat_hash": diff_stat_hash
    })
}

fn next_action_request_review(task_id: &str) -> Value {
    serde_json::json!({
        "kind": "request_review",
        "tool": "verification",
        "args": {
            "action": "request_review",
            "task_id": task_id
        }
    })
}

fn next_action_review_status(task_id: &str) -> Value {
    serde_json::json!({
        "kind": "wait_for_reviews",
        "tool": "verification",
        "args": {
            "action": "review_status",
            "task_id": task_id
        }
    })
}

fn next_action_after_review_outcome(task_id: &str, outcome: &str) -> Value {
    match outcome {
        "approved" => match read_task_completion_mode(task_id) {
            TaskCompletionMode::Close => serde_json::json!({
                "kind": "close_approved_task",
                "tool": "task",
                "args": {
                    "action": "close",
                    "id": task_id
                }
            }),
            TaskCompletionMode::Merge if merge_target_requires_epic_integration(task_id) => {
                serde_json::json!({
                    "kind": "integrate_approved_task",
                    "tool": "task",
                    "args": {
                        "action": "integrate",
                        "id": task_id
                    }
                })
            }
            TaskCompletionMode::Merge => serde_json::json!({
                "kind": "close_approved_direct_merge_task",
                "tool": "task",
                "args": {
                    "action": "close",
                    "id": task_id
                }
            }),
        },
        "changes_requested" | "rejected" => serde_json::json!({
            "kind": "assign_revision_worker",
            "tool": "factory",
            "args": {
                "action": "assign_workers",
                "task_id": task_id
            },
            "requires": ["workers"]
        }),
        "escalated" => serde_json::json!({
            "kind": "supervisor_intervention_required",
            "tool": "verification",
            "args": {
                "action": "review_status",
                "task_id": task_id
            },
            "requires": ["supervisor"]
        }),
        _ => serde_json::json!({
            "kind": "none"
        }),
    }
}

fn reviewed_commit_changed_for_new_epoch(
    task_id: &str,
    state: &ReviewState,
    next_commit: &str,
) -> bool {
    let next_commit = next_commit.trim();
    if next_commit.is_empty() {
        return false;
    }
    let last_round = state.current_round.max(highest_round_on_disk(task_id));
    let previous_request = read_round_request(task_id, state.current_round)
        .or_else(|| read_round_request(task_id, last_round));
    let Some(previous_commit) = previous_request
        .as_ref()
        .map(|request| request.commit.trim())
        .filter(|commit| !commit.is_empty())
    else {
        return false;
    };

    !commits_refer_to_same_oid(previous_commit, next_commit)
}

impl VerificationTool {
    pub(super) async fn handle_request_review(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };
        let caller_role = args
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var("BREHON_AGENT_ROLE")
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(caller_role.as_str(), "reviewer" | "worker") {
            return Ok(error_result(
                "Only supervisors or Brehon maintenance can request reviews. \
                 Reviewers must use verification action=submit_review for assigned review obligations \
                 and must not reseat, reassign, release, reset, override, or request review panels.",
            ));
        }

        // Validate task exists
        let task = match read_task(task_id) {
            Some(task) => task,
            None => return Ok(error_result(format!("Task {task_id} not found"))),
        };

        let task_type = task
            .get("task_type")
            .and_then(|value| value.as_str())
            .unwrap_or("task");
        if matches!(task_type, "epic" | "initiative") {
            return Ok(error_result(format!(
                "Task {task_id} is a {task_type}. Container tasks are planning units and cannot enter code review."
            )));
        }

        let task_status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let normalized_task_status = normalize_task_status(task_status);
        let blocked_integration_conflict_recovery = normalized_task_status == Some("blocked")
            && task
                .as_object()
                .is_some_and(task_has_integration_conflict_recovery_marker);
        match normalized_task_status {
            Some("pending" | "assigned" | "approved" | "merged" | "closed") => {
                return Ok(error_result(format!(
                    "Task {task_id} is in status '{task_status}' and cannot enter review from that gate."
                )))
            }
            Some("blocked") if !blocked_integration_conflict_recovery => {
                return Ok(error_result(format!(
                    "Task {task_id} is in status '{task_status}' and cannot enter review from that gate."
                )))
            }
            Some("in_progress" | "review_ready" | "in_review" | "changes_requested") => {}
            Some("blocked") => {}
            _ => {
                return Ok(error_result(format!(
                    "Task {task_id} has unknown status '{task_status}'. Refusing to start review."
                )))
            }
        }

        let task_title = task_str(&task, "title").unwrap_or("(untitled)");
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(task_title);
        let task_description = task_str(&task, "description").unwrap_or("");
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(task_description);
        let requested_commit = args
            .get("commit")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let completion_mode = read_task_completion_mode(task_id);
        let recorded_commit = read_task_recorded_commit(task_id);
        let mut commit = requested_commit.clone();
        if completion_mode == TaskCompletionMode::Merge {
            if let Some(recorded_commit) = recorded_commit {
                if !requested_commit.is_empty()
                    && !commits_refer_to_same_oid(&requested_commit, &recorded_commit)
                {
                    return Ok(error_result(format!(
                        "Refusing stale review commit for task {task_id}: request supplied commit {requested_commit}, \
                         but the task's authoritative latest_commit is {recorded_commit}. \
                         Omit commit= to use latest_commit, or have the assigned worker run task action=checkpoint/complete so task state records the new SHA before review."
                    )));
                }
                commit = recorded_commit;
            } else if commit.is_empty() {
                commit = match current_git_head() {
                    Some(head) => head,
                    None => {
                        return Ok(error_result(
                            "Merge-mode reviews require a commit hash. Pass commit=<hash>, report worker progress from a git worktree, or run from a git workspace with HEAD available.",
                        ))
                    }
                };
            }
        }
        let mut reviewed_commit_set = super::commits::ResolvedReviewCommitSet::default();
        if !commit.is_empty() && completion_mode == TaskCompletionMode::Merge {
            let merge_target = read_task_merge_target(task_id)
                .or_else(detect_default_branch)
                .unwrap_or_else(|| "main".to_string());
            reviewed_commit_set =
                resolve_review_commit_set(task_id, &merge_target, &commit).unwrap_or_default();
            match preview_commit_integration_conflicts(
                task_id,
                &merge_target,
                &reviewed_commit_set.commits,
                reviewed_commit_set.commit_set_resolved,
                &commit,
            ) {
                Ok(conflicts) if !conflicts.is_empty() => {
                    if let Err(err) = mark_task_supervisor_integration_conflict(
                        task_id,
                        "changes_requested",
                        &merge_target,
                        &commit,
                        &reviewed_commit_set.commits,
                        &conflicts,
                        "review_preflight",
                        task.get("assignee").and_then(|value| value.as_str()),
                    )
                    .await
                    {
                        return Ok(error_result(format!(
                            "Cannot request review for task {task_id}: reviewed commit {commit} does not integrate cleanly into '{merge_target}' and Brehon failed to persist the supervisor-owned conflict state: {err}"
                        )));
                    }
                    let released_panel = match cleanup_review_artifacts_for_supervisor_conflict(
                        task_id,
                    )
                    .await
                    {
                        Ok(panel) => panel,
                        Err(err) => {
                            return Ok(error_result(format!(
                                    "Cannot request review for task {task_id}: reviewed commit {commit} does not integrate cleanly into '{merge_target}'. The task was moved into a supervisor-owned integration conflict, but Brehon failed to clear stale review state: {err}"
                                )));
                        }
                    };
                    return Ok(error_result(format!(
                        "Cannot request review for task {task_id}: reviewed commit {commit} does not integrate cleanly into '{merge_target}'. \
                         Rebase the task branch onto '{merge_target}', resolve the listed conflicts locally, and re-request review — the conflict marker auto-clears on the next clean preflight. \
                         If rebase repeatedly fails, escalate to the supervisor.{} Conflicting files: {}",
                        released_panel
                            .as_deref()
                            .map(|panel| format!(" Stale review panel {panel} was released."))
                            .unwrap_or_default(),
                        conflicts.join(", ")
                    )));
                }
                Ok(_) => {}
                Err(err) => return Ok(error_result(err)),
            }
        }
        let requested_context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");
        let context = build_review_handoff_context(&task, requested_context);

        let requested_by = args
            .get("requested_by")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "supervisor".to_string());

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        let now = chrono::Utc::now().to_rfc3339();

        // Check for existing active review -- support re-review (next round)
        let mut superseded_notice: Option<(String, Vec<String>, String)> = None;
        let existing_state = read_review_state(task_id);
        let stored_round = highest_round_on_disk(task_id);

        if let Some(state) = existing_state.as_ref() {
            if state.status == "collecting" {
                let recovery_hint = if find_panel_lease_by_task(task_id).is_none() {
                    " The review state has no active panel lease. Run: verification action=reseat_panel task_id=<id>"
                } else {
                    ""
                };
                return Ok(error_result(format!(
                    "Task {task_id} already has an active review (round {}). \
                     Wait for current round to complete.{recovery_hint}",
                    state.current_round,
                )));
            }
        }

        let next_round = existing_state
            .as_ref()
            .map(|state| state.current_round.max(stored_round) + 1)
            .unwrap_or(stored_round + 1);
        let next_cycle_round = existing_state
            .as_ref()
            .map(|state| review_cycle_round(state, next_round))
            .unwrap_or(1);
        let max_rounds = existing_state
            .as_ref()
            .map(|state| state.max_rounds)
            .unwrap_or(self.config.policy.max_review_rounds);
        let mut starts_new_review_epoch = existing_state.is_none();
        if let Some(state) = existing_state.as_ref() {
            if next_review_round_would_exceed_total_limit(state, next_round) {
                if reviewed_commit_changed_for_new_epoch(task_id, state, &commit) {
                    starts_new_review_epoch = true;
                } else {
                    let consumed_epoch_round =
                        review_epoch_round(state, next_round.saturating_sub(1));
                    return Ok(error_result(review_livelock_guard_message(
                        task_id,
                        consumed_epoch_round,
                        next_round,
                        max_rounds,
                    )));
                }
            }
        }
        if !starts_new_review_epoch && next_cycle_round > max_rounds as u32 {
            return Ok(error_result(format!(
                "Task {task_id} has exhausted its current review cycle ({} round limit reached). \
                 Supervisor must decide whether to reset review rounds or apply a negative override before continuing. \
                 Use verification action=reset_rounds task_id={task_id} reason=\"...\" to permit a fresh cycle, \
                 or verification action=override verdict=needs_revision|rejected reason=\"...\" to keep the task blocked.",
                max_rounds
            )));
        }

        if let Some(state) = existing_state.as_ref() {
            let pending_reviewers = Self::pending_panel_reviewers(state);
            if !pending_reviewers.is_empty() {
                superseded_notice = Some((
                    state.current_review_id.clone(),
                    pending_reviewers,
                    state.status.clone(),
                ));
            }
        }

        let new_review_id = format!(
            "REV-{}",
            uuid::Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("0000")
        );
        let lease =
            match self.acquire_or_reuse_panel_lease(task_id, &new_review_id, next_round, &now) {
                Ok(lease) => lease,
                Err(err) => return Ok(error_result(err)),
            };
        let panel = lease.panel();
        let panel_id = lease.panel_id.clone();
        let new_state = ReviewState {
            task_id: task_id.to_string(),
            status: "collecting".to_string(),
            current_round: next_round,
            cycle_start_round: existing_state
                .as_ref()
                .map(|state| {
                    if starts_new_review_epoch {
                        next_round
                    } else {
                        state.cycle_start_round.max(1)
                    }
                })
                .unwrap_or(next_round),
            review_epoch_start_round: existing_state
                .as_ref()
                .map(|state| {
                    if starts_new_review_epoch {
                        next_round
                    } else {
                        state.review_epoch_start_round.max(1)
                    }
                })
                .unwrap_or(next_round),
            current_review_id: new_review_id.clone(),
            max_rounds,
            panel_id,
            panel_mode: self.panel_mode_str().to_string(),
            panel: panel.clone(),
            submissions_received: Vec::new(),
            created_at: now.clone(),
            updated_at: now.clone(),
        };

        // Normalize re-review/recovery gates through review_ready before seating review.
        if matches!(
            normalized_task_status,
            Some("changes_requested" | "blocked")
        ) {
            if let Err(err) = update_task_status_atomic(task_id, "review_ready").await {
                return Ok(error_result(format!(
                    "Failed to move task {task_id} from {task_status} to review_ready before review: {err}"
                )));
            }
        }

        // Set task status to in_review BEFORE persisting review state
        if let Err(err) = update_task_status_atomic(task_id, "in_review").await {
            return Ok(error_result(format!(
                "Failed to update task {task_id} status to in_review: {err}"
            )));
        }
        // Clear supervisor-owned integration conflict metadata BEFORE releasing the worker to
        // review. The conflict-apply path parks the previous worker in
        // `integration_conflict.previous_worker`; clearing first lets that function restore
        // `assignee`/`review_owner` so `release_task_worker_to_review` can observe the worker
        // identity rather than seeing null fields.
        if let Err(err) = clear_task_supervisor_integration_conflict(task_id).await {
            return Ok(error_result(format!(
                "Failed to clear stale supervisor-owned integration conflict metadata for task {task_id}: {err}"
            )));
        }
        if let Err(err) = release_task_worker_to_review(task_id, None).await {
            return Ok(error_result(format!(
                "Failed to release worker ownership for task {task_id} while seating review: {err}"
            )));
        }

        // Persist review state (only after task status update succeeds)
        if let Err(err) = write_review_state(task_id, &new_state) {
            return Ok(error_result(format!(
                "Failed to persist review state for task {task_id}: {err}"
            )));
        }
        if let Err(err) = set_task_review_feedback(task_id, None).await {
            return Ok(error_result(format!(
                "Failed to clear stale review feedback for task {task_id}: {err}"
            )));
        }

        let round = next_round;
        let review_id = new_review_id;

        if let Some((previous_review_id, pending_reviewers, previous_status)) = superseded_notice {
            let superseded_message = format!(
                "Review {previous_review_id} for task {task_id} is no longer active ({previous_status}). Stop reviewing that round. If review is still needed, wait for the new review request and use its review_id. Any late submission for {previous_review_id} will be ignored."
            );
            for reviewer in pending_reviewers {
                notify_agent(&reviewer, &requested_by, &superseded_message);
            }
        }

        // Write round request metadata
        let resolved_empty_commit_set =
            reviewed_commit_set.commit_set_resolved && reviewed_commit_set.commits.is_empty();
        // Captured before the struct move so the reviewer prompt below can
        // reference base_commit without a second resolve.
        let base_commit_for_prompt: Option<String> = (!reviewed_commit_set.base_commit.is_empty())
            .then(|| reviewed_commit_set.base_commit.clone());
        let review_fingerprint = build_review_fingerprint(
            task_id,
            &review_id,
            round,
            &commit,
            &reviewed_commit_set,
            resolved_empty_commit_set,
        );
        let req = ReviewRequestFile {
            task_id: task_id.to_string(),
            review_id: review_id.clone(),
            requested_by: requested_by.clone(),
            requested_at: chrono::Utc::now().to_rfc3339(),
            title: title.to_string(),
            description: description.to_string(),
            commit: commit.clone(),
            base_commit: reviewed_commit_set.base_commit,
            merge_target_head: reviewed_commit_set.merge_target_head,
            commits: reviewed_commit_set.commits,
            resolved_empty_commit_set,
            review_fingerprint: review_fingerprint.clone(),
            context: context.clone(),
        };
        if let Err(err) = write_round_request(task_id, round, &req) {
            return Ok(error_result(format!(
                "Failed to persist review request metadata for task {task_id}: {err}"
            )));
        }

        if let Err(err) = self.emit_review_requested(&task, task_id, &review_id).await {
            return Ok(error_result(format!(
                "Failed to persist review request for task {task_id}: {err}"
            )));
        }

        // Resolve fields the reviewer needs but the task-state lookup paths
        // above did not load. These are stable across the panel loop.
        //
        // worker_branch comes from `task.branch` so a reviewer that needs the
        // working tree state knows where to look. merge_target falls back to
        // the detected default; base_commit was captured before the
        // ReviewRequestFile move above.
        let worker_branch = task
            .get("branch")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let merge_target = read_task_merge_target(task_id)
            .or_else(detect_default_branch)
            .unwrap_or_else(|| "main".to_string());

        // Fetch a compact proof bundle digest so reviewers see recorded
        // evidence (and gaps) alongside the diff guidance. Falls back to
        // `absent` when no bundle exists, which surfaces missing proof
        // explicitly rather than silently hiding it.
        let proof_bundle = self.proof_recorder.proof_bundle_for_task(task_id).await;
        let proof_summary = proof_bundle
            .as_ref()
            .map(crate::tools::proof_summary::ProofSummary::from_bundle)
            .or_else(|| {
                if self.proof_recorder.is_attached() {
                    Some(crate::tools::proof_summary::ProofSummary::absent())
                } else {
                    None
                }
            });
        let project_config =
            workspace_root().and_then(|root| brehon_config::load_config(Some(&root)).ok());
        let mut research_jobs_queued = Vec::new();
        let mut research_warning = None;
        if project_config
            .as_ref()
            .is_some_and(|config| config.research.enabled)
        {
            match crate::tools::research::run_automatic_routes_for_task(
                task_id,
                brehon_types::ResearchTrigger::BeforeReview,
                &requested_by,
            ) {
                Ok(queued) => {
                    research_jobs_queued = queued;
                }
                Err(err) => {
                    research_warning = Some(err);
                }
            }
        }
        let research_context = if project_config
            .as_ref()
            .is_none_or(|config| config.research.attach.on_review_request)
        {
            crate::tools::research::render_task_research_handoff(&task, project_config.as_ref())
        } else {
            String::new()
        };

        // Send review prompt to each panel reviewer -- personalized with their name
        for reviewer in &panel {
            let review_prompt = build_review_request_prompt(&ReviewRequestPromptInput {
                review_id: &review_id,
                task_id,
                title,
                description,
                context: &context,
                panel_id: &new_state.panel_id,
                round,
                reviewer,
                commit: &commit,
                base_commit: base_commit_for_prompt.as_deref(),
                worker_branch,
                merge_target: Some(&merge_target),
                review_fingerprint: Some(&review_fingerprint),
                proof_summary: proof_summary.as_ref(),
                research_context: Some(&research_context),
            });
            notify_agent(reviewer, &requested_by, &review_prompt);
        }

        let mut result = serde_json::json!({
            "status": "ok",
            "review_id": review_id,
            "task_id": task_id,
            "panel_id": new_state.panel_id,
            "panel": panel,
            "round": round,
            "cycle_round": review_cycle_round(&new_state, round),
            "review_epoch_round": current_review_epoch_round(&new_state),
            "review_epoch_start_round": new_state.review_epoch_start_round,
            "new_review_epoch": starts_new_review_epoch,
            "review_fingerprint": review_fingerprint,
            "next_action": next_action_review_status(task_id)
        });
        if !research_jobs_queued.is_empty() {
            result["research_jobs_queued"] = Value::Array(research_jobs_queued);
        }
        if let Some(warning) = research_warning {
            result["research_warning"] = Value::String(warning);
        }
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_submit_review(&self, args: &Value) -> Result<ToolResult, McpError> {
        let review_id = match args.get("review_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(error_result("Missing required parameter: review_id")),
        };

        let verdict_str_arg = match args.get("verdict").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => return Ok(error_result("Missing required parameter: verdict")),
        };
        if !is_supported_review_verdict(&verdict_str_arg) {
            return Ok(error_result(format!(
                "Unsupported verdict `{verdict_str_arg}`. Use approved, needs_revision, changes_requested, or rejected."
            )));
        }

        let score_val = match args.get("score").and_then(|v| v.as_u64()) {
            Some(s) if (1..=10).contains(&s) => s as u8,
            Some(s) => return Ok(error_result(format!("Score must be 1-10, got {s}"))),
            None => return Ok(error_result("Missing required parameter: score (1-10)")),
        };

        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let findings = match parse_review_findings_arg(args.get("findings")) {
            Ok(findings) => findings,
            Err(err) => return Ok(error_result(err)),
        };
        // Reviewer identity: prefer explicit parameter, then env var, then
        // session file lookup. The env var often doesn't propagate through
        // agent CLI -> MCP subprocess boundaries.
        let reviewer = args
            .get("reviewer")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "unknown-reviewer".to_string());

        // Find the task this review_id belongs to. Search active review state first,
        // then fall back to persisted request metadata so stale late submissions can
        // be acknowledged without surfacing a failed tool call.
        let active_task_id = reviews_dir().and_then(|reviews_root| {
            std::fs::read_dir(&reviews_root).ok().and_then(|entries| {
                entries.flatten().find_map(|entry| {
                    if !entry.path().is_dir() {
                        return None;
                    }
                    let task_id = entry.file_name().to_string_lossy().to_string();
                    read_review_state(&task_id)
                        .filter(|state| state.current_review_id == review_id)
                        .map(|_| task_id)
                })
            })
        });

        let Some(task_id) = active_task_id
            .or_else(|| find_review_request_by_id(&review_id).map(|(task_id, _)| task_id))
        else {
            return Ok(self.submit_review_rejection_result(
                &review_id,
                RejectionReason::UnknownReviewId,
                format!("No review found for review_id {review_id}"),
            ));
        };

        let _lock = match acquire_review_lock(&task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        let task_status = read_task_status(&task_id);
        let mut state = match read_review_state(&task_id) {
            Some(s) => s,
            None => {
                let reason = if task_status_closes_review(task_status.as_deref()) {
                    RejectionReason::TaskClosed
                } else {
                    RejectionReason::MissingReviewState
                };
                return Ok(self.ignored_submit_review_result(
                    &review_id,
                    &task_id,
                    reason,
                    format!(
                        "Review {review_id} for task {task_id} is no longer active because its review state is gone. Late submissions are ignored."
                    ),
                    None,
                    task_status.as_deref(),
                ));
            }
        };

        if state.current_review_id != review_id {
            return Ok(self.ignored_submit_review_result(
                &review_id,
                &task_id,
                RejectionReason::RoundSuperseded,
                format!(
                    "Review {review_id} for task {task_id} is no longer active. Current active review: {}. Late submissions are ignored.",
                    state.current_review_id
                ),
                Some(&state),
                task_status.as_deref(),
            ));
        }

        if state.status != "collecting" {
            let reason = if task_status_closes_review(task_status.as_deref()) {
                RejectionReason::TaskClosed
            } else {
                RejectionReason::RoundSuperseded
            };
            return Ok(self.ignored_submit_review_result(
                &review_id,
                &task_id,
                reason,
                format!(
                    "Review {review_id} for task {task_id} is already {}. Late submissions are ignored.",
                    state.status
                ),
                Some(&state),
                task_status.as_deref(),
            ));
        }

        if !matches!(task_status.as_deref(), Some("in_review" | "InReview")) {
            let task_status_display = task_status.as_deref().unwrap_or("unknown");
            let reason = if task_status_closes_review(task_status.as_deref()) {
                RejectionReason::TaskClosed
            } else {
                RejectionReason::RoundSuperseded
            };
            return Ok(self.ignored_submit_review_result(
                &review_id,
                &task_id,
                reason,
                format!(
                    "Task {task_id} is no longer in_review (current status: {task_status_display}). Review {review_id} is obsolete and late submissions are ignored."
                ),
                Some(&state),
                task_status.as_deref(),
            ));
        }

        // Validate: reviewer is on the panel
        if !state.panel.contains(&reviewer) {
            return Ok(error_result(format!(
                "Reviewer {reviewer} is not on the panel for this review. \
                 Panel: {}",
                state.panel.join(", ")
            )));
        }

        // Validate: not already submitted this round
        if state.submissions_received.contains(&reviewer) {
            return Ok(error_result(format!(
                "Reviewer {reviewer} has already submitted for round {}",
                state.current_round
            )));
        }

        if let Some(reason) = unsupported_negative_review_reason(
            &self.config.policy,
            score_val,
            &verdict_str_arg,
            &findings,
        ) {
            return Ok(error_result(format!(
                "Unsupported negative review from {reviewer}: {reason}. Submit an actionable blocking finding for this same review_id, or submit verdict=approved with non-blocking findings as suggestions/nitpicks."
            )));
        }

        // Store the submission
        let submission = StoredSubmission {
            review_id: review_id.clone(),
            reviewer: reviewer.clone(),
            round: state.current_round,
            score: score_val,
            verdict: verdict_str_arg.clone(),
            summary,
            findings,
            submitted_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(err) = write_submission(&task_id, state.current_round, &reviewer, &submission) {
            return Ok(error_result(format!(
                "Failed to persist review submission for task {task_id}: {err}"
            )));
        }

        // Update state
        state.submissions_received.push(reviewer.clone());
        state.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(err) = write_review_state(&task_id, &state) {
            return Ok(error_result(format!(
                "Failed to persist review state for task {task_id}: {err}"
            )));
        }

        // Emit ReviewScoreReceived
        self.emit_event(
            EventKind::ReviewScoreReceived {
                review_id: review_id.clone(),
                reviewer_id: reviewer.clone(),
                score: score_val,
            },
            &review_id,
        )
        .await;

        // Check if panel is complete
        let all_submitted = state
            .panel
            .iter()
            .all(|r| state.submissions_received.contains(r));

        let mut reviewer_reset_queued = false;
        if self.share_after_submit_enabled() && !all_submitted {
            reviewer_reset_queued =
                enqueue_reviewer_reset_with_logging(&task_id, &review_id, &reviewer);
        }

        let submissions = read_round_submissions(&task_id, state.current_round);
        let mut report = self.evaluate_round(&task_id, &review_id, &state, &submissions);
        let completed_early = !all_submitted
            && matches!(
                report.outcome.as_str(),
                "changes_requested" | "rejected" | "escalated"
            );

        if !all_submitted && !completed_early {
            let progress = format!("{}/{}", state.submissions_received.len(), state.panel.len());
            let result = serde_json::json!({
                "status": "ok",
                "review_id": review_id,
                "task_id": task_id,
                "panel_progress": progress,
                "reviewer_reset_queued": reviewer_reset_queued,
                "next_action": next_action_review_status(&task_id),
                "message": if self.share_after_submit_enabled() {
                    format!("Review submitted. Waiting for remaining reviewers ({progress}). Reviewer reuse is gated on a clean session reset.")
                } else {
                    format!("Review submitted. Waiting for remaining reviewers ({progress}).")
                }
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        }

        // -- Panel complete, or an irreversible negative verdict arrived ----

        if report.outcome == "collecting" {
            report.outcome = "escalated".to_string();
            report.threshold_reason = format!(
                "{} Closed review round finished without enough reviewers to satisfy policy. Escalating to supervisor.",
                report.threshold_reason
            );
        }

        // Persist consolidated report
        if let Err(err) = super::state::write_consolidated(&task_id, state.current_round, &report) {
            return Ok(error_result(format!(
                "Failed to persist consolidated report for task {task_id}: {err}"
            )));
        }

        // Best-effort: record consolidated review evidence into the durable
        // proof bundle. Failure surfaces as `proof_status`/`proof_warning`
        // on the response but never blocks consolidation.
        let proof_outcome = self
            .proof_recorder
            .record_consolidation(&task_id, &review_id, &report, &submissions)
            .await;

        // Update task status BEFORE updating review state
        let status_to_set =
            if report.outcome == "escalated" && total_review_rounds_exhausted(&state) {
                "blocked"
            } else {
                task_status_for_review_outcome(&report.outcome).unwrap_or("changes_requested")
            };
        if let Err(err) = update_task_status_atomic(&task_id, status_to_set).await {
            return Ok(error_result(format!(
                "Failed to update task {task_id} status to {status_to_set}: {err}"
            )));
        }
        let task_feedback = Some(build_task_review_feedback(&state, &report));
        if let Err(err) = set_task_review_feedback(&task_id, task_feedback).await {
            return Ok(error_result(format!(
                "Failed to persist review feedback on task {task_id}: {err}"
            )));
        }
        if report.outcome == "approved" {
            let followups = build_task_review_followups(&report);
            if let Err(err) = append_task_review_followups(&task_id, &followups).await {
                return Ok(error_result(format!(
                    "Failed to persist approved review followups on task {task_id}: {err}"
                )));
            }
        }
        if status_to_set == "changes_requested" {
            if let Err(err) = restore_task_worker_from_review_owner(&task_id).await {
                return Ok(error_result(format!(
                    "Failed to restore worker ownership for task {task_id} after review feedback: {err}"
                )));
            }
        }

        let pending_reviewers_notified = if completed_early {
            let cancellation_message = format!(
                "Review {review_id} for task {task_id} closed early as {} after reviewer {reviewer} submitted. Stop reviewing this round; late submissions will be ignored. The task will return to the worker/supervisor flow.",
                report.outcome
            );
            self.notify_pending_panel_reviewers(&state, "review-coordinator", &cancellation_message)
        } else {
            Vec::new()
        };

        // Update review state
        state.status = report.outcome.clone();
        state.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(err) = write_review_state(&task_id, &state) {
            return Ok(error_result(format!(
                "Failed to persist final review state for task {task_id}: {err}"
            )));
        }
        match report.outcome.as_str() {
            "approved" => {
                self.emit_event(
                    EventKind::ReviewApproved {
                        review_id: review_id.clone(),
                    },
                    &review_id,
                )
                .await;
            }
            "changes_requested" => {
                self.emit_event(
                    EventKind::ReviewChangesRequested {
                        review_id: review_id.clone(),
                    },
                    &review_id,
                )
                .await;
            }
            "rejected" => {
                self.emit_event(
                    EventKind::ReviewRejected {
                        review_id: review_id.clone(),
                    },
                    &review_id,
                )
                .await;
            }
            "escalated" => {
                // max rounds exceeded
                self.emit_event(
                    EventKind::EscalationTriggered {
                        reason: "review_escalated".to_string(),
                        context: format!(
                            "Task {task_id} review {} escalated in round {}: {}",
                            review_id, state.current_round, report.threshold_reason
                        ),
                    },
                    &task_id,
                )
                .await;
            }
            _ => {}
        }

        // Build human-readable consolidated report for supervisor
        let notification = self.format_consolidated_report(&task_id, &report);
        let notified = notify_review_stakeholders(
            &task_id,
            state.current_round,
            "review-coordinator",
            &notification,
        );
        let worker_notified = match report.outcome.as_str() {
            "changes_requested" | "rejected" | "escalated" => {
                if let Some(assignee) = read_task_assignee(&task_id) {
                    let worker_message = format_worker_feedback_message(
                        &task_id,
                        &report.review_id,
                        report.round,
                        &state.panel_id,
                        &report.outcome,
                        &report.threshold_reason,
                        &report.blocking,
                        &report.suggestions,
                    );
                    notify_agent(&assignee, "review-coordinator", &worker_message);
                    Some(assignee)
                } else {
                    None
                }
            }
            _ => None,
        };

        if self.share_after_submit_enabled() && all_submitted {
            reviewer_reset_queued =
                enqueue_reviewer_reset_with_logging(&task_id, &review_id, &reviewer);
        }

        let mut result = serde_json::json!({
            "status": "ok",
            "review_id": review_id,
            "task_id": task_id,
            "panel_progress": format!("{}/{}", state.submissions_received.len(), state.panel.len()),
            "completed_early": completed_early,
            "pending_reviewers_notified": pending_reviewers_notified,
            "outcome": &report.outcome,
            "average_score": report.average_score,
            "min_score": report.min_score,
            "threshold_result": report.threshold_result,
            "notified_supervisors": notified,
            "notified_worker": worker_notified,
            "reviewer_reset_queued": reviewer_reset_queued,
            "next_action": next_action_after_review_outcome(&task_id, &report.outcome),
            "message": if completed_early {
                "Review round closed early after an irreversible negative verdict. Consolidated report delivered to supervisor."
            } else {
                "Panel complete. Consolidated report delivered to supervisor."
            }
        });
        proof_outcome.attach_to_result(&mut result);
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_review_status(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        let review_id_arg = args.get("review_id").and_then(|v| v.as_str()).unwrap_or("");

        if task_id.is_empty() && review_id_arg.is_empty() {
            return Ok(error_result(
                "At least one of task_id or review_id is required",
            ));
        }

        // Find task_id from review_id if needed
        let resolved_task_id = if !task_id.is_empty() {
            task_id.to_string()
        } else {
            // Search for the review_id across all tasks
            let Some(reviews_root) = reviews_dir() else {
                return Ok(error_result("Cannot locate reviews directory"));
            };
            let mut found = None;
            if let Ok(entries) = std::fs::read_dir(&reviews_root) {
                for entry in entries.flatten() {
                    if entry.path().is_dir() {
                        let tid = entry.file_name().to_string_lossy().to_string();
                        if let Some(state) = read_review_state(&tid) {
                            if state.current_review_id == review_id_arg {
                                found = Some(tid);
                                break;
                            }
                        }
                    }
                }
            }
            match found {
                Some(tid) => tid,
                None => {
                    return Ok(error_result(format!(
                        "No review found for review_id {review_id_arg}"
                    )))
                }
            }
        };

        let _lock = match acquire_review_lock(&resolved_task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {resolved_task_id}: {err}"
                )))
            }
        };

        match read_review_state(&resolved_task_id) {
            Some(mut state) => {
                // Check for timeout -- may auto-escalate (this is the ONE
                // acceptable side effect: timeouts are time-based, not
                // triggered by who's checking)
                let timed_out = self.check_timeout(&resolved_task_id, &mut state).await;

                // Check for stale review (read-only)
                let stale_warning = self.check_stale(&resolved_task_id, &state);

                let needs_fresh_round = state.status == "escalated"
                    && state.submissions_received.is_empty()
                    && read_round_submissions(&resolved_task_id, state.current_round).is_empty();

                // Check panel health (read-only -- reports dead reviewers
                // but does NOT auto-evaluate)
                let dead_reviewers = if !timed_out && state.status == "collecting" {
                    self.check_panel_health(&state)
                } else {
                    Vec::new()
                };

                let pending: Vec<&String> = state
                    .panel
                    .iter()
                    .filter(|r| !state.submissions_received.contains(r))
                    .collect();
                let task_status = read_task_status(&resolved_task_id);
                let mut result = serde_json::json!({
                    "status": "ok",
                    "task_id": resolved_task_id,
                    "review_id": state.current_review_id,
                    "review_status": state.status,
                    "round": state.current_round,
                    "cycle_round": current_review_cycle_round(&state),
                    "review_epoch_round": current_review_epoch_round(&state),
                    "max_rounds": state.max_rounds,
                    "total_round_limit": total_review_round_limit(state.max_rounds),
                    "review_epoch_start_round": state.review_epoch_start_round.max(1),
                    "panel_id": state.panel_id,
                    "panel_mode": state.panel_mode,
                    "panel": state.panel,
                    "submitted": state.submissions_received,
                    "pending": pending,
                    "progress": format!("{}/{}", state.submissions_received.len(), state.panel.len())
                });
                if let Some(task_status) = task_status.as_deref() {
                    result["task_status"] = serde_json::json!(task_status);
                }
                if let Some(request) = read_round_request(&resolved_task_id, state.current_round) {
                    result["review_commit"] = serde_json::json!(&request.commit);
                    result["reviewed_commits"] =
                        serde_json::json!(super::state::reviewed_commits(&request));
                    result["review_fingerprint"] = request.review_fingerprint.clone();
                }
                result["next_action"] = match state.status.as_str() {
                    "collecting" => next_action_review_status(&resolved_task_id),
                    "approved" | "changes_requested" | "rejected" | "escalated" => {
                        next_action_after_review_outcome(&resolved_task_id, &state.status)
                    }
                    "released" => next_action_request_review(&resolved_task_id),
                    _ => serde_json::json!({ "kind": "none" }),
                };
                if let Some(lease) = find_panel_lease_by_task(&resolved_task_id) {
                    let task_status = task_status.as_deref().unwrap_or("unknown");
                    let lease_state = match normalize_task_status(task_status) {
                        Some("in_review") => "collecting",
                        Some("approved") => "approved_pending_terminal",
                        Some(
                            "changes_requested" | "in_progress" | "pending" | "assigned"
                            | "blocked",
                        ) => "leased_waiting_for_revision",
                        Some("merged" | "closed") => "terminal_release_pending",
                        _ => "leased",
                    };
                    result["panel_lease"] = serde_json::json!({
                        "panel_id": lease.panel_id,
                        "task_id": lease.task_id,
                        "review_id": lease.review_id,
                        "round": lease.round,
                        "members": lease.panel(),
                        "leased_at": lease.leased_at,
                        "updated_at": lease.updated_at,
                        "lease_state": lease_state
                    });
                } else if !state.panel_id.is_empty() {
                    result["panel_lease"] = serde_json::json!({
                        "panel_id": state.panel_id,
                        "lease_state": "missing"
                    });
                    if state.status == "collecting" {
                        result["action_needed"] = serde_json::json!("reseat_panel");
                        result["next_action"] = serde_json::json!({
                            "kind": "reseat_panel",
                            "tool": "verification",
                            "args": {
                                "action": "reseat_panel",
                                "task_id": resolved_task_id
                            }
                        });
                        result["message"] = serde_json::json!(format!(
                            "Review {} for task {resolved_task_id} is still collecting, but panel '{}' has no persisted lease. \
                             Re-seat the active round with: verification action=reseat_panel task_id={resolved_task_id}",
                            state.current_review_id, state.panel_id
                        ));
                    }
                }
                if timed_out {
                    result["timed_out"] = serde_json::json!(true);
                    result["message"] = serde_json::json!(
                        "Review timed out with incomplete quorum. Available submissions were not evaluated as a terminal outcome."
                    );
                }
                if let Some(ref warning) = stale_warning {
                    result["stale_warning"] = serde_json::json!(warning);
                }

                if needs_fresh_round {
                    result["action_needed"] = serde_json::json!("request_review");
                    result["next_action"] = next_action_request_review(&resolved_task_id);
                    result["message"] = serde_json::json!(format!(
                        "The current review round is terminal and has no submissions. \
                         Start a fresh round with: verification action=request_review task_id={resolved_task_id}"
                    ));
                }

                if state.status == "released" {
                    result["action_needed"] = serde_json::json!("request_review");
                    result["next_action"] = next_action_request_review(&resolved_task_id);
                    result["message"] = serde_json::json!(format!(
                        "Panel '{}' was explicitly released for task {resolved_task_id}. \
                         Start a fresh round with: verification action=request_review task_id={resolved_task_id}",
                        state.panel_id
                    ));
                }

                if state.status == "escalated" && total_review_rounds_exhausted(&state) {
                    result["action_needed"] = serde_json::json!("supervisor_intervention_required");
                    result["next_action"] = serde_json::json!({
                        "kind": "supervisor_intervention_required",
                        "tool": "verification",
                        "args": {
                            "action": "review_status",
                            "task_id": resolved_task_id
                        },
                        "requires": ["supervisor"]
                    });
                    result["message"] = serde_json::json!(format!(
                        "Task {resolved_task_id} exhausted the total review round limit ({}/{}). \
                         Do not request another review round for the same reviewed work. Produce a new checkpoint/commit, \
                         or use reset_rounds force_new_epoch=true with an explicit reason for non-commit work.",
                        current_review_epoch_round(&state),
                        total_review_round_limit(state.max_rounds)
                    ));
                } else if state.status == "escalated"
                    && current_review_cycle_round(&state) >= state.max_rounds as u32
                {
                    result["action_needed"] =
                        serde_json::json!("reset_rounds_or_negative_override");
                    result["next_action"] = serde_json::json!({
                        "kind": "reset_rounds_or_negative_override",
                        "tool": "verification",
                        "args": {
                            "action": "reset_rounds",
                            "task_id": resolved_task_id
                        },
                        "alternatives": [{
                            "tool": "verification",
                            "args": {
                                "action": "override",
                                "task_id": resolved_task_id,
                                "verdict": "needs_revision"
                            }
                        }]
                    });
                    result["message"] = serde_json::json!(format!(
                        "Task {resolved_task_id} exhausted review cycle {}/{}. \
                         Supervisor must choose: reset the review cycle with \
                         verification action=reset_rounds task_id={resolved_task_id} reason=\"...\" \
                         or mark changes requested/rejected with verification action=override verdict=needs_revision ...",
                        current_review_cycle_round(&state),
                        state.max_rounds
                    ));
                }

                // Flag dead panel members and suggest reassign_panel
                if !dead_reviewers.is_empty() {
                    let lease = find_panel_lease_by_task(&resolved_task_id);
                    result["replacement_candidates"] =
                        serde_json::json!(self.replacement_candidates(&state));
                    result["replacement_candidates_by_reviewer"] =
                        serde_json::json!(dead_reviewers
                            .iter()
                            .map(|reviewer| {
                                (
                                    reviewer.clone(),
                                    self.replacement_candidates_for_reviewer(
                                        lease.as_ref(),
                                        &state,
                                        reviewer,
                                    ),
                                )
                            })
                            .collect::<HashMap<_, _>>());
                    result["dead_reviewers"] = serde_json::json!(dead_reviewers);
                    result["action_needed"] = serde_json::json!("reassign_panel");
                    result["next_action"] = serde_json::json!({
                        "kind": "reassign_panel",
                        "tool": "verification",
                        "args": {
                            "action": "reassign_panel",
                            "task_id": resolved_task_id
                        }
                    });
                    result["message"] = serde_json::json!(format!(
                        "Panel members no longer live: {}. \
                         Run: verification action=reassign_panel task_id={resolved_task_id}",
                        dead_reviewers.join(", ")
                    ));
                }

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            None => {
                if let Some(task_status) = read_task_status(&resolved_task_id) {
                    let normalized_status = normalize_task_status(&task_status);
                    let mut result = serde_json::json!({
                        "status": "ok",
                        "task_id": resolved_task_id,
                        "review_status": "not_requested",
                        "task_status": task_status,
                    });

                    match normalized_status {
                        Some("review_ready") => {
                            result["action_needed"] = serde_json::json!("request_review");
                            result["next_action"] = next_action_request_review(&resolved_task_id);
                            result["message"] = serde_json::json!(format!(
                                "Task {resolved_task_id} is ready for review, but no review round exists yet. \
                                 Start one with: verification action=request_review task_id={resolved_task_id}"
                            ));
                        }
                        Some("in_review") => {
                            result["review_status"] = serde_json::json!("missing");
                            result["action_needed"] = serde_json::json!("request_review");
                            result["next_action"] = next_action_request_review(&resolved_task_id);
                            result["message"] = serde_json::json!(format!(
                                "Task {resolved_task_id} is marked in_review, but no persisted review state exists. \
                                 Re-seat review with: verification action=request_review task_id={resolved_task_id}"
                            ));
                        }
                        Some("changes_requested") => {
                            result["next_action"] = next_action_after_review_outcome(
                                &resolved_task_id,
                                "changes_requested",
                            );
                            result["message"] = serde_json::json!(format!(
                                "Task {resolved_task_id} has no active review round and is waiting on revision work before the next request_review."
                            ));
                        }
                        _ => {
                            result["next_action"] = serde_json::json!({ "kind": "none" });
                            result["message"] = serde_json::json!(format!(
                                "Task {resolved_task_id} has no active review state."
                            ));
                        }
                    }

                    return Ok(text_result(
                        serde_json::to_string_pretty(&result)
                            .map_err(|e| McpError::Serialization(e.to_string()))?,
                    ));
                }

                Ok(error_result(format!(
                    "No review state found for task {resolved_task_id}"
                )))
            }
        }
    }

    pub(super) async fn handle_override(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };

        let verdict = match args.get("verdict").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => v,
            _ => return Ok(error_result("Missing required parameter: verdict")),
        };

        let reason = match args.get("reason").and_then(|v| v.as_str()) {
            Some(r) if !r.is_empty() => r,
            _ => return Ok(error_result("Missing required parameter: reason")),
        };

        // Verify caller is a supervisor (check args, then env var, then session)
        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
            .unwrap_or_default();
        if role != "supervisor" {
            return Ok(error_result(
                "Only supervisors can override review outcomes. \
                 Pass role=supervisor if your env var is not set.",
            ));
        }

        let overrider = args
            .get("requested_by")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "supervisor".to_string());

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        if matches!(
            read_task_status(task_id)
                .as_deref()
                .and_then(normalize_task_status),
            Some("merged" | "closed")
        ) {
            return Ok(error_result(format!(
                "Task {task_id} is already terminal. Review overrides cannot reopen merged or closed work."
            )));
        }

        if let Some(mut state) = read_review_state(task_id) {
            let (new_status, task_status, event_status) = match verdict {
                "approved" => {
                    return Ok(error_result(
                        "Supervisor approval override is disabled. A task can only become approved \
                         through submitted reviewer verdicts that satisfy the review policy. \
                         If reviewers are unavailable, reset/reseat/reassign the panel or keep the task in review_ready.",
                    ));
                }
                "needs_revision" | "changes_requested" => (
                    "changes_requested",
                    "changes_requested",
                    "changes_requested",
                ),
                "rejected" => ("rejected", "changes_requested", "rejected"),
                _ => {
                    return Ok(error_result(
                        "Override verdict must be 'needs_revision'/'changes_requested' or 'rejected'. Approval override is disabled.",
                    ))
                }
            };

            // Update task status BEFORE updating review state
            if let Err(err) = update_task_status_atomic(task_id, task_status).await {
                return Ok(error_result(format!(
                    "Failed to update task {task_id} status to {task_status}: {err}"
                )));
            }
            let override_feedback = Some(build_override_feedback(
                &state, new_status, reason, &overrider,
            ));
            if let Err(err) = set_task_review_feedback(task_id, override_feedback).await {
                return Ok(error_result(format!(
                    "Failed to persist override feedback on task {task_id}: {err}"
                )));
            }

            state.status = new_status.to_string();
            state.updated_at = chrono::Utc::now().to_rfc3339();
            if let Err(err) = write_review_state(task_id, &state) {
                return Ok(error_result(format!(
                    "Failed to persist override state for task {task_id}: {err}"
                )));
            }

            let cancellation_message = format!(
                "Review {} for task {task_id} is no longer active. Supervisor {} overrode the outcome as {new_status}. Stop reviewing this round. Any late submission will be ignored.",
                state.current_review_id, overrider
            );
            let cancelled_reviewers =
                self.notify_pending_panel_reviewers(&state, &overrider, &cancellation_message);
            let worker_notified = match new_status {
                "changes_requested" | "rejected" => {
                    if let Some(assignee) = read_task_assignee(task_id) {
                        let worker_message = format!(
                            "Supervisor override for task {task_id}\n\
                             Review ID: {}\n\
                             Round: {}\n\
                             Panel: {}\n\
                             Outcome: {}\n\
                             Reason: {reason}\n\
                             The structured review_feedback is attached to your task. Call `task action=mine` before revising.",
                            state.current_review_id,
                            state.current_round,
                            state.panel_id,
                            new_status.to_uppercase(),
                        );
                        notify_agent(&assignee, &overrider, &worker_message);
                        Some(assignee)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            // Emit appropriate event
            let review_id = state.current_review_id.clone();
            match event_status {
                "approved" => {
                    self.emit_event(
                        EventKind::ReviewApproved {
                            review_id: review_id.clone(),
                        },
                        &review_id,
                    )
                    .await;
                }
                "rejected" => {
                    self.emit_event(
                        EventKind::ReviewRejected {
                            review_id: review_id.clone(),
                        },
                        &review_id,
                    )
                    .await;
                }
                "changes_requested" => {
                    self.emit_event(
                        EventKind::ReviewChangesRequested {
                            review_id: review_id.clone(),
                        },
                        &review_id,
                    )
                    .await;
                }
                _ => {}
            }

            let result = serde_json::json!({
                "status": "ok",
                "task_id": task_id,
                "review_id": state.current_review_id,
                "override_verdict": new_status,
                "requested_verdict": verdict,
                "override_reason": reason,
                "override_by": overrider,
                "pending_reviewers_notified": cancelled_reviewers,
                "notified_worker": worker_notified,
            });
            Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ))
        } else {
            Ok(error_result(format!(
                "No review state found for task {task_id}"
            )))
        }
    }

    pub(super) async fn handle_reset_rounds(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };

        let reason = match args.get("reason").and_then(|v| v.as_str()) {
            Some(r) if !r.trim().is_empty() => r.trim(),
            _ => return Ok(error_result("Missing required parameter: reason")),
        };
        let force_new_epoch = args
            .get("force_new_epoch")
            .or_else(|| args.get("force"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
            .unwrap_or_default();
        if role != "supervisor" {
            return Ok(error_result("Only supervisors can reset review rounds."));
        }

        let requested_by = args
            .get("requested_by")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "supervisor".to_string());

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        if matches!(
            read_task_status(task_id)
                .as_deref()
                .and_then(normalize_task_status),
            Some("merged" | "closed")
        ) {
            return Ok(error_result(format!(
                "Task {task_id} is already terminal. Resetting review rounds cannot reopen merged or closed work."
            )));
        }

        let Some(mut state) = read_review_state(task_id) else {
            return Ok(error_result(format!(
                "No review state found for task {task_id}"
            )));
        };

        let prior_cycle_rounds = current_review_cycle_round(&state);
        let consumed_rounds = highest_round_on_disk(task_id).max(state.current_round);
        let attempted_round = consumed_rounds.saturating_add(1);
        let mut consumed_state = state.clone();
        consumed_state.current_round = consumed_rounds;
        if total_review_rounds_exhausted(&consumed_state) && !force_new_epoch {
            return Ok(error_result(review_livelock_guard_message(
                task_id,
                current_review_epoch_round(&consumed_state),
                attempted_round,
                state.max_rounds,
            )));
        }
        if state.status != "escalated" && prior_cycle_rounds < state.max_rounds as u32 {
            return Ok(error_result(format!(
                "Task {task_id} has only consumed {prior_cycle_rounds}/{} review rounds in the current cycle. A reset is not needed yet.",
                state.max_rounds
            )));
        }

        let pending_reviewers = if state.status == "collecting" {
            let cancellation_message = format!(
                "Review {} for task {task_id} is no longer active. Supervisor {} reset the review cycle. \
                 Stop reviewing this round. A fresh review request will arrive with a new review_id if needed.",
                state.current_review_id, requested_by
            );
            self.notify_pending_panel_reviewers(&state, &requested_by, &cancellation_message)
        } else {
            Vec::new()
        };

        let released_panel = match release_panel_lease_for_task(task_id) {
            Ok(panel) => panel,
            Err(err) => return Ok(error_result(err)),
        };

        if matches!(
            read_task_status(task_id)
                .as_deref()
                .and_then(normalize_task_status),
            Some("in_review")
        ) {
            if let Err(err) = update_task_status_atomic(task_id, "changes_requested").await {
                return Ok(error_result(format!(
                    "Failed to move task {task_id} out of in_review before resetting rounds: {err}"
                )));
            }
        }

        let next_round = attempted_round;
        state.status = "released".to_string();
        state.cycle_start_round = next_round;
        if force_new_epoch {
            state.review_epoch_start_round = next_round;
        }
        state.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(err) = write_review_state(task_id, &state) {
            return Ok(error_result(format!(
                "Failed to persist reset review state for task {task_id}: {err}"
            )));
        }

        let result = serde_json::json!({
            "status": "ok",
            "task_id": task_id,
            "review_id": state.current_review_id,
            "prior_round": state.current_round,
            "prior_cycle_rounds": prior_cycle_rounds,
            "max_rounds": state.max_rounds,
            "next_round": next_round,
            "next_cycle_round": 1,
            "force_new_epoch": force_new_epoch,
            "review_epoch_start_round": state.review_epoch_start_round,
            "released_panel": released_panel,
            "pending_reviewers_notified": pending_reviewers,
            "reason": reason,
            "reset_by": requested_by,
            "message": format!(
                "Review rounds reset for task {task_id}. The next request_review will start fresh at round {next_round} / cycle round 1."
            ),
        });
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_reseat_panel(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };
        let requested_panel_id = args
            .get("panel_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
            .unwrap_or_default();
        if role != "supervisor" {
            return Ok(error_result(
                "Only supervisors can reseat an active review panel.",
            ));
        }

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        let Some(state) = read_review_state(task_id) else {
            return Ok(error_result(format!(
                "No review state found for task {task_id}"
            )));
        };

        if state.status != "collecting" {
            return Ok(error_result(format!(
                "Review for task {task_id} is not collecting (current: {}). Only active collecting rounds can be reseated.",
                state.status
            )));
        }

        if let Some(lease) = find_panel_lease_by_task(task_id) {
            let result = serde_json::json!({
                "status": "ok",
                "task_id": task_id,
                "reseated": false,
                "panel_id": lease.panel_id,
                "members": lease.panel(),
                "message": format!(
                    "Task {task_id} already owns panel '{}'. No reseat needed.",
                    lease.panel_id
                )
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        }

        let (panel_id, members) =
            match self.reseated_panel_members(task_id, &state, requested_panel_id) {
                Ok(result) => result,
                Err(err) => return Ok(error_result(err)),
            };

        let now = chrono::Utc::now().to_rfc3339();
        let lease = PanelLeaseState {
            panel_id: panel_id.clone(),
            task_id: task_id.to_string(),
            review_id: state.current_review_id.clone(),
            round: state.current_round,
            members,
            leased_at: now.clone(),
            updated_at: now,
        };
        if let Err(err) = write_panel_lease(&lease) {
            return Ok(error_result(format!(
                "Failed to persist panel lease for task {task_id}: {err}"
            )));
        }

        let requested_by = args
            .get("requested_by")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "supervisor".to_string());
        let reseat_notice = format!(
            "Review {} for task {task_id} remains active on leased panel '{}'. \
             If you still appear in review_obligations, submit against the existing review_id; do not wait for a new round.",
            state.current_review_id, panel_id
        );
        let nudged = self.notify_pending_panel_reviewers(&state, &requested_by, &reseat_notice);

        let result = serde_json::json!({
            "status": "ok",
            "task_id": task_id,
            "review_id": state.current_review_id,
            "round": state.current_round,
            "panel_id": panel_id,
            "members": lease.panel(),
            "reseated": true,
            "nudged_reviewers": nudged,
            "message": format!(
                "Re-seated active review {} for task {task_id} onto panel '{}'.",
                state.current_review_id, panel_id
            )
        });
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_reassign_panel(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        let mut state = match read_review_state(task_id) {
            Some(s) => s,
            None => {
                return Ok(error_result(format!(
                    "No review state found for task {task_id}"
                )));
            }
        };

        if state.status != "collecting" {
            let recovery_hint = if state.status == "escalated"
                && state.submissions_received.is_empty()
            {
                format!(
                    " Start a fresh round with: verification action=request_review task_id={task_id}"
                )
            } else {
                String::new()
            };
            return Ok(error_result(format!(
                "Review for task {task_id} is not in 'collecting' status (current: {}). \
                 Can only reassign panel during active collection.{recovery_hint}",
                state.status,
            )));
        }

        let requested_by = args
            .get("requested_by")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
            .unwrap_or_else(|| "supervisor".to_string());
        let dead = self.find_dead_panel_members(&state);
        if dead.is_empty() {
            let result = serde_json::json!({
                "status": "ok",
                "task_id": task_id,
                "message": "All panel members are live. No reassignment needed.",
                "panel_id": state.panel_id,
                "panel": state.panel
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        }

        let result = match self
            .reassign_dead_panel_members(task_id, &mut state, &requested_by)
            .await
        {
            Ok(Some(result)) => result,
            Ok(None) => {
                let result = serde_json::json!({
                    "status": "ok",
                    "task_id": task_id,
                    "message": "All panel members are live. No reassignment needed.",
                    "panel_id": state.panel_id,
                    "panel": state.panel
                });
                return Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ));
            }
            Err(err) => return Ok(error_result(err)),
        };

        let result = serde_json::json!({
            "status": "ok",
            "task_id": task_id,
            "review_id": result.review_id,
            "panel_id": result.panel_id,
            "panel": result.panel,
            "replacements": result.replacements.iter().map(|replacement| {
                serde_json::json!({
                    "removed": replacement.removed,
                    "replaced_with": replacement.replaced_with
                })
            }).collect::<Vec<_>>(),
            "prompts_sent_to": result.prompts_sent_to,
            "submissions_already_received": result.submissions_already_received
        });
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_release_panel(&self, args: &Value) -> Result<ToolResult, McpError> {
        let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(error_result("Missing required parameter: task_id")),
        };
        let reason = match args.get("reason").and_then(|v| v.as_str()) {
            Some(reason) if !reason.trim().is_empty() => reason.trim(),
            _ => return Ok(error_result("Missing required parameter: reason")),
        };

        let role = args
            .get("role")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
            .unwrap_or_default();
        if role != "supervisor" {
            return Ok(error_result(
                "Only supervisors can release a leased review panel.",
            ));
        }

        let _lock = match acquire_review_lock(task_id).await {
            Ok(lock) => lock,
            Err(err) => {
                return Ok(error_result(format!(
                    "Failed to lock review state for task {task_id}: {err}"
                )))
            }
        };

        let Some(lease) = find_panel_lease_by_task(task_id) else {
            let result = serde_json::json!({
                "status": "ok",
                "task_id": task_id,
                "released": false,
                "message": format!("Task {task_id} does not currently own a panel lease.")
            });
            return Ok(text_result(
                serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            ));
        };

        if let Some(mut state) = read_review_state(task_id) {
            if state.status == "collecting" {
                let requested_by = args
                    .get("requested_by")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
                    .unwrap_or_else(|| "supervisor".to_string());
                let release_notice = format!(
                    "Review {} for task {task_id} is no longer active because panel '{}' was explicitly released by the supervisor. Reason: {}. Late submissions will be ignored until a new review round is requested.",
                    state.current_review_id,
                    lease.panel_id,
                    reason
                );
                self.notify_pending_panel_reviewers(&state, &requested_by, &release_notice);
                state.status = "released".to_string();
                state.updated_at = chrono::Utc::now().to_rfc3339();
                if let Err(err) = write_review_state(task_id, &state) {
                    return Ok(error_result(format!(
                        "Failed to persist released review state for task {task_id}: {err}"
                    )));
                }
            }
        }

        if let Err(err) = delete_panel_lease(task_id) {
            return Ok(error_result(format!(
                "Failed to release panel '{}' for task {task_id}: {err}",
                lease.panel_id
            )));
        }

        let result = serde_json::json!({
            "status": "ok",
            "task_id": task_id,
            "released": true,
            "panel_id": lease.panel_id,
            "members": lease.panel(),
            "reason": reason,
            "message": format!(
                "Released panel '{}' from task {task_id}. Request a fresh review round when the task is ready to reacquire a panel.",
                lease.panel_id
            )
        });
        Ok(text_result(
            serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::Serialization(e.to_string()))?,
        ))
    }

    pub(super) async fn handle_calibration_stats(
        &self,
        args: &Value,
    ) -> Result<ToolResult, McpError> {
        let reviewer_filter = args
            .get("reviewer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let calibration = self.build_calibration();
        let snapshot = self.persist_calibration(&calibration);

        match snapshot {
            Some(snapshot) if !reviewer_filter.is_empty() => {
                // Return stats for a specific reviewer
                let entry = snapshot
                    .reviewers
                    .iter()
                    .find(|e| e.reviewer_id == reviewer_filter);
                match entry {
                    Some(e) => Ok(text_result(
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "ok",
                            "reviewer": e,
                            "global_average": snapshot.global_average,
                        }))
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                    )),
                    None => Ok(error_result(format!(
                        "No calibration data for reviewer {reviewer_filter}"
                    ))),
                }
            }
            Some(snapshot) => Ok(text_result(
                serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::Serialization(e.to_string()))?,
            )),
            None => Ok(error_result("Could not build calibration data")),
        }
    }
}

#[cfg(test)]
mod handoff_context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handoff_context_caps_large_review_feedback_sections() {
        let blocking: Vec<_> = (0..30)
            .map(|idx| {
                json!({
                    "severity": "blocking",
                    "file": format!("src/file_{idx}.rs"),
                    "line": idx + 1,
                    "description": format!("blocking issue {idx}"),
                    "suggestion": "apply the targeted fix"
                })
            })
            .collect();
        let nitpicks: Vec<_> = (0..12)
            .map(|idx| {
                json!({
                    "severity": "nit",
                    "description": format!("nitpick {idx}")
                })
            })
            .collect();
        let task = json!({
            "status": "changes_requested",
            "title": "Review handoff cap test",
            "notes": "n".repeat(HANDOFF_FREEFORM_MAX_CHARS + 100),
            "review_feedback": {
                "review_id": "REV-old",
                "round": 3,
                "outcome": "changes_requested",
                "threshold_reason": "r".repeat(HANDOFF_METADATA_VALUE_MAX_CHARS + 100),
                "blocking": blocking,
                "nitpicks": nitpicks
            }
        });

        let context = build_review_handoff_context(
            &task,
            &"supervisor context ".repeat(HANDOFF_SUPERVISOR_CONTEXT_MAX_CHARS / 10),
        );

        assert!(context.contains("Previous review feedback to verify"));
        assert!(context.contains("blocking issue 0"));
        assert!(context.contains("... 6 more omitted from handoff"));
        assert!(context.contains("... 4 more omitted from handoff"));
        assert!(context.contains("chars omitted"));
        assert!(!context.contains("blocking issue 29"));
    }

    #[test]
    fn handoff_context_global_cap_preserves_prior_blockers() {
        let file_hints: Vec<_> = (0..100)
            .map(|idx| format!("large file hint {idx} {}", "x".repeat(400)))
            .collect();
        let task = json!({
            "status": "changes_requested",
            "file_hints": file_hints,
            "review_feedback": {
                "review_id": "REV-old",
                "blocking": [{
                    "severity": "blocking",
                    "file": "src/critical.rs",
                    "line": 42,
                    "description": "critical prior blocker"
                }]
            }
        });

        let context = build_review_handoff_context(
            &task,
            &"supervisor context ".repeat(HANDOFF_SUPERVISOR_CONTEXT_MAX_CHARS / 10),
        );

        assert!(context.contains("critical prior blocker"));
        assert!(context.contains("omitted from review handoff"));
        assert!(!context.contains("large file hint 99"));
        assert!(context.chars().count() <= HANDOFF_TOTAL_MAX_CHARS + 140);
    }
}
