//! JSON reading helpers and small utility predicates.

mod pane;

pub(crate) use pane::pane_needs_post_spawn_prompt;

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

fn sanitize_runtime_key(value: &str) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    sanitized
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

        for reviewer in pending {
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

    let next_step = if frontier.integration_conflict_tasks.is_empty() {
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
