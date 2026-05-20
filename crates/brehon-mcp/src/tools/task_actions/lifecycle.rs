//! Task lifecycle helpers: role checks, hierarchy validation, status transitions, dependency reconciliation.

use serde_json::Value;

use brehon_types::{
    infer_task_completion_mode, is_terminal_task_status, normalize_task_status,
    parse_task_completion_mode, TaskCompletionMode,
};

use crate::tools::agent::{current_runtime_session_name_from_root, session_is_live};

use super::dependencies::{
    read_dependency_ids, read_string_list_field, task_has_dependency_scoped_blocker_text,
    task_has_manual_blockers, task_has_recoverable_worker_state_blocker_text,
    write_string_list_field,
};
use super::locking::acquire_task_lock;
use super::paths::brehon_root_dir;
use super::persistence::{read_all_tasks, read_task, write_task};

pub(super) fn caller_role(args: &Value) -> String {
    args.get("role")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| std::env::var("BREHON_AGENT_ROLE").ok())
        .unwrap_or_default()
}

pub(super) fn caller_name(args: &Value, default: &str) -> String {
    args.get("agent_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| std::env::var("BREHON_AGENT_NAME").ok())
        .unwrap_or_else(|| default.to_string())
}

pub(super) fn caller_supervisor(args: &Value) -> String {
    crate::tools::agent::resolve_supervisor_name(args.get("supervisor").and_then(|v| v.as_str()))
        .unwrap_or_default()
}

pub(super) fn is_initiative(task_type: &str) -> bool {
    task_type == "initiative"
}

pub(super) fn is_epic(task_type: &str) -> bool {
    task_type == "epic"
}

pub(super) fn is_container_task(task_type: &str) -> bool {
    matches!(task_type, "initiative" | "epic")
}

pub(super) fn allows_parent(task_type: &str) -> bool {
    !is_initiative(task_type)
}

pub(super) fn parent_can_contain(parent_type: &str, child_type: &str) -> bool {
    matches!(
        (parent_type, child_type),
        ("initiative", "epic") | ("epic", "task")
    )
}

pub(super) fn child_collection_label(task_type: &str) -> &'static str {
    match task_type {
        "initiative" => "epics",
        "epic" => "subtasks",
        _ => "children",
    }
}

pub(super) fn direct_children<'a>(
    all: &'a [serde_json::Map<String, Value>],
    parent_id: &str,
) -> Vec<&'a serde_json::Map<String, Value>> {
    all.iter()
        .filter(|t| t.get("parent_id").and_then(|v| v.as_str()) == Some(parent_id))
        .collect()
}

pub(super) fn ancestor_chain_has_closed_parent(
    all: &[serde_json::Map<String, Value>],
    task: &serde_json::Map<String, Value>,
) -> bool {
    let mut current_parent = task
        .get("parent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    while let Some(parent_id) = current_parent {
        let Some(parent) = all.iter().find(|candidate| {
            candidate.get("task_id").and_then(|v| v.as_str()) == Some(parent_id.as_str())
        }) else {
            break;
        };

        let parent_status = parent.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if is_terminal_task_status(parent_status) {
            return true;
        }

        current_parent = parent
            .get("parent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }

    false
}

pub(super) fn is_valid_subtask(task: &serde_json::Map<String, Value>) -> bool {
    task.get("parent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|parent_id| {
            read_task(parent_id)
                .and_then(|p| {
                    p.get("task_type")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .map(|tt| tt == "epic")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

pub(super) fn task_completion_mode_from_fields(
    task_type: &str,
    title: &str,
    description: &str,
    explicit_mode: Option<&str>,
) -> TaskCompletionMode {
    if is_container_task(task_type) {
        return TaskCompletionMode::Close;
    }

    explicit_mode
        .and_then(parse_task_completion_mode)
        .unwrap_or_else(|| infer_task_completion_mode(title, description))
}

pub(super) fn task_completion_mode_from_task(
    task: &serde_json::Map<String, Value>,
) -> TaskCompletionMode {
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let title = task.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let description = task
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let explicit_mode = task.get("completion_mode").and_then(|v| v.as_str());

    task_completion_mode_from_fields(task_type, title, description, explicit_mode)
}

pub(super) fn direct_to_main_requested(args: &Value) -> bool {
    args.get("direct_to_main")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub(super) fn close_terminal_status(
    current_status: &str,
    task_type: &str,
    completion_mode: TaskCompletionMode,
) -> &'static str {
    if normalize_task_status(current_status) == Some("approved") && !is_container_task(task_type) {
        match completion_mode {
            TaskCompletionMode::Merge => "merged",
            TaskCompletionMode::Close => "closed",
        }
    } else {
        "closed"
    }
}

pub(super) fn task_has_merge_flow_state(task: &serde_json::Map<String, Value>) -> bool {
    task.get("merge_target")
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.trim().is_empty())
        || task
            .get("latest_commit")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty())
        || task
            .get("merged_commit")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.trim().is_empty())
        || task
            .get("integration_status")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v != "not_applicable" && !v.trim().is_empty())
}

pub(super) fn validate_status_transition(
    current: &str,
    proposed: &str,
    caller_role: &str,
    task_type: &str,
    completion_mode: TaskCompletionMode,
) -> Result<(), String> {
    let current_raw = current;
    let proposed_raw = proposed;
    let current = normalize_task_status(current)
        .ok_or_else(|| format!("Unknown current task status: '{current_raw}'"))?;
    let proposed = normalize_task_status(proposed)
        .ok_or_else(|| format!("Unknown proposed task status: '{proposed_raw}'"))?;

    // Same-status is always a no-op
    if current == proposed {
        return Ok(());
    }

    // Layer 1: Role-based guards — certain statuses restricted by role
    match proposed {
        "approved" if caller_role == "worker" => {
            return Err("Workers cannot set status to 'approved'. \
                 Tasks must go through the review pipeline."
                .to_string());
        }
        "changes_requested" if current == "approved" && caller_role != "supervisor" => {
            return Err(
                "Only supervisors can reopen an approved task for another review round."
                    .to_string(),
            );
        }
        "merged" if caller_role != "supervisor" => {
            return Err("Only supervisors can set status to 'merged'. \
                 Approved tasks must be merged by the supervisor."
                .to_string());
        }
        "closed" if caller_role != "supervisor" => {
            return Err("Only supervisors can set status to 'closed'. \
                 Approved close-mode tasks, epics, and initiatives must be closed by the supervisor."
                .to_string());
        }
        _ => {}
    }

    // Epics have a simpler lifecycle, but still respect the role guards above.
    if is_container_task(task_type) {
        return Ok(());
    }

    // Layer 2: State machine — valid transitions for ALL roles
    let valid: &[&str] = match current {
        "pending" => &["assigned", "blocked"],
        "assigned" => &["in_progress", "pending"],
        "in_progress" => &["review_ready", "in_review", "blocked", "pending"],
        "review_ready" => &["in_review", "changes_requested", "pending"],
        "in_review" => &["changes_requested", "approved", "pending"],
        "changes_requested" => &["in_progress", "review_ready"],
        "approved" => match completion_mode {
            TaskCompletionMode::Merge => &["merged", "changes_requested"],
            TaskCompletionMode::Close => &["closed", "changes_requested"],
        },
        "merged" => &[],
        "blocked" => &["pending"],
        "closed" => &[],
        _ => unreachable!("normalized statuses should have matched earlier"),
    };

    if valid.contains(&proposed) {
        Ok(())
    } else {
        let valid_desc = if valid.is_empty() {
            "none (terminal state)".to_string()
        } else {
            valid.join(", ")
        };
        Err(format!(
            "Invalid status transition: '{current}' → '{proposed}'. \
             Valid transitions from '{current}': {valid_desc}"
        ))
    }
}

pub(super) fn clear_terminal_task_ownership(task: &mut serde_json::Map<String, Value>) {
    task.insert("assignee".into(), Value::Null);
    task.insert("review_owner".into(), Value::Null);
    task.remove("activity");
}

pub(super) fn task_has_started_worker_execution(task: &serde_json::Map<String, Value>) -> bool {
    task.get("assignee")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .is_some()
        && (task
            .get("percent")
            .and_then(|value| value.as_u64())
            .is_some_and(|percent| percent > 0)
            || task
                .get("latest_commit")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty()))
}

fn read_live_worker_names() -> Option<std::collections::HashSet<String>> {
    let brehon_root = brehon_root_dir()?;
    let expected_session = current_runtime_session_name_from_root(&brehon_root);
    let sessions_dir = brehon_root.join("runtime").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return None;
    };

    let mut saw_session_file = false;
    let mut live_workers = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        saw_session_file = true;
        if !session_is_live(&value) {
            continue;
        }
        if let Some(expected_session) = expected_session.as_deref() {
            let session_name = value
                .get("session_name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty());
            if session_name != Some(expected_session) {
                continue;
            }
        }
        if value.get("role").and_then(|v| v.as_str()) != Some("worker") {
            continue;
        }
        if let Some(name) = value
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            live_workers.insert(name.to_string());
        }
    }

    saw_session_file.then_some(live_workers)
}

fn task_has_collecting_review_state(task_id: &str) -> bool {
    let Some(brehon_root) = brehon_root_dir() else {
        return false;
    };
    let path = brehon_root
        .join("runtime")
        .join("reviews")
        .join(task_id)
        .join("state.json");
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    value.get("status").and_then(|value| value.as_str()) == Some("collecting")
}

fn task_has_unconsolidated_review_round(task_id: &str) -> bool {
    let Some(brehon_root) = brehon_root_dir() else {
        return false;
    };
    let review_dir = brehon_root.join("runtime").join("reviews").join(task_id);
    let Ok(entries) = std::fs::read_dir(review_dir) else {
        return false;
    };

    let latest_round = entries.flatten().filter_map(|entry| {
        let path = entry.path();
        if !path.is_dir() {
            return None;
        }
        let name = path.file_name().and_then(|name| name.to_str())?;
        let round = name
            .strip_prefix("round-")
            .and_then(|suffix| suffix.parse::<u32>().ok())?;
        Some((round, path))
    });
    let Some((_, path)) = latest_round.max_by_key(|(round, _)| *round) else {
        return false;
    };
    path.join("request.json").exists() && !path.join("consolidated.json").exists()
}

pub(crate) fn task_has_active_or_unconsolidated_review(task_id: &str) -> bool {
    task_has_collecting_review_state(task_id) || task_has_unconsolidated_review_round(task_id)
}

fn assignee_is_live_worker(
    assignee: &str,
    live_workers: Option<&std::collections::HashSet<String>>,
) -> bool {
    live_workers
        .map(|live_workers| live_workers.contains(assignee))
        .unwrap_or(true)
}

fn recover_orphaned_active_task(
    task_id: &str,
    task: &mut serde_json::Map<String, Value>,
    normalized_status: Option<&str>,
    live_workers: Option<&std::collections::HashSet<String>>,
) -> bool {
    if !matches!(
        normalized_status,
        Some("assigned" | "in_progress" | "changes_requested")
    ) {
        return false;
    }

    let Some(assignee) = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return false;
    };

    if assignee_is_live_worker(&assignee, live_workers) {
        return false;
    }
    if task_has_active_or_unconsolidated_review(task_id) {
        return false;
    }

    let orphaned_status = normalized_status.unwrap_or("in_progress").to_string();
    task.insert("orphaned_assignee".into(), Value::String(assignee.clone()));
    task.insert(
        "orphaned_status".into(),
        Value::String(orphaned_status.clone()),
    );
    task.insert("assignee".into(), Value::Null);
    task.remove("activity");
    if normalized_status == Some("changes_requested") {
        task.insert(
            "status".into(),
            Value::String("changes_requested".to_string()),
        );
        task.insert(
            "recovery_note".into(),
            Value::String(format!(
                "Dependency reconciliation cleared orphaned revision ownership: previous assignee {assignee} was no longer live. The task remains changes_requested for reassignment."
            )),
        );
    } else {
        task.insert("review_owner".into(), Value::Null);
        task.insert("status".into(), Value::String("pending".to_string()));
        task.insert(
            "recovery_note".into(),
            Value::String(format!(
                "Dependency reconciliation recovered orphaned task ownership: previous assignee {assignee} was no longer live. Returned to pending for reassignment."
            )),
        );
    }
    true
}

pub(crate) fn unmet_dependency_ids_for_task(
    task: &serde_json::Map<String, Value>,
    all_tasks: &[serde_json::Map<String, Value>],
) -> Vec<String> {
    let mut unmet = Vec::new();
    for dependency_id in read_dependency_ids(task) {
        let Some(dependency_task) = all_tasks.iter().find(|candidate| {
            candidate.get("task_id").and_then(|value| value.as_str())
                == Some(dependency_id.as_str())
        }) else {
            unmet.push(dependency_id);
            continue;
        };

        let dependency_status = dependency_task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !is_terminal_task_status(dependency_status) {
            unmet.push(dependency_id);
        }
    }
    unmet
}

pub(crate) async fn reconcile_dependency_states() -> Result<Vec<String>, String> {
    reconcile_dependency_states_inner(None).await
}

pub(crate) async fn reconcile_dependency_states_with_task_lock(
    locked_task_id: &str,
) -> Result<Vec<String>, String> {
    reconcile_dependency_states_inner(Some(locked_task_id)).await
}

async fn reconcile_dependency_states_inner(
    locked_task_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let tasks = read_all_tasks();
    let snapshot = tasks.clone();
    let live_workers = read_live_worker_names();
    let mut modified = Vec::new();

    for mut task in tasks {
        let task_id = task
            .get("task_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if task_id.is_empty() {
            continue;
        }

        let current_status = task
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("pending")
            .to_string();
        if is_terminal_task_status(&current_status) {
            continue;
        }

        let previous_blocked_by = read_string_list_field(&task, "blocked_by");
        let unmet = unmet_dependency_ids_for_task(&task, &snapshot);
        let mut changed = false;

        if unmet != previous_blocked_by {
            write_string_list_field(&mut task, "blocked_by", &unmet);
            changed = true;
        }

        let started_worker_execution = task_has_started_worker_execution(&task);
        let normalized_status = normalize_task_status(&current_status);
        if unmet.is_empty() && task_has_dependency_scoped_blocker_text(&task) {
            task.remove("blockers");
            changed = true;
        }
        if unmet.is_empty()
            && recover_orphaned_active_task(
                &task_id,
                &mut task,
                normalized_status,
                live_workers.as_ref(),
            )
        {
            modified.push(task_id.clone());
            write_task(&task_id, &task);
            continue;
        }
        let next_status = match normalized_status {
            Some("pending") if !unmet.is_empty() => Some("blocked"),
            Some("pending") if unmet.is_empty() && started_worker_execution => Some("in_progress"),
            Some("blocked")
                if unmet.is_empty()
                    && !task_has_manual_blockers(&task)
                    && !task_has_recoverable_worker_state_blocker_text(&task) =>
            {
                if started_worker_execution {
                    Some("in_progress")
                } else {
                    Some("pending")
                }
            }
            _ => None,
        };

        if let Some(next_status) = next_status {
            let mut next_status = next_status.to_string();
            if next_status == "in_progress" {
                if let Some(assignee) = task
                    .get("assignee")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                {
                    if !assignee_is_live_worker(&assignee, live_workers.as_ref())
                        && !task_has_active_or_unconsolidated_review(&task_id)
                    {
                        task.insert("orphaned_assignee".into(), Value::String(assignee.clone()));
                        task.insert(
                            "orphaned_status".into(),
                            Value::String("in_progress".to_string()),
                        );
                        task.insert("assignee".into(), Value::Null);
                        task.insert("review_owner".into(), Value::Null);
                        task.remove("activity");
                        task.insert(
                            "recovery_note".into(),
                            Value::String(format!(
                                "Dependency reconciliation recovered orphaned task ownership: previous assignee {assignee} was no longer live. Returned to pending for reassignment."
                            )),
                        );
                        next_status = "pending".to_string();
                        changed = true;
                    }
                }
            }
            if current_status != next_status {
                task.insert("status".into(), Value::String(next_status));
                changed = true;
            }
        }

        let normalized_after = next_status.or(normalized_status);
        if matches!(normalized_after, Some("pending" | "blocked"))
            && !started_worker_execution
            && !task_has_active_or_unconsolidated_review(&task_id)
        {
            let had_assignee = task
                .get("assignee")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            let had_review_owner = task
                .get("review_owner")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            let had_activity = task
                .get("activity")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            if had_assignee || had_review_owner || had_activity {
                task.insert("assignee".into(), Value::Null);
                task.insert("review_owner".into(), Value::Null);
                task.remove("activity");
                changed = true;
            }
        }

        if changed {
            task.insert(
                "updated_at".into(),
                Value::String(chrono::Utc::now().to_rfc3339()),
            );
            let _lock = if locked_task_id == Some(task_id.as_str()) {
                None
            } else {
                Some(acquire_task_lock(&task_id).await?)
            };
            if !write_task(&task_id, &task) {
                return Err(format!(
                    "Failed to persist dependency reconciliation for task {task_id}"
                ));
            }
            modified.push(task_id);
        }
    }

    Ok(modified)
}
