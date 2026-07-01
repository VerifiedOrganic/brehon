//! Argument parsing and dependency field helpers.

use serde_json::Value;

use crate::tools::verification::{read_round_request, reviewed_commits};

pub(super) fn normalize_list_item(item: &str) -> Option<String> {
    let trimmed = item
        .trim()
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn parse_string_list_arg(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };

    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| format!("Invalid {key}. Expected an array of strings."))
                    .and_then(|s| {
                        normalize_list_item(s).ok_or_else(|| {
                            format!("Invalid {key}. Items must be non-empty strings.")
                        })
                    })
            })
            .collect(),
        Value::String(text) => Ok(text.lines().filter_map(normalize_list_item).collect()),
        _ => Err(format!(
            "Invalid {key}. Expected a string or array of strings."
        )),
    }
}

pub(super) fn parse_task_id_list_arg(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let items = parse_string_list_arg(args, key)?;
    if items.iter().any(|item| item.trim().is_empty()) {
        return Err(format!(
            "Invalid {key}. Task IDs must be non-empty strings."
        ));
    }
    Ok(items)
}

pub(super) fn read_string_list_field(
    task: &serde_json::Map<String, Value>,
    key: &str,
) -> Vec<String> {
    task.get(key)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::trim))
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect()
}

pub(super) fn read_dependency_ids(task: &serde_json::Map<String, Value>) -> Vec<String> {
    read_string_list_field(task, "dependencies")
}

pub(super) fn write_string_list_field(
    task: &mut serde_json::Map<String, Value>,
    key: &str,
    values: &[String],
) {
    if values.is_empty() {
        task.remove(key);
    } else {
        task.insert(
            key.to_string(),
            Value::Array(values.iter().cloned().map(Value::String).collect()),
        );
    }
}

pub(super) fn task_has_manual_blockers(task: &serde_json::Map<String, Value>) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    !task_has_dependency_scoped_blocker_text(task)
        && !task_has_recoverable_worker_state_blocker_text(task)
}

pub(super) fn task_has_dependency_scoped_blocker_text(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    let dependencies = read_dependency_ids(task);
    if dependencies.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    dependencies
        .iter()
        .any(|dependency_id| blockers.contains(dependency_id))
        || blockers_lower.contains("dependency dag")
        || blockers_lower.contains("dependency")
        || blockers_lower.contains("dependencies complete")
        || blockers_lower.contains("once unblocked")
        || blockers_lower.contains("still inprogress")
        || blockers_lower.contains("still in progress")
        || blockers_lower.contains("until those dependencies complete")
}

pub(super) fn task_has_recoverable_worker_state_blocker_text(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let Some(blockers) = task.get("blockers").and_then(|value| value.as_str()) else {
        return false;
    };
    let blockers = blockers.trim();
    if blockers.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    blockers_lower.contains("state deadlock")
        || blockers_lower.contains("assignment mismatch")
        || blockers_lower.contains("assignee mismatch")
        || blockers_lower.contains("complete call reports task assigned")
        || blockers_lower.contains("reports task assigned to")
        || blockers_lower.contains("assigned to '' not")
        || blockers_lower.contains("assigned to '' instead")
        || blockers_lower.contains("assigned to '' rather than")
        || blockers_lower.contains("empty string instead")
        || blockers_lower.contains("pending/unassigned while work was in progress")
        || blockers_lower.contains("cannot checkpoint/complete")
        || blockers_lower.contains("complete is rejected because")
        || blockers_lower.contains("checkpoint created during pending state")
        || blockers_lower.contains("could not move it to review_ready")
        || blockers_lower.contains("requires supervisor reassignment")
        || blockers_lower.contains("supervisor-side reassignment")
        || blockers_lower.contains("need reassignment to complete")
        || blockers_lower.contains("not permitted to complete")
        || blockers_lower.contains("ownership drift")
        || blockers_lower.contains("ownership fix")
        || blockers_lower.contains("preventing worker completion handoff")
        || blockers_lower.contains("rejects completion from non-assignee")
        || blockers_lower.contains("worker progress updates are rejected")
        || (blockers_lower.contains("invalid status transition")
            && blockers_lower.contains("'pending'")
            && blockers_lower.contains("'in_progress'"))
        || (blockers_lower.contains("invalid status transition")
            && blockers_lower.contains("'blocked'")
            && blockers_lower.contains("'in_progress'"))
        || (blockers_lower.contains("invalid transition")
            && blockers_lower.contains("pending")
            && blockers_lower.contains("in_progress"))
        || (blockers_lower.contains("invalid transition")
            && blockers_lower.contains("blocked")
            && blockers_lower.contains("in_progress"))
}

fn task_id(task: &serde_json::Map<String, Value>) -> Option<&str> {
    task.get("task_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn latest_commit(task: &serde_json::Map<String, Value>) -> Option<&str> {
    task.get("latest_commit")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn review_feedback_round(task: &serde_json::Map<String, Value>) -> Option<u32> {
    task.get("review_feedback")
        .and_then(|value| value.get("round"))
        .and_then(|value| value.as_u64())
        .and_then(|round| u32::try_from(round).ok())
        .filter(|round| *round > 0)
}

fn commit_text_matches(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    left == right
        || (left.len() >= 12 && right.starts_with(left))
        || (right.len() >= 12 && left.starts_with(right))
}

pub(super) fn task_has_recoverable_review_checkpoint_blocker_text(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let blockers = task
        .get("blockers")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let notes = task
        .get("notes")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let text = format!("{blockers}\n{notes}").to_ascii_lowercase();
    let has_checkpoint_marker = text.contains("checkpoint")
        || text.contains("latest_commit")
        || text.contains("latest commit")
        || text.contains("recorded commit");
    let has_checkpointed_fix_marker = text.contains("checkpoint fix")
        || text.contains("checkpointed fix")
        || text.contains("fix is checkpointed");
    let has_environment_blocked_validation_marker = (text.contains("validation is blocked")
        || text.contains("validation blocked")
        || text.contains("phase gate cannot pass")
        || text.contains("gate cannot pass"))
        && (text.contains("sandbox")
            || text.contains("environment")
            || text.contains("tool availability")
            || text.contains("restricted"));
    let has_completion_marker = text.contains("addressed")
        || text.contains("resolved")
        || text.contains("fixed")
        || text.contains("implemented")
        || text.contains("completed")
        || text.contains("complete")
        || text.contains("work is done")
        || text.contains("ready for review")
        || text.contains("review_ready")
        || text.contains("review-ready")
        || text.contains("re-request review")
        || text.contains("rerequest review")
        || has_checkpointed_fix_marker
        || has_environment_blocked_validation_marker;

    has_checkpoint_marker && has_completion_marker
}

pub(super) fn task_has_recoverable_environment_limited_checkpoint(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let blockers = task
        .get("blockers")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let notes = task
        .get("notes")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let text = format!("{blockers}\n{notes}").to_ascii_lowercase();
    let has_checkpoint_marker = text.contains("checkpoint")
        || text.contains("latest_commit")
        || text.contains("latest commit")
        || text.contains("recorded commit");
    let has_completed_work_marker = text.contains("completed checkpoint")
        || text.contains("checkpoint includes")
        || text.contains("passing evidence")
        || text.contains("found and fixed")
        || text.contains("fixed a")
        || text.contains("updated")
        || text.contains("work is done");
    let has_validation_block_marker = text.contains("validation cannot be completed")
        || text.contains("final validation cannot be completed")
        || text.contains("remaining final validation")
        || text.contains("remaining validation")
        || text.contains("phase validation is blocked")
        || text.contains("validation is blocked")
        || text.contains("environment blockers")
        || text.contains("local environment blockers");
    let has_environment_marker = text.contains("sandbox")
        || text.contains("environment")
        || text.contains("tooling")
        || text.contains("toolchain")
        || text.contains("network/dns")
        || text.contains("operation not permitted")
        || text.contains("af_unix")
        || text.contains("go ")
        || text.contains("advisory database")
        || text.contains("advisory db")
        || text.contains("timed out");

    has_checkpoint_marker
        && has_completed_work_marker
        && has_validation_block_marker
        && has_environment_marker
}

pub(super) fn task_has_operator_directed_checkpoint_recovery(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let blockers = task
        .get("blockers")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let notes = task
        .get("notes")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let text = format!("{blockers}\n{notes}").to_ascii_lowercase();
    let has_checkpoint_marker = text.contains("checkpoint")
        || text.contains("latest_commit")
        || text.contains("latest commit")
        || text.contains("stale checkpoint");
    let asks_for_review_recovery = text.contains("recover_handoff")
        || text.contains("checkpoint-review discharge")
        || (text.contains("review_ready")
            && (text.contains("adjudicat") || text.contains("archive")));

    has_checkpoint_marker && asks_for_review_recovery
}

pub(super) fn task_has_resolved_external_unblock_marker(
    task: &serde_json::Map<String, Value>,
    reason: Option<&str>,
) -> bool {
    let blockers = task
        .get("blockers")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let notes = task
        .get("notes")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let reason = reason.unwrap_or("");
    let text = format!("{blockers}\n{notes}\n{reason}").to_ascii_lowercase();
    let resolved = text.contains("resolved")
        || text.contains("available")
        || text.contains("re-pinned")
        || text.contains("repinned")
        || text.contains("updated")
        || text.contains("satisfied")
        || text.contains("cleared");
    let external = text.contains("external")
        || text.contains("environment")
        || text.contains("sdk")
        || text.contains("pin")
        || text.contains("client")
        || text.contains("missing api")
        || text.contains("tool availability");
    let worker_frontier = text.contains("worker frontier")
        || text.contains("rework")
        || text.contains("reassignment")
        || text.contains("reassign")
        || text.contains("normal implementation")
        || text.contains("return to pending");

    (resolved && external && worker_frontier)
        || text.contains("external blocker resolved")
        || text.contains("external prerequisite is now satisfied")
}

pub(super) fn task_has_recoverable_blocked_review_checkpoint(
    task: &serde_json::Map<String, Value>,
) -> bool {
    if task_review_feedback_outcome(task).as_deref() != Some("changes_requested") {
        return false;
    }
    if !task_has_recoverable_review_checkpoint_blocker_text(task) {
        return false;
    }

    let Some(task_id) = task_id(task) else {
        return false;
    };
    let Some(latest_commit) = latest_commit(task) else {
        return false;
    };
    let Some(round) = review_feedback_round(task) else {
        return false;
    };
    let Some(request) = read_round_request(task_id, round) else {
        return false;
    };

    !reviewed_commits(&request)
        .iter()
        .any(|commit| commit_text_matches(commit, latest_commit))
        && !commit_text_matches(&request.commit, latest_commit)
}

pub(super) fn task_has_integrated_record(task: &serde_json::Map<String, Value>) -> bool {
    task.get("integration_status")
        .and_then(|value| value.as_str())
        .is_some_and(|value| value.trim() == "integrated")
        || task
            .get("merged_commit")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.trim().is_empty())
}

pub(super) fn task_review_feedback_outcome(
    task: &serde_json::Map<String, Value>,
) -> Option<String> {
    task.get("review_feedback")
        .and_then(|value| value.get("outcome"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

pub(super) fn task_has_final_review_feedback(task: &serde_json::Map<String, Value>) -> bool {
    task_review_feedback_outcome(task)
        .as_deref()
        .is_some_and(|outcome| matches!(outcome, "approved" | "rejected"))
}

pub(super) fn task_has_legacy_completed_worker_status(
    task: &serde_json::Map<String, Value>,
) -> bool {
    let status = task
        .get("status")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");

    task_type == "task" && matches!(status, "complete" | "Complete" | "completed" | "Completed")
}

pub(super) fn parse_optional_text_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Some(_) => Err(format!("Invalid {key}. Expected a string.")),
    }
}
