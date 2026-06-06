//! Worker status helpers: liveness detection, task reservation, nudge state.

use serde_json::Value;
use std::collections::HashSet;

use brehon_ports::EventStore;
use brehon_types::{normalize_task_status, EventFilter, EventKind};

use crate::server::ToolResult;
use crate::tools::error_result;
use crate::tools::task_actions::{
    task_has_active_integration, task_has_supervisor_integration_conflict,
};

use super::paths::{brehon_root, read_all_tasks, read_sessions};
use super::worktree_ops::candidate_worker_worktree_paths;

/// Heartbeat staleness threshold in seconds (5 minutes).
pub(super) const HEARTBEAT_THRESHOLD_SECS: i64 = 300;

/// Output silence threshold in seconds (10 minutes).
pub(super) const OUTPUT_THRESHOLD_SECS: i64 = 600;

pub(super) const FORCE_REASSIGN_PARAM: &str = "force_reassign";

pub(super) fn require_supervisor_role() -> Result<(), ToolResult> {
    let role = std::env::var("BREHON_AGENT_ROLE").unwrap_or_default();
    if role == "supervisor" {
        Ok(())
    } else {
        Err(error_result(
            "Only supervisors can assign workers or change task ownership.",
        ))
    }
}

pub(super) fn live_worker_names() -> HashSet<String> {
    read_sessions()
        .into_iter()
        .filter(|entry| entry.get("role").and_then(|v| v.as_str()) == Some("worker"))
        .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()).map(String::from))
        .filter(|name| !agent_is_unavailable(name))
        .collect()
}

pub(super) fn agent_is_unavailable(agent_name: &str) -> bool {
    let Some(path) = agent_health_path(agent_name) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    value.get("status").and_then(Value::as_str) == Some("unavailable")
}

pub(super) fn agent_health(agent_name: &str) -> Option<Value> {
    let path = agent_health_path(agent_name)?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&content).ok()
}

fn agent_health_path(agent_name: &str) -> Option<std::path::PathBuf> {
    Some(
        brehon_root()?
            .join("runtime")
            .join("agent-health")
            .join(format!("{}.json", sanitize_path_component(agent_name))),
    )
}

fn sanitize_path_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "agent".to_string()
    } else {
        out
    }
}

pub(super) fn active_tasks_for_worker(
    worker_name: &str,
    excluding_task_id: Option<&str>,
) -> Vec<serde_json::Map<String, Value>> {
    read_all_tasks()
        .into_iter()
        .filter(|task| task.get("assignee").and_then(|v| v.as_str()) == Some(worker_name))
        .filter(|task| {
            task.get("task_id")
                .and_then(|v| v.as_str())
                .zip(excluding_task_id)
                .is_none_or(|(task_id, excluded)| task_id != excluded)
        })
        .filter(task_reserves_worker)
        .collect()
}

pub(super) fn task_reserves_worker(task: &serde_json::Map<String, Value>) -> bool {
    if task_has_supervisor_integration_conflict(task) || task_has_active_integration(task) {
        return false;
    }

    match task.get("status").and_then(|v| v.as_str()) {
        Some(status) => match normalize_task_status(status) {
            Some(
                "assigned" | "in_progress" | "review_ready" | "in_review" | "changes_requested"
                | "approved",
            ) => true,
            Some(_) => false,
            None => true,
        },
        None => true,
    }
}

/// Query the event store for nudge state for a given session.
/// Returns (delivery_state, last_nudge_at, nudges_sent_count).
pub(super) async fn query_nudge_state(
    store: &dyn EventStore,
    session_id: &str,
) -> (Option<String>, Option<String>, usize) {
    let filter = EventFilter {
        aggregate_id: Some(session_id.to_string()),
        ..EventFilter::default()
    };
    let events = match store.query(filter).await {
        Ok(events) => events,
        Err(_) => return (None, None, 0),
    };

    let mut nudges_sent = 0usize;
    let mut last_nudge_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut delivery_state: Option<String> = None;

    for event in &events {
        match &event.kind {
            EventKind::NudgeSent {
                session_id: sid, ..
            } if sid == session_id => {
                nudges_sent += 1;
                last_nudge_at = Some(event.timestamp);
                delivery_state = Some("Delivered".to_string());
            }
            EventKind::NudgeAcknowledged {
                session_id: sid, ..
            } if sid == session_id => {
                delivery_state = Some("Acknowledged".to_string());
            }
            EventKind::NudgeActedOn {
                session_id: sid, ..
            } if sid == session_id => {
                delivery_state = Some("ActedOn".to_string());
            }
            EventKind::NudgeTimedOut {
                session_id: sid, ..
            } if sid == session_id => {
                delivery_state = Some("TimedOut".to_string());
            }
            _ => {}
        }
    }

    let last_nudge_str = last_nudge_at.map(|t| t.to_rfc3339());
    (delivery_state, last_nudge_str, nudges_sent)
}

/// Inspect a worker's worktree for branch, merge-target alignment, and dirty state.
pub(super) fn inspect_worktree(worker_name: &str, merge_target: Option<&str>) -> Value {
    let matches = candidate_worker_worktree_paths(worker_name);
    if matches.is_empty() {
        return serde_json::json!({
            "worktree_exists": false
        });
    }
    if matches.len() > 1 {
        let match_list: Vec<_> = matches.iter().map(|p| p.display().to_string()).collect();
        return serde_json::json!({
            "worktree_exists": false,
            "error": format!(
                "ambiguous run-scoped worktree candidates for worker '{}': {}",
                worker_name,
                match_list.join(", ")
            )
        });
    }
    let worktree_path = matches.into_iter().next().unwrap();

    // Use git2 to inspect
    let repo = match git2::Repository::open(&worktree_path) {
        Ok(r) => r,
        Err(_) => {
            return serde_json::json!({
                "worktree_exists": true,
                "error": "could not open git repository"
            });
        }
    };

    let worktree_branch = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(String::from));

    let on_merge_target_branch = match (&worktree_branch, merge_target) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => false,
    };

    // Check dirty state
    let mut dirty_count = 0u32;
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true);
    if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
        dirty_count = statuses
            .iter()
            .filter(|entry| match entry.path() {
                Some(path) => !brehon_git::is_brehon_local_scaffold_path(path),
                None => true,
            })
            .count() as u32;
    }

    let reassignment_safe = dirty_count == 0;

    serde_json::json!({
        "worktree_exists": true,
        "worktree_path": worktree_path,
        "worktree_branch": worktree_branch,
        "merge_target": merge_target,
        "branch_drifted": Value::Null,
        "on_merge_target_branch": on_merge_target_branch,
        "reassignment_safe": reassignment_safe,
        "dirty_files_count": dirty_count
    })
}
