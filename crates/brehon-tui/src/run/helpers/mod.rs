//! JSON reading helpers and small utility predicates.

mod pane;

pub(crate) use pane::pane_needs_post_spawn_prompt;

pub(crate) use brehon_types::sanitize_runtime_key;
use brehon_types::task::normalize_task_status;
use serde::{Deserialize, Serialize};

use super::types::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueuedReviewerReset {
    pub(crate) task_id: String,
    pub(crate) review_id: String,
    pub(crate) reviewer: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

pub(crate) type ReviewerResetEntry = QueuedReviewerReset;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueuedWorkerRecycle {
    pub(crate) task_id: String,
    pub(crate) worker: String,
}

pub(crate) type WorkerRecycleEntry = QueuedWorkerRecycle;

fn prompt_file_matches_id(path: &std::path::Path, prompt_id: &str) -> bool {
    if path.is_dir() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    if value.get("prompt_id").and_then(|v| v.as_str()) == Some(prompt_id) {
        return true;
    }
    value
        .get("entry")
        .and_then(|entry| entry.get("prompt_id"))
        .and_then(|v| v.as_str())
        == Some(prompt_id)
}

/// Find an ack file in `dir` whose JSON payload contains the given `prompt_id`.
/// This avoids the collision risk of filename-based sanitization by matching on
/// file content, consistent with how queue and dead-letter directories work.
///
/// Performance: O(n) in directory entries. Ack directories are expected to
/// remain small (single-digit file counts per session), so this scan is cheap
/// in practice and eliminates the collision risk of the old O(1) filename-
/// based lookup.
///
/// NOTE: Keep in sync with `find_prompt_ack_in_dir` in
/// `crates/brehon-mcp/src/tools/agent.rs`.
fn prompt_ack_file_for_id(dir: &std::path::Path, prompt_id: &str) -> Option<std::path::PathBuf> {
    brehon_types::find_prompt_ack_in_dir(dir, prompt_id).map(|(path, _value)| path)
}

/// Runtime prompt directories are session-scoped and only contain the queue
/// layout Brehon writes today (root/session/file). Keep the search recursive so
/// orphaned legacy entries are still found, but guard depth to avoid unbounded
/// traversal if the directory layout ever changes.
fn prompt_id_exists_in_dir_tree(dir: &std::path::Path, prompt_id: &str, max_depth: usize) -> bool {
    if max_depth == 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            prompt_id_exists_in_dir_tree(&path, prompt_id, max_depth.saturating_sub(1))
        } else {
            prompt_file_matches_id(&path, prompt_id)
        }
    })
}

fn resolve_prompt_delivery_state(brehon_root: &std::path::Path, prompt_id: &str) -> Option<String> {
    let prompt_id = prompt_id.trim();
    if prompt_id.is_empty() {
        return None;
    }

    let enqueued = prompt_ack_file_for_id(
        &brehon_root.join("runtime").join("prompt-enqueue-acks"),
        prompt_id,
    )
    .is_some();
    let injected = prompt_ack_file_for_id(
        &brehon_root.join("runtime").join("prompt-delivery-acks"),
        prompt_id,
    )
    .is_some();
    let queued = prompt_id_exists_in_dir_tree(
        &brehon_root.join("runtime").join("prompt-queue"),
        prompt_id,
        3,
    );
    let dead_lettered = prompt_id_exists_in_dir_tree(
        &brehon_root.join("runtime").join("prompt-dead-letter"),
        prompt_id,
        3,
    );

    Some(
        if dead_lettered {
            "dead_lettered"
        } else if injected {
            "injected"
        } else if queued {
            "queued"
        } else if enqueued {
            "drained_without_ack"
        } else {
            "unknown"
        }
        .to_string(),
    )
}

#[allow(dead_code)]
pub(crate) fn read_queued_reviewer_reset(path: &std::path::Path) -> Option<QueuedReviewerReset> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some(QueuedReviewerReset {
        task_id: value.get("task_id")?.as_str()?.to_string(),
        review_id: value.get("review_id")?.as_str()?.to_string(),
        reviewer: value.get("reviewer")?.as_str()?.to_string(),
        reason: value
            .get("reason")
            .and_then(|reason| reason.as_str())
            .map(str::to_string),
    })
}

pub(crate) fn write_reviewer_reset_ack(
    brehon_root: &std::path::Path,
    request: &QueuedReviewerReset,
) -> std::io::Result<()> {
    let dir = brehon_root.join("runtime").join("reviewer-reset-acks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}--{}--{}.json",
        sanitize_runtime_key(&request.task_id),
        sanitize_runtime_key(&request.review_id),
        sanitize_runtime_key(&request.reviewer)
    ));
    let temp_path = path.with_extension("tmp");
    let payload = serde_json::json!({
        "task_id": request.task_id,
        "review_id": request.review_id,
        "reviewer": request.reviewer,
        "reason": request.reason,
        "reset_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        &temp_path,
        serde_json::to_string_pretty(&payload).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&temp_path, &path)
}

pub(crate) fn reviewer_reset_ack_exists(
    brehon_root: &std::path::Path,
    request: &QueuedReviewerReset,
) -> bool {
    let path = brehon_root
        .join("runtime")
        .join("reviewer-reset-acks")
        .join(format!(
            "{}--{}--{}.json",
            sanitize_runtime_key(&request.task_id),
            sanitize_runtime_key(&request.review_id),
            sanitize_runtime_key(&request.reviewer)
        ));
    path.exists()
}

/// Write a delivery acknowledgement for a queued prompt that was successfully
/// injected into the target pane. Keyed by `prompt_id` so the MCP side can
/// poll `agent action=delivery_status prompt_id=<id>` and observe that the
/// prompt actually landed — distinct from "MCP enqueued it" which is all that
/// `try_deliver_message` can know on its own.
pub(crate) fn write_prompt_delivery_ack(
    brehon_root: &std::path::Path,
    prompt_id: &str,
    target: &str,
    method: &str,
) -> std::io::Result<()> {
    let trimmed = prompt_id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let dir = brehon_root.join("runtime").join("prompt-delivery-acks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", sanitize_runtime_key(trimmed)));
    let temp_path = path.with_extension("tmp");
    let payload = serde_json::json!({
        "prompt_id": trimmed,
        "target": target,
        "method": method,
        "injected_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        &temp_path,
        serde_json::to_string_pretty(&payload).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&temp_path, &path)
}

pub(crate) fn write_worker_recycle_ack(
    brehon_root: &std::path::Path,
    request: &QueuedWorkerRecycle,
) -> std::io::Result<()> {
    let dir = brehon_root.join("runtime").join("worker-recycle-acks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}--{}.json",
        sanitize_runtime_key(&request.task_id),
        sanitize_runtime_key(&request.worker)
    ));
    let temp_path = path.with_extension("tmp");
    let payload = serde_json::json!({
        "task_id": request.task_id,
        "worker": request.worker,
        "recycled_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        &temp_path,
        serde_json::to_string_pretty(&payload).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&temp_path, &path)
}

/// Load the compact supervisor feedback summary written by the
/// supervisor feedback cache into `.brehon/runtime/feedback/{task_id}.json`.
/// Returns `None` when the file is missing or malformed; the TUI then
/// renders no feedback section rather than spurious "missing" placeholders.
pub(crate) fn read_feedback_summary_for(
    task_id: &str,
) -> Option<brehon_types::FeedbackTaskSummary> {
    read_runtime_summary_for(task_id, "feedback")
}

/// Load the compact proof bundle summary written by the MCP proof
/// recorders into `.brehon/runtime/proof/{task_id}.json`. Returns `None`
/// when the file is missing or malformed; the TUI then renders no proof
/// section rather than spurious "missing" placeholders.
pub(crate) fn read_proof_summary_for(task_id: &str) -> Option<brehon_types::ProofSummary> {
    read_runtime_summary_for(task_id, "proof")
}

fn read_runtime_summary_for<T: serde::de::DeserializeOwned>(
    task_id: &str,
    subdir: &str,
) -> Option<T> {
    let task_id = task_id.trim();
    if task_id.is_empty() {
        return None;
    }
    if !task_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return None;
    }
    let root = std::env::var_os("BREHON_ROOT")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(".brehon")))?;
    let path = root
        .join("runtime")
        .join(subdir)
        .join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(crate) fn read_optional_string(v: &serde_json::Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

pub(crate) fn read_optional_u64(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

pub(crate) fn read_string_list(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn read_reviewer_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| {
                    value.as_str().map(ToOwned::to_owned).or_else(|| {
                        value
                            .get("reviewer")
                            .and_then(|reviewer| reviewer.as_str())
                            .map(ToOwned::to_owned)
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn read_review_finding_summaries(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| {
                    if let Some(text) = value.as_str() {
                        return Some(text.to_string());
                    }

                    let description = value.get("description")?.as_str()?.to_string();
                    let location = match (
                        value.get("file").and_then(|file| file.as_str()),
                        value.get("line").and_then(|line| line.as_u64()),
                    ) {
                        (Some(file), Some(line)) => format!("[{file}:{line}] "),
                        (Some(file), None) => format!("[{file}] "),
                        _ => String::new(),
                    };
                    let suggestion = value
                        .get("suggestion")
                        .and_then(|suggestion| suggestion.as_str())
                        .map(str::trim)
                        .filter(|suggestion| !suggestion.is_empty());
                    Some(match suggestion {
                        Some(suggestion) => {
                            format!("{location}{description} — Suggestion: {suggestion}")
                        }
                        None => format!("{location}{description}"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn read_review_contexts(
    brehon_root: &std::path::Path,
) -> std::collections::HashMap<String, ReviewContextSnapshot> {
    let mut contexts = std::collections::HashMap::new();

    let reviews_dir = brehon_root.join("runtime").join("reviews");
    if let Ok(entries) = std::fs::read_dir(&reviews_dir) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let state_path = entry.path().join("state.json");
            let Ok(content) = std::fs::read_to_string(&state_path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            let fallback_task_id = entry.file_name().to_string_lossy().to_string();
            let task_id = value
                .get("task_id")
                .and_then(|task_id| task_id.as_str())
                .unwrap_or(fallback_task_id.as_str())
                .to_string();
            let context = contexts
                .entry(task_id)
                .or_insert_with(ReviewContextSnapshot::default);
            context.review_id = read_optional_string(&value, "current_review_id");
            context.review_status = read_optional_string(&value, "status");
            context.review_round = read_optional_u64(&value, "current_round");
            context.review_panel_id = read_optional_string(&value, "panel_id");
            context.review_panel_members = read_reviewer_list(value.get("panel"));
        }
    }

    let leases_dir = brehon_root.join("runtime").join("review-panels");
    if let Ok(entries) = std::fs::read_dir(&leases_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            let Some(task_id) = value.get("task_id").and_then(|task_id| task_id.as_str()) else {
                continue;
            };
            let context = contexts
                .entry(task_id.to_string())
                .or_insert_with(ReviewContextSnapshot::default);
            context.has_lease = true;
            context.review_panel_id = read_optional_string(&value, "panel_id")
                .or_else(|| context.review_panel_id.clone());
            context.review_id =
                read_optional_string(&value, "review_id").or_else(|| context.review_id.clone());
            context.review_round = read_optional_u64(&value, "round").or(context.review_round);
            let lease_members = read_reviewer_list(value.get("members"));
            if !lease_members.is_empty() {
                context.review_panel_members = lease_members;
            }
        }
    }

    contexts
}

#[allow(dead_code)]
pub(crate) fn read_pending_review_obligations(
    brehon_root: &std::path::Path,
    tasks: &[TaskInfo],
) -> std::collections::HashMap<String, Vec<PendingReviewObligation>> {
    let mut obligations = std::collections::HashMap::<String, Vec<PendingReviewObligation>>::new();
    let reviews_dir = brehon_root.join("runtime").join("reviews");
    let Ok(entries) = std::fs::read_dir(&reviews_dir) else {
        return obligations;
    };

    let task_info_by_id = tasks
        .iter()
        .map(|task| {
            (
                task.id.as_str(),
                (task.title.as_str(), task.status.as_str()),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }

        let state_path = entry.path().join("state.json");
        let Ok(content) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };

        if read_optional_string(&value, "status").as_deref() != Some("collecting") {
            continue;
        }

        let fallback_task_id = entry.file_name().to_string_lossy().to_string();
        let task_id = read_optional_string(&value, "task_id").unwrap_or(fallback_task_id);
        let review_id = read_optional_string(&value, "current_review_id").unwrap_or_default();
        if review_id.is_empty() {
            continue;
        }

        let panel = read_reviewer_list(value.get("panel"));
        if panel.is_empty() {
            continue;
        }
        let submitted: std::collections::HashSet<String> =
            read_reviewer_list(value.get("submissions_received"))
                .into_iter()
                .collect();
        let pending: Vec<String> = panel
            .iter()
            .filter(|reviewer| !submitted.contains(*reviewer))
            .cloned()
            .collect();
        if pending.is_empty() {
            continue;
        }

        let Some((task_title, task_status)) = task_info_by_id.get(task_id.as_str()).copied() else {
            continue;
        };
        if normalize_task_status(task_status) != Some("in_review") {
            continue;
        }
        let panel_id = read_optional_string(&value, "panel_id");
        let round = read_optional_u64(&value, "current_round");
        let pending_reviewers = pending.len();
        let reviewer_assignments = value
            .get("reviewer_assignments")
            .and_then(|value| value.as_object());

        for reviewer in pending {
            let assignment =
                reviewer_assignments.and_then(|assignments| assignments.get(&reviewer));
            let assignment_delivery_state = assignment
                .and_then(|value| value.get("prompt_id"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(|prompt_id| resolve_prompt_delivery_state(brehon_root, prompt_id));
            let assignment_acknowledged_at = assignment
                .and_then(|value| value.get("acknowledged_at"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            obligations
                .entry(reviewer)
                .or_default()
                .push(PendingReviewObligation {
                    task_id: task_id.clone(),
                    task_title: task_title.to_string(),
                    review_id: review_id.clone(),
                    panel_id: panel_id.clone(),
                    round,
                    pending_reviewers,
                    assignment_delivery_state,
                    assignment_acknowledged_at,
                });
        }
    }

    for reviewer_obligations in obligations.values_mut() {
        reviewer_obligations.sort_by(|left, right| {
            left.task_id
                .cmp(&right.task_id)
                .then(left.review_id.cmp(&right.review_id))
        });
    }

    obligations
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisorDispatchFrontier {
    pub idle_workers: Vec<String>,
    pub integration_conflict_tasks: Vec<String>,
    pub pending_tasks: Vec<String>,
    pub changes_requested_tasks: Vec<String>,
    pub review_ready_tasks: Vec<String>,
    pub approved_tasks: Vec<String>,
}

impl SupervisorDispatchFrontier {
    pub(crate) fn signature(&self) -> String {
        format!(
            "idle:{}|conflicts:{}|pending:{}|changes:{}|review:{}|approved:{}",
            self.idle_workers.join(","),
            self.integration_conflict_tasks.join(","),
            self.pending_tasks.join(","),
            self.changes_requested_tasks.join(","),
            self.review_ready_tasks.join(","),
            self.approved_tasks.join(","),
        )
    }
}

fn task_has_terminal_ancestor(
    task: &TaskInfo,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> bool {
    let mut current_parent = task.parent_id.as_deref();
    while let Some(parent_id) = current_parent {
        let Some(parent) = tasks_by_id.get(parent_id).copied() else {
            break;
        };
        if task_is_terminal(parent) {
            return true;
        }
        current_parent = parent.parent_id.as_deref();
    }
    false
}

fn task_reserves_worker_slot(task: &TaskInfo) -> bool {
    if task.integration_conflict_owner.as_deref() == Some("supervisor") {
        return false;
    }

    match normalize_task_status(&task.status) {
        Some(
            "assigned" | "in_progress" | "review_ready" | "in_review" | "changes_requested"
            | "approved",
        ) => true,
        Some(_) => false,
        None => true,
    }
}

fn task_has_started_worker_execution(task: &TaskInfo) -> bool {
    task.assignee
        .as_deref()
        .map(str::trim)
        .is_some_and(|assignee| !assignee.is_empty())
        && (task.percent.is_some_and(|percent| percent > 0)
            || task
                .activity
                .as_deref()
                .map(str::trim)
                .is_some_and(|activity| !activity.is_empty()))
}

fn task_dependencies_are_satisfied(
    task: &TaskInfo,
    tasks_by_id: &std::collections::HashMap<&str, &TaskInfo>,
) -> bool {
    !task.dependencies.is_empty()
        && task.dependencies.iter().all(|dependency_id| {
            tasks_by_id
                .get(dependency_id.as_str())
                .copied()
                .is_some_and(task_is_terminal)
        })
}

fn task_has_dependency_scoped_blocker_text(task: &TaskInfo) -> bool {
    let Some(blockers) = task.blockers.as_deref().map(str::trim) else {
        return false;
    };
    if blockers.is_empty() || task.dependencies.is_empty() {
        return false;
    }

    let blockers_lower = blockers.to_ascii_lowercase();
    task.dependencies
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

pub(crate) fn compute_supervisor_dispatch_frontier(
    tasks: &[TaskInfo],
    sessions: &std::collections::HashMap<String, (String, String, String)>,
) -> Option<SupervisorDispatchFrontier> {
    let tasks_by_id: std::collections::HashMap<&str, &TaskInfo> =
        tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    let live_workers: std::collections::HashSet<&str> = sessions
        .iter()
        .filter(|(_, (role, _, _))| role == "worker")
        .map(|(name, _)| name.as_str())
        .collect();
    let busy_workers: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|task| task_reserves_worker_slot(task))
        .filter_map(|task| task.assignee.as_deref().map(str::trim))
        .filter(|assignee| !assignee.is_empty())
        .collect();
    let mut idle_workers: Vec<String> = live_workers
        .iter()
        .copied()
        .filter(|worker| !busy_workers.contains(worker))
        .map(str::to_string)
        .collect();
    idle_workers.sort();

    let mut pending_tasks = Vec::new();
    let mut integration_conflict_tasks = Vec::new();
    let mut changes_requested_tasks = Vec::new();
    let mut review_ready_tasks = Vec::new();
    let mut approved_tasks = Vec::new();

    for task in tasks {
        if task.task_type != "task" || task_has_terminal_ancestor(task, &tasks_by_id) {
            continue;
        }
        if task.integration_conflict_owner.as_deref() == Some("supervisor") {
            integration_conflict_tasks.push(task.id.clone());
            continue;
        }
        let status = normalize_task_status(&task.status).unwrap_or("pending");
        let assignee = task.assignee.as_deref().map(str::trim).unwrap_or("");
        let unassigned = assignee.is_empty();
        let assignee_live = unassigned || live_workers.contains(assignee);
        let effectively_pending_blocked = status == "blocked"
            && task_dependencies_are_satisfied(task, &tasks_by_id)
            && task_has_dependency_scoped_blocker_text(task)
            && (!task_has_started_worker_execution(task) || !assignee_live);
        match status {
            "pending" if unassigned => pending_tasks.push(task.id.clone()),
            "blocked" if effectively_pending_blocked => pending_tasks.push(task.id.clone()),
            "changes_requested" if unassigned => changes_requested_tasks.push(task.id.clone()),
            "review_ready" => review_ready_tasks.push(task.id.clone()),
            "approved" if task.completion_mode.as_deref().unwrap_or_default().trim() == "merge" => {
                approved_tasks.push(task.id.clone())
            }
            _ => {}
        }
    }

    integration_conflict_tasks.sort();
    pending_tasks.sort();
    changes_requested_tasks.sort();
    review_ready_tasks.sort();
    approved_tasks.sort();

    let actionable = !integration_conflict_tasks.is_empty()
        || !review_ready_tasks.is_empty()
        || !approved_tasks.is_empty()
        || (!idle_workers.is_empty()
            && (!pending_tasks.is_empty() || !changes_requested_tasks.is_empty()));
    if !actionable {
        return None;
    }

    Some(SupervisorDispatchFrontier {
        idle_workers,
        integration_conflict_tasks,
        pending_tasks,
        changes_requested_tasks,
        review_ready_tasks,
        approved_tasks,
    })
}

pub(crate) fn build_supervisor_dispatch_nudge_message(
    frontier: &SupervisorDispatchFrontier,
    host_owned: bool,
) -> String {
    let idle_workers = if frontier.idle_workers.is_empty() {
        "none".to_string()
    } else {
        frontier.idle_workers.join(", ")
    };
    let mut queues = Vec::new();
    if !frontier.integration_conflict_tasks.is_empty() {
        queues.push(format!(
            "integration_conflict={}",
            frontier.integration_conflict_tasks.join(", ")
        ));
    }
    if !frontier.pending_tasks.is_empty() {
        queues.push(format!("pending={}", frontier.pending_tasks.join(", ")));
    }
    if !frontier.changes_requested_tasks.is_empty() {
        queues.push(format!(
            "changes_requested={}",
            frontier.changes_requested_tasks.join(", ")
        ));
    }
    if !frontier.review_ready_tasks.is_empty() {
        queues.push(format!(
            "review_ready={}",
            frontier.review_ready_tasks.join(", ")
        ));
    }
    if !frontier.approved_tasks.is_empty() {
        queues.push(format!("approved={}", frontier.approved_tasks.join(", ")));
    }

    let next_step = if host_owned {
        if frontier.integration_conflict_tasks.is_empty() {
            "Use `task action=ready` directly to get full state, then request review, integrate approved work, or assign idle workers before ending your turn. Do not wait for operator confirmation."
        } else {
            "Use `task action=ready` and `task action=conflicts` directly to get full state, then resolve or explicitly triage supervisor-owned integration conflicts before requesting review, integrating approved work, or assigning idle workers. Do not wait for operator confirmation."
        }
    } else if frontier.integration_conflict_tasks.is_empty() {
        "Re-run `task action=ready` now, then request review, integrate approved work, or assign idle workers before ending your turn."
    } else {
        "Re-run `task action=ready` and `task action=conflicts` now, then resolve or explicitly triage supervisor-owned integration conflicts before requesting review, integrating approved work, or assigning idle workers."
    };

    format!(
        "The frontier changed while you were idle. Idle workers: {idle_workers}. Actionable queues: {}. {next_step}",
        queues.join("; ")
    )
}

pub(crate) fn read_live_reviewer_panels(
    _brehon_root: &std::path::Path,
    fallback_panels: &[ReviewerPanel],
) -> Vec<ReviewerPanel> {
    // Reviewer panels are the terminal's physical tab groups. Runtime panel
    // leases are per-task reservations, and share-after-submit can make those
    // lease records sparse or overlapping without changing the configured UI.
    fallback_panels.to_vec()
}

pub(crate) fn description_mentions_heading(description: &str, headings: &[&str]) -> bool {
    let lower = description.to_ascii_lowercase();
    headings
        .iter()
        .any(|heading| lower.contains(&format!("{heading}:")) || lower.contains(heading))
}

pub(crate) fn has_active_descendant(tasks: &[TaskInfo], id: &str) -> bool {
    let direct_children: Vec<&TaskInfo> = tasks
        .iter()
        .filter(|task| task.parent_id.as_deref() == Some(id))
        .collect();

    direct_children.iter().any(|child| {
        !task_is_terminal(child)
            || (task_is_container(child) && has_active_descendant(tasks, &child.id))
    })
}

pub(crate) fn has_nonterminal_container_ancestor(tasks: &[TaskInfo], task: &TaskInfo) -> bool {
    let mut current_parent = task.parent_id.as_deref();
    while let Some(parent_id) = current_parent {
        let Some(parent) = tasks.iter().find(|candidate| candidate.id == parent_id) else {
            break;
        };
        if task_is_container(parent) && !task_is_terminal(parent) {
            return true;
        }
        current_parent = parent.parent_id.as_deref();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_prompt_record(path: &std::path::Path, prompt_id: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            serde_json::json!({
                "prompt_id": prompt_id
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn resolve_prompt_delivery_state_prioritizes_all_known_variants() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let prompt_id = "prompt:1";
        let runtime = root.join("runtime");

        assert_eq!(
            resolve_prompt_delivery_state(root, prompt_id).as_deref(),
            Some("unknown")
        );

        // Use arbitrary filenames for ack files to prove content-based
        // matching (not filename lookup) drives correctness.
        let enqueue_ack = runtime.join("prompt-enqueue-acks").join("ack1.json");
        write_prompt_record(&enqueue_ack, prompt_id);
        assert_eq!(
            resolve_prompt_delivery_state(root, prompt_id).as_deref(),
            Some("drained_without_ack")
        );

        let queued_entry = runtime
            .join("prompt-queue")
            .join("session-a")
            .join("001.prompt");
        write_prompt_record(&queued_entry, prompt_id);
        assert_eq!(
            resolve_prompt_delivery_state(root, prompt_id).as_deref(),
            Some("queued")
        );

        let delivery_ack = runtime.join("prompt-delivery-acks").join("ack2.json");
        write_prompt_record(&delivery_ack, prompt_id);
        assert_eq!(
            resolve_prompt_delivery_state(root, prompt_id).as_deref(),
            Some("injected")
        );

        let dead_letter = runtime
            .join("prompt-dead-letter")
            .join("session-a")
            .join("001.entry");
        write_prompt_record(&dead_letter, prompt_id);
        assert_eq!(
            resolve_prompt_delivery_state(root, prompt_id).as_deref(),
            Some("dead_lettered")
        );
    }

    #[test]
    fn prompt_ack_file_for_id_edge_cases() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("acks");
        std::fs::create_dir_all(&dir).unwrap();

        // Empty dir → None
        assert_eq!(prompt_ack_file_for_id(&dir, "p1"), None);

        // Dir with only non-JSON files → None
        std::fs::write(dir.join("foo.txt"), "not json").unwrap();
        assert_eq!(prompt_ack_file_for_id(&dir, "p1"), None);

        // Dir with unrelated JSON files → None
        std::fs::write(
            dir.join("bar.json"),
            serde_json::json!({ "prompt_id": "p2" }).to_string(),
        )
        .unwrap();
        assert_eq!(prompt_ack_file_for_id(&dir, "p1"), None);

        // Dir with matching file → Some(path)
        std::fs::write(
            dir.join("baz.json"),
            serde_json::json!({ "prompt_id": "p1", "extra": 42 }).to_string(),
        )
        .unwrap();
        let path = prompt_ack_file_for_id(&dir, "p1").expect("should find matching ack");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "baz.json");
    }

    #[test]
    fn prompt_id_exists_in_dir_tree_respects_max_depth_zero() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("queue");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("entry.prompt"),
            serde_json::json!({ "prompt_id": "p1" }).to_string(),
        )
        .unwrap();

        assert!(
            !prompt_id_exists_in_dir_tree(&dir, "p1", 0),
            "max_depth=0 should return false even when a matching file exists"
        );
        assert!(
            prompt_id_exists_in_dir_tree(&dir, "p1", 1),
            "max_depth=1 should find the matching file"
        );
    }
}
