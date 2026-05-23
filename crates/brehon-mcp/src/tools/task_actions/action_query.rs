//! Handlers for query-type task actions: list, mine, conflicts, ready.

use serde_json::Value;

use brehon_types::{is_terminal_task_status, normalize_task_status};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::routing::routing_summary;
use crate::tools::verification::{find_panel_lease_by_task, read_review_state, read_round_request};
use crate::tools::{error_result, text_result};

use super::dependencies::{
    task_has_legacy_completed_worker_status, task_has_recoverable_worker_state_blocker_text,
};
use super::epic::{
    check_child_completion, check_epic_integration_status, task_has_supervisor_integration_conflict,
};
use super::followups::summarize_followups;
use super::git_ops::detect_default_branch;
use super::lifecycle::{
    ancestor_chain_has_closed_parent, child_collection_label, direct_children, is_container_task,
    is_epic, is_initiative, reconcile_dependency_states,
};
use super::paths::{brehon_root_dir, project_root};
use super::persistence::{read_all_tasks, read_task};
use super::structured_spec::control_plane_scope_issue_for_task;

pub(super) fn read_active_review_obligations(reviewer: &str) -> Vec<Value> {
    let Some(reviews_dir) = brehon_root_dir().map(|root| root.join("runtime").join("reviews"))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&reviews_dir) else {
        return Vec::new();
    };

    let mut obligations = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(task_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(state) = read_review_state(&task_id) else {
            continue;
        };
        if state.status != "collecting" {
            continue;
        }
        if !state.panel.iter().any(|member| member == reviewer) {
            continue;
        }
        if state
            .submissions_received
            .iter()
            .any(|member| member == reviewer)
        {
            continue;
        }

        let pending: Vec<String> = state
            .panel
            .iter()
            .filter(|member| !state.submissions_received.contains(*member))
            .cloned()
            .collect();
        let request = read_round_request(&task_id, state.current_round);
        let task = read_task(&task_id);
        if !task.as_ref().is_some_and(|task| {
            normalize_task_status(
                task.get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("pending"),
            ) == Some("in_review")
        }) {
            continue;
        }
        let title = request
            .as_ref()
            .map(|request| request.title.clone())
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                task.as_ref()
                    .and_then(|task| task.get("title").and_then(|value| value.as_str()))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "(untitled)".to_string());

        let mut obligation = serde_json::Map::new();
        obligation.insert(
            "assignment_kind".into(),
            Value::String("review".to_string()),
        );
        obligation.insert("reviewer".into(), Value::String(reviewer.to_string()));
        obligation.insert("task_id".into(), Value::String(task_id.clone()));
        obligation.insert(
            "review_id".into(),
            Value::String(state.current_review_id.clone()),
        );
        obligation.insert(
            "round".into(),
            Value::Number(serde_json::Number::from(state.current_round)),
        );
        obligation.insert("status".into(), Value::String(state.status.clone()));
        obligation.insert("task_status".into(), Value::String("in_review".to_string()));
        obligation.insert(
            "action_required".into(),
            Value::String("submit_review".to_string()),
        );
        obligation.insert("panel_id".into(), Value::String(state.panel_id.clone()));
        obligation.insert("title".into(), Value::String(title));
        obligation.insert(
            "panel_progress".into(),
            serde_json::json!(format!(
                "{}/{}",
                state.submissions_received.len(),
                state.panel.len()
            )),
        );
        obligation.insert(
            "pending_reviewers".into(),
            Value::Array(pending.into_iter().map(Value::String).collect()),
        );
        obligation.insert(
            "panel_lease_state".into(),
            Value::String(
                find_panel_lease_by_task(&task_id)
                    .map(|_| "leased".to_string())
                    .unwrap_or_else(|| "missing".to_string()),
            ),
        );
        obligation.insert(
            "next_action".into(),
            serde_json::json!({
                "kind": "submit_review",
                "tool": "verification",
                "args": {
                    "action": "submit_review",
                    "review_id": state.current_review_id.clone(),
                    "reviewer": reviewer
                }
            }),
        );
        if let Some(request) = request {
            obligation.insert(
                "requested_by".into(),
                Value::String(request.requested_by.clone()),
            );
            obligation.insert(
                "requested_at".into(),
                Value::String(request.requested_at.clone()),
            );
            if !request.commit.trim().is_empty() {
                obligation.insert("commit".into(), Value::String(request.commit));
            }
        }
        obligations.push(Value::Object(obligation));
    }

    obligations.sort_by(|a, b| {
        let a_requested = a
            .get("requested_at")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let b_requested = b
            .get("requested_at")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        b_requested.cmp(a_requested)
    });
    obligations
}

fn ready_queue_task(
    task: &serde_json::Map<String, Value>,
    config: Option<&brehon_types::BrehonConfig>,
) -> Value {
    let mut value = Value::Object(task.clone());
    if let Some(summary) = summarize_followups(task) {
        value["followup_summary"] = summary;
    }
    if let Some(routing) = routing_summary(task, config) {
        value["routing"] = routing;
    }
    value["liveness"] = task_liveness_context(task);
    value
}

fn task_liveness_context(task: &serde_json::Map<String, Value>) -> Value {
    let assignee = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let review_owner = task
        .get("review_owner")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let owner = assignee.or(review_owner);
    let Some(owner) = owner else {
        return serde_json::json!({
            "owner": null,
            "state": "unassigned",
            "next_decision": "assign_or_repair",
        });
    };

    let Some(root) = brehon_root_dir() else {
        return serde_json::json!({
            "owner": owner,
            "state": "unknown",
            "next_decision": "refresh_runtime_context",
            "reason": "BREHON_ROOT is not configured",
        });
    };
    let session_path = root
        .join("runtime")
        .join("sessions")
        .join(format!("{owner}.json"));
    let Ok(content) = std::fs::read_to_string(&session_path) else {
        return serde_json::json!({
            "owner": owner,
            "state": "missing_session",
            "next_decision": "reassign_or_reseat",
        });
    };
    let Ok(session) = serde_json::from_str::<Value>(&content) else {
        return serde_json::json!({
            "owner": owner,
            "state": "bad_session_file",
            "next_decision": "repair_runtime",
        });
    };

    let live = crate::tools::agent::session_is_live(&session);
    serde_json::json!({
        "owner": owner,
        "state": if live { "live" } else { "stale_session" },
        "next_decision": if live { "wait_or_message" } else { "reassign_or_reseat" },
        "session_id": session.get("session_id").cloned().unwrap_or(Value::Null),
        "session_name": session.get("session_name").cloned().unwrap_or(Value::Null),
        "role": session.get("role").cloned().unwrap_or(Value::Null),
        "last_seen_at": session.get("last_seen_at").cloned().unwrap_or(Value::Null),
        "registered_at": session.get("registered_at").cloned().unwrap_or(Value::Null),
    })
}

fn task_has_recorded_handoff_commit(task: &serde_json::Map<String, Value>) -> bool {
    task.get("latest_commit")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
}

fn blocked_handoff_context(
    task: &serde_json::Map<String, Value>,
    all_tasks: &[serde_json::Map<String, Value>],
    config: Option<&brehon_types::BrehonConfig>,
) -> Option<Value> {
    let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let recoverable_blocked =
        status == "blocked" && task_has_recoverable_worker_state_blocker_text(task);
    let legacy_completed = task_has_legacy_completed_worker_status(task);
    if task_type != "task" || (!recoverable_blocked && !legacy_completed) {
        return None;
    }

    let has_commit = task_has_recorded_handoff_commit(task);
    let closed_parent = ancestor_chain_has_closed_parent(all_tasks, task);
    let scope_issue = control_plane_scope_issue_for_task(task);
    let safe_repair = has_commit && !closed_parent && scope_issue.is_none();
    let mut value = ready_queue_task(task, config);
    let task_id = queued_task_id(&value).unwrap_or("").to_string();
    value["safe_repair"] = Value::Bool(safe_repair);
    value["repair_action"] = if safe_repair {
        serde_json::json!({
            "kind": "recover_handoff",
            "tool": "task",
            "args": {
                "action": "recover_handoff",
                "id": task_id
            }
        })
    } else if !has_commit {
        serde_json::json!({
            "kind": "wait_for_worker_checkpoint_or_reassign",
            "tool": "task",
            "args": {
                "action": "ready"
            }
        })
    } else {
        serde_json::json!({
            "kind": "inspect_task",
            "tool": "task",
            "args": {
                "action": "list",
                "status": "blocked"
            }
        })
    };
    value["repair_blocker"] = if safe_repair {
        Value::Null
    } else if !has_commit {
        Value::String("latest_commit is missing".to_string())
    } else if legacy_completed {
        Value::String("legacy completed handoff state is not safe to repair".to_string())
    } else if closed_parent {
        Value::String("task has a closed ancestor".to_string())
    } else {
        Value::String(scope_issue.unwrap_or_else(|| "unsafe handoff state".to_string()))
    };
    Some(value)
}

fn queued_task_id(task: &Value) -> Option<&str> {
    task.get("id")
        .or_else(|| task.get("task_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// How long an assigned `changes_requested` task may sit without an
/// `updated_at` bump before it is surfaced to the supervisor as stalled.
///
/// Tuned to be larger than a typical worker think-and-edit cycle (one review
/// feedback round is usually in the low-single-digit minutes) but small
/// enough that a genuinely silent worker gets flagged before the supervisor
/// moves on. Environment override lets operators narrow the window on noisier
/// deployments.
const STALLED_CHANGES_REQUESTED_SECS_DEFAULT: i64 = 10 * 60;

fn stalled_changes_requested_threshold_secs() -> i64 {
    std::env::var("BREHON_STALLED_CHANGES_REQUESTED_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(STALLED_CHANGES_REQUESTED_SECS_DEFAULT)
}

/// If `task` is an assigned `changes_requested` task that has been silent
/// longer than the stall threshold, return a supervisor-facing summary.
///
/// Returned shape:
/// ```json
/// {
///   "task_id", "title", "assignee", "updated_at",
///   "idle_secs", "threshold_secs",
///   "percent", "activity",
///   "supervisor_action": "Check `agent action=delivery_status prompt_id=...`, `factory action=worker_status`, or reassign."
/// }
/// ```
/// This closes the loop from Fix 1–3: if the ACP event bus reports the worker
/// is actively producing `ResponseReceived`/`OperationStarted` events the
/// task's `updated_at` will move whenever the worker calls `task action=...`,
/// so a stalled entry here means the worker is not acting on the feedback.
fn stalled_changes_requested_entry(
    task: &serde_json::Map<String, Value>,
    all_tasks: &[serde_json::Map<String, Value>],
) -> Option<Value> {
    let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let task_type = task
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    if status != "changes_requested" || task_type != "task" {
        return None;
    }
    if task_has_supervisor_integration_conflict(task) {
        return None;
    }
    let assignee = task
        .get("assignee")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    if ancestor_chain_has_closed_parent(all_tasks, task) {
        return None;
    }
    if control_plane_scope_issue_for_task(task).is_some() {
        return None;
    }
    let updated_at_str = task.get("updated_at").and_then(|v| v.as_str())?;
    let updated_at = chrono::DateTime::parse_from_rfc3339(updated_at_str).ok()?;
    let now = chrono::Utc::now();
    let idle_secs = (now - updated_at.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    let threshold = stalled_changes_requested_threshold_secs();
    if idle_secs < threshold {
        return None;
    }

    let task_id = task
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let title = task
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let percent = task.get("percent").cloned().unwrap_or(Value::Null);
    let activity = task
        .get("activity")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(serde_json::json!({
        "task_id": task_id,
        "title": title,
        "assignee": assignee,
        "updated_at": updated_at_str,
        "idle_secs": idle_secs,
        "threshold_secs": threshold,
        "percent": percent,
        "activity": activity,
        "supervisor_action": "Worker has been silent past the stall threshold. Check `agent action=delivery_status prompt_id=<last nudge id>` to see whether your last message landed, `factory action=worker_status` to see whether the worker is producing any output, and consider reassigning if the worker is unreachable. Do NOT just re-send the same message — if it was already injected, the worker saw it and chose not to act."
    }))
}

fn supervisor_integration_conflicts_from_tasks(
    tasks: &[serde_json::Map<String, Value>],
) -> Vec<Value> {
    let mut conflicts: Vec<Value> = tasks
        .iter()
        .filter(|task| {
            let status = task
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            !is_terminal_task_status(status) && task_has_supervisor_integration_conflict(task)
        })
        .cloned()
        .map(Value::Object)
        .collect();

    conflicts.sort_by(|a, b| {
        let a_time = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let b_time = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });
    conflicts
}

pub(super) fn read_supervisor_integration_conflicts() -> Vec<Value> {
    supervisor_integration_conflicts_from_tasks(&read_all_tasks())
}

pub(super) fn sanitize_task_for_agent(
    mut task: serde_json::Map<String, Value>,
    role: &str,
) -> serde_json::Map<String, Value> {
    if role == "worker" {
        if let Some(plan_import) = task.get_mut("plan_import").and_then(|v| v.as_object_mut()) {
            plan_import.remove("source_file");
        }
    }
    task
}

pub(super) async fn execute_list(args: &Value) -> Result<ToolResult, McpError> {
    let task_type = args.get("task_type").and_then(|v| v.as_str());
    let status = args.get("status").and_then(|v| v.as_str());
    let include_closed = args
        .get("include_closed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut tasks: Vec<Value> = read_all_tasks()
        .into_iter()
        .filter(|t| {
            // Filter by task_type if specified
            if let Some(tt) = task_type {
                if t.get("task_type").and_then(|v| v.as_str()) != Some(tt) {
                    return false;
                }
            }
            // Filter by explicit status if specified
            if let Some(st) = status {
                if t.get("status").and_then(|v| v.as_str()) != Some(st) {
                    return false;
                }
            }
            // Exclude closed tasks by default unless status=closed
            // was explicitly requested or include_closed=true
            if !include_closed && status.is_none() {
                let task_status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if is_terminal_task_status(task_status) {
                    return false;
                }
            }
            true
        })
        .map(|t| {
            let mut v = Value::Object(t.clone());
            let current_task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            // For container tasks, include direct child progress summary.
            if is_container_task(current_task_type) {
                if let Some(id) = t.get("task_id").and_then(|v| v.as_str()) {
                    let (total, closed, all_done) = check_child_completion(id);
                    let progress_key = if is_initiative(current_task_type) {
                        "epic_progress"
                    } else {
                        "subtask_progress"
                    };
                    v[progress_key] = serde_json::json!({
                        "total": total,
                        "closed": closed,
                        "all_complete": all_done
                    });

                    // Include integration progress for feature epics only.
                    if is_epic(current_task_type)
                        && t.get("integration_branch")
                            .and_then(|v| v.as_str())
                            .map(|b| !b.is_empty())
                            .unwrap_or(false)
                    {
                        let (total_subs, integrated, _) = check_epic_integration_status(id);
                        v["integration_progress"] = serde_json::json!({
                            "integrated": integrated,
                            "total": total_subs
                        });
                    }
                    if let Some(worktree) = t.get("integration_worktree").and_then(|v| v.as_str()) {
                        if !worktree.is_empty() {
                            v["integration_worktree"] = Value::String(worktree.to_string());
                        }
                    }
                }
            }
            if let Some(summary) = summarize_followups(&t) {
                v["followup_summary"] = summary;
            }
            // For subtasks, always include merge_target if present
            if let Some(merge_target) = t.get("merge_target").and_then(|v| v.as_str()) {
                if !merge_target.is_empty() {
                    v["merge_target"] = Value::String(merge_target.to_string());
                }
            }
            v
        })
        .collect();

    tasks.sort_by(|a, b| {
        let a_time = a.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let b_time = b.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });

    let count = tasks.len();
    let result = serde_json::json!({
        "tasks": tasks,
        "count": count,
        "filter": {
            "task_type": task_type,
            "status": status,
            "include_closed": include_closed
        }
    });

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_mine(args: &Value) -> Result<ToolResult, McpError> {
    let _ = args;
    let agent_name = std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "unknown".to_string());
    let role = std::env::var("BREHON_AGENT_ROLE").unwrap_or_else(|_| "unknown".to_string());

    let tasks: Vec<Value> = read_all_tasks()
        .into_iter()
        .filter(|t| {
            t.get("assignee").and_then(|v| v.as_str()) == Some(&agent_name)
                && task_is_visible_in_mine(t, &role)
        })
        .map(|task| Value::Object(sanitize_task_for_agent(task, &role)))
        .collect();

    // Do not gate this on BREHON_AGENT_ROLE. Some launchers expose the lane
    // name or lose role env propagation across the CLI -> MCP boundary. Panel
    // membership by agent name is the durable source of truth.
    let review_obligations = read_active_review_obligations(&agent_name);

    let task_count = tasks.len();
    let review_count = review_obligations.len();
    let count = task_count + review_count;
    let assignments = tasks
        .iter()
        .cloned()
        .map(|mut task| {
            if let Value::Object(ref mut object) = task {
                object.insert("assignment_kind".into(), Value::String("task".to_string()));
            }
            task
        })
        .chain(review_obligations.iter().cloned())
        .collect::<Vec<_>>();
    let worktree_cleanup = if role == "worker" && task_count > 0 {
        Some(
            super::build_artifact_cleanup::cleanup_current_worker_build_artifacts(
                "before_task_start",
            ),
        )
    } else {
        None
    };
    let mut result = serde_json::json!({
        "tasks": tasks,
        "review_obligations": review_obligations,
        "assignments": assignments,
        "count": count,
        "task_count": task_count,
        "review_count": review_count,
        "has_assigned_work": count > 0,
        "agent": agent_name,
        "role": role
    });
    if let Some(cleanup) = worktree_cleanup {
        result["worktree_cleanup"] = cleanup;
    }

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

fn task_is_visible_in_mine(task: &serde_json::Map<String, Value>, role: &str) -> bool {
    let status = task
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    if is_terminal_task_status(status) {
        return false;
    }

    if role == "worker" {
        return matches!(
            normalize_task_status(status),
            Some("assigned" | "in_progress" | "changes_requested")
        );
    }

    true
}

pub(super) async fn execute_conflicts(args: &Value) -> Result<ToolResult, McpError> {
    let _ = args;
    let tasks = read_supervisor_integration_conflicts();
    let count = tasks.len();
    let result = serde_json::json!({
        "tasks": tasks,
        "count": count,
        "type": "supervisor_integration_conflicts"
    });

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_children(args: &Value) -> Result<ToolResult, McpError> {
    let Some(id) = args.get("id").and_then(|value| value.as_str()) else {
        return Ok(error_result("Missing required parameter: id"));
    };
    let verbose = args
        .get("verbose")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let tasks = read_all_tasks();
    let Some(parent) = tasks
        .iter()
        .find(|task| task.get("task_id").and_then(|value| value.as_str()) == Some(id))
    else {
        return Ok(error_result(format!("Task not found: {id}")));
    };

    let parent_type = parent
        .get("task_type")
        .and_then(|value| value.as_str())
        .unwrap_or("task");
    let child_type = child_collection_label(parent_type);
    let children = direct_children(&tasks, id);

    let rendered_children: Vec<Value> = if verbose {
        children.into_iter().cloned().map(Value::Object).collect()
    } else {
        children
            .into_iter()
            .map(|task| {
                let mut projected = serde_json::Map::new();
                for key in [
                    "task_id",
                    "title",
                    "status",
                    "task_type",
                    "assignee",
                    "blocked_by",
                    "merge_target",
                    "integration_status",
                    "priority",
                    "percent",
                ] {
                    if let Some(value) = task.get(key) {
                        projected.insert(key.to_string(), value.clone());
                    }
                }
                Value::Object(projected)
            })
            .collect()
    };

    let payload = serde_json::json!({
        "parent_id": id,
        "parent_type": parent_type,
        "child_type": child_type,
        "total": rendered_children.len(),
        "verbose": verbose,
        "children": rendered_children,
    });

    Ok(text_result(
        serde_json::to_string_pretty(&payload)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}

pub(super) async fn execute_ready(args: &Value) -> Result<ToolResult, McpError> {
    let _ = args;
    if let Err(err) = reconcile_dependency_states().await {
        return Ok(error_result(err));
    }
    let all_tasks = read_all_tasks();
    let project_config =
        project_root().and_then(|root| brehon_config::load_config(Some(&root)).ok());
    let integration_conflict_tasks = supervisor_integration_conflicts_from_tasks(&all_tasks);
    let blocked_handoff_tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| !task_has_supervisor_integration_conflict(t))
        .filter_map(|task| blocked_handoff_context(task, &all_tasks, project_config.as_ref()))
        .collect();
    let recoverable_blocked_tasks: Vec<Value> = blocked_handoff_tasks
        .iter()
        .filter(|task| task.get("safe_repair").and_then(|v| v.as_bool()) == Some(true))
        .cloned()
        .collect();
    let tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let assignee = t.get("assignee");
            let task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if task_has_supervisor_integration_conflict(t) {
                return false;
            }
            // Only concrete worker tasks with no assignee.
            if status != "pending"
                || task_type != "task"
                || !(assignee.is_none()
                    || assignee == Some(&Value::Null)
                    || assignee.and_then(|v| v.as_str()) == Some(""))
            {
                return false;
            }
            if ancestor_chain_has_closed_parent(&all_tasks, t) {
                return false;
            }
            if control_plane_scope_issue_for_task(t).is_some() {
                return false;
            }
            true
        })
        .map(|task| ready_queue_task(task, project_config.as_ref()))
        .collect();
    let review_ready_tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if task_has_supervisor_integration_conflict(t) {
                return false;
            }
            if status != "review_ready" || task_type != "task" {
                return false;
            }
            if ancestor_chain_has_closed_parent(&all_tasks, t) {
                return false;
            }
            if control_plane_scope_issue_for_task(t).is_some() {
                return false;
            }
            true
        })
        .map(|task| ready_queue_task(task, project_config.as_ref()))
        .collect();
    let changes_requested_tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let assignee = t.get("assignee");
            let task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if task_has_supervisor_integration_conflict(t) {
                return false;
            }
            if status != "changes_requested"
                || task_type != "task"
                || !(assignee.is_none()
                    || assignee == Some(&Value::Null)
                    || assignee.and_then(|v| v.as_str()) == Some(""))
            {
                return false;
            }
            if ancestor_chain_has_closed_parent(&all_tasks, t) {
                return false;
            }
            if control_plane_scope_issue_for_task(t).is_some() {
                return false;
            }
            true
        })
        .map(|task| ready_queue_task(task, project_config.as_ref()))
        .collect();
    let stalled_tasks: Vec<Value> = all_tasks
        .iter()
        .filter_map(|t| stalled_changes_requested_entry(t, &all_tasks))
        .collect();
    let approved_tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| {
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            let completion_mode = t
                .get("completion_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if task_has_supervisor_integration_conflict(t) {
                return false;
            }
            if status != "approved" || task_type != "task" || completion_mode != "merge" {
                return false;
            }
            if ancestor_chain_has_closed_parent(&all_tasks, t) {
                return false;
            }
            if control_plane_scope_issue_for_task(t).is_some() {
                return false;
            }
            true
        })
        .map(|task| ready_queue_task(task, project_config.as_ref()))
        .collect();
    let followup_source_tasks: Vec<Value> = all_tasks
        .iter()
        .filter(|t| {
            let task_type = t
                .get("task_type")
                .and_then(|v| v.as_str())
                .unwrap_or("task");
            if task_has_supervisor_integration_conflict(t) {
                return false;
            }
            if task_type != "task" {
                return false;
            }
            if ancestor_chain_has_closed_parent(&all_tasks, t) {
                return false;
            }
            if control_plane_scope_issue_for_task(t).is_some() {
                return false;
            }
            summarize_followups(t)
                .and_then(|summary| summary.get("open").and_then(|v| v.as_u64()))
                .unwrap_or(0)
                > 0
        })
        .map(|task| ready_queue_task(task, project_config.as_ref()))
        .collect();

    let count = tasks.len();
    let integration_conflict_count = integration_conflict_tasks.len();
    let blocked_handoff_count = blocked_handoff_tasks.len();
    let recoverable_blocked_count = recoverable_blocked_tasks.len();
    let review_ready_count = review_ready_tasks.len();
    let changes_requested_count = changes_requested_tasks.len();
    let approved_count = approved_tasks.len();
    let followup_source_count = followup_source_tasks.len();
    let stalled_count = stalled_tasks.len();
    let mut priority_notes = Vec::new();
    if integration_conflict_count > 0 {
        priority_notes.push(format!(
            "{integration_conflict_count} supervisor-owned integration conflict(s) require immediate resolution"
        ));
    }
    if recoverable_blocked_count > 0 {
        priority_notes.push(format!(
            "{recoverable_blocked_count} blocked task(s) have recoverable worker handoff state and should be repaired with task action=repair_frontier"
        ));
    }
    if blocked_handoff_count > recoverable_blocked_count {
        priority_notes.push(format!(
            "{} worker handoff task(s) require worker checkpoint/reassignment before review can proceed",
            blocked_handoff_count - recoverable_blocked_count
        ));
    }
    if stalled_count > 0 {
        priority_notes.push(format!(
            "{stalled_count} assigned changes_requested task(s) have been silent past the stall threshold — check worker liveness and `agent action=delivery_status` before assuming they are still working"
        ));
    }
    if review_ready_count > 0 {
        priority_notes.push(format!(
            "{review_ready_count} task(s) are awaiting formal review request"
        ));
    }
    if changes_requested_count > 0 {
        priority_notes.push(format!(
            "{changes_requested_count} task(s) need worker reassignment after review feedback"
        ));
    }
    if approved_count > 0 {
        priority_notes.push(format!(
            "{approved_count} approved merge task(s) are awaiting supervisor integration"
        ));
    }
    if followup_source_count > 0 {
        priority_notes.push(format!(
            "{followup_source_count} task(s) have open approved-review followups that should be promoted or explicitly waived"
        ));
    }
    let default_branch = detect_default_branch().unwrap_or_else(|_| "main".to_string());
    let next_action = if integration_conflict_count > 0 {
        serde_json::json!({
            "kind": "inspect_integration_conflicts",
            "tool": "task",
            "args": {
                "action": "conflicts"
            }
        })
    } else if recoverable_blocked_count > 0 {
        serde_json::json!({
            "kind": "repair_frontier",
            "tool": "task",
            "description": "Apply deterministic safe repairs from ready.recoverable_blocked_tasks. This recovers blocked worker handoffs with recorded latest_commit, then you must call task action=ready again.",
            "args": {
                "action": "repair_frontier"
            }
        })
    } else if let Some(action) = blocked_handoff_tasks
        .first()
        .and_then(|task| task.get("repair_action"))
        .cloned()
    {
        action
    } else if let Some(task_id) = review_ready_tasks.first().and_then(queued_task_id) {
        serde_json::json!({
            "kind": "request_review",
            "tool": "verification",
            "args": {
                "action": "request_review",
                "task_id": task_id
            }
        })
    } else if let Some(task_id) = changes_requested_tasks.first().and_then(queued_task_id) {
        serde_json::json!({
            "kind": "assign_revision_worker",
            "tool": "factory",
            "args": {
                "action": "assign_workers",
                "task_id": task_id
            },
            "requires": ["workers"]
        })
    } else if let Some(task) = approved_tasks.first() {
        if let Some(task_id) = queued_task_id(task) {
            let merge_target = task
                .get("merge_target")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(default_branch.as_str());
            if merge_target == default_branch {
                serde_json::json!({
                    "kind": "close_approved_direct_merge_task",
                    "tool": "task",
                    "args": {
                        "action": "close",
                        "id": task_id
                    }
                })
            } else {
                serde_json::json!({
                    "kind": "integrate_approved_task",
                    "tool": "task",
                    "args": {
                        "action": "integrate",
                        "id": task_id
                    }
                })
            }
        } else {
            serde_json::json!({ "kind": "none" })
        }
    } else if let Some(task_id) = followup_source_tasks.first().and_then(queued_task_id) {
        serde_json::json!({
            "kind": "promote_review_followups",
            "tool": "task",
            "args": {
                "action": "promote_followups",
                "id": task_id
            }
        })
    } else if let Some(task_id) = tasks.first().and_then(queued_task_id) {
        serde_json::json!({
            "kind": "assign_worker",
            "tool": "factory",
            "args": {
                "action": "assign_workers",
                "task_id": task_id
            },
            "requires": ["workers"]
        })
    } else {
        serde_json::json!({ "kind": "none" })
    };
    let result = serde_json::json!({
        "tasks": tasks,
        "count": count,
        "integration_conflict_tasks": integration_conflict_tasks,
        "integration_conflict_count": integration_conflict_count,
        "blocked_handoff_tasks": blocked_handoff_tasks,
        "blocked_handoff_count": blocked_handoff_count,
        "recoverable_blocked_tasks": recoverable_blocked_tasks,
        "recoverable_blocked_count": recoverable_blocked_count,
        "review_ready_tasks": review_ready_tasks,
        "review_ready_count": review_ready_count,
        "changes_requested_tasks": changes_requested_tasks,
        "changes_requested_count": changes_requested_count,
        "stalled_tasks": stalled_tasks,
        "stalled_count": stalled_count,
        "approved_tasks": approved_tasks,
        "approved_count": approved_count,
        "followup_source_tasks": followup_source_tasks,
        "followup_source_count": followup_source_count,
        "next_action": next_action,
        "message": if !priority_notes.is_empty() {
            format!(
                "{}. {}",
                priority_notes.join("; "),
                if integration_conflict_count > 0 {
                    "Resolve supervisor-owned integration conflicts before review, integration, or new assignment work."
                } else if recoverable_blocked_count > 0 {
                    "Recover blocked handoff tasks before declaring the frontier blocked."
                } else {
                    "Treat these queues before declaring the frontier blocked."
                }
            )
        } else {
            format!("{count} pending worker task(s) ready for assignment.")
        }
    });

    Ok(text_result(
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::Serialization(e.to_string()))?,
    ))
}
