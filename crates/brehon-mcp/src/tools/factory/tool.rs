//! FactoryTool: the MCP tool struct, schema, and execute dispatch.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use brehon_ports::EventStore;
use brehon_types::{
    is_terminal_task_status, normalize_task_status, Event, EventKind, WorkerAssignmentMode,
};

use crate::error::McpError;
use crate::server::ToolResult;
use crate::tools::agent::{
    prepare_delivery_message, try_deliver_prepared_message, PROMPT_QUEUE_DELIVERY_METHOD,
};
use crate::tools::assignment_observability::{
    read_task_assignment_propagation, write_task_assignment_propagation, AssignmentPropagation,
};
use crate::tools::routing::{resolve_execution_policy, ExecutionPolicySource};
use crate::tools::stability::increment_assignment_history;
use crate::tools::task_actions::{
    acquire_task_lock, control_plane_scope_issue_for_task,
    task_has_active_or_unconsolidated_review, unmet_dependency_ids_for_task,
};
use crate::tools::{error_result, text_result, Tool};
use brehon_mux::PromptQueueEntry;

use super::git_sync::{
    AssignmentSeedKind, AssignmentSeedSyncResult, MergeTargetBaseSyncResult,
    MergeTargetBaseSyncStatus, MergeTargetSyncStatus,
};
use super::paths::{read_all_tasks, read_sessions, read_task, write_task};
use super::workers::{
    active_tasks_for_worker, agent_health, agent_is_unavailable, inspect_worktree,
    live_worker_names, query_nudge_state, require_supervisor_role, FORCE_REASSIGN_PARAM,
    HEARTBEAT_THRESHOLD_SECS, OUTPUT_THRESHOLD_SECS,
};
use super::worktree_ops::{
    archive_worktree_with_git2, check_worktree_state_with_git2, find_worktree_by_worker,
    remove_worktree_with_git2,
};

/// MCP tool for factory orchestration: spawning workers, checking status, and assigning tasks.
pub struct FactoryTool {
    event_store: Option<Arc<dyn EventStore + Send + Sync>>,
}

impl Default for FactoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl FactoryTool {
    /// Create a new factory tool with no event store.
    pub fn new() -> Self {
        Self { event_store: None }
    }

    /// Attach an event store for emitting domain events on assignments.
    pub fn with_event_store(mut self, store: Arc<dyn EventStore + Send + Sync>) -> Self {
        self.event_store = Some(store);
        self
    }

    async fn emit_event(&self, kind: EventKind, aggregate_id: String) {
        if let Some(ref store) = self.event_store {
            let event = Event {
                kind,
                timestamp: chrono::Utc::now(),
                aggregate_id,
            };
            if let Err(e) = store.append(event).await {
                tracing::warn!("Failed to emit event: {e}");
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WorkerAssignmentRoute {
    lane: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    assignment_mode: WorkerAssignmentMode,
    accepts: Vec<String>,
}

fn load_project_config() -> Option<brehon_types::BrehonConfig> {
    let project_root = super::paths::project_root()?;
    brehon_config::load_config(Some(&project_root)).ok()
}

fn assignment_mode_name(mode: WorkerAssignmentMode) -> &'static str {
    match mode {
        WorkerAssignmentMode::Normal => "normal",
        WorkerAssignmentMode::Reserved => "reserved",
    }
}

fn worker_route(
    worker_session: &Value,
    config: Option<&brehon_types::BrehonConfig>,
) -> WorkerAssignmentRoute {
    let lane = worker_session
        .get("agent_type")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let pool = lane.as_deref().and_then(|lane| {
        config.and_then(|config| config.roles.workers.iter().find(|pool| pool.lane == lane))
    });

    WorkerAssignmentRoute {
        lane,
        model: worker_session
            .get("model")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        reasoning_effort: worker_session
            .get("reasoning_effort")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        assignment_mode: pool
            .map(|pool| pool.assignment_mode)
            .unwrap_or(WorkerAssignmentMode::Normal),
        accepts: pool.map(|pool| pool.accepts.clone()).unwrap_or_default(),
    }
}

fn policy_string<'a>(
    policy: Option<&'a serde_json::Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    policy?
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
}

fn policy_work_classes(policy: Option<&serde_json::Map<String, Value>>) -> Vec<String> {
    let Some(policy) = policy else {
        return Vec::new();
    };
    let mut classes = Vec::new();
    if let Some(class) = policy
        .get("work_class")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        classes.push(class.to_string());
    }
    for key in ["work_classes", "tags"] {
        if let Some(values) = policy.get(key).and_then(|value| value.as_array()) {
            classes.extend(
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string),
            );
        }
    }
    classes.sort();
    classes.dedup();
    classes
}

pub(super) fn stage_task_assignment_propagation(
    task: &mut serde_json::Map<String, Value>,
    owner: &str,
    assignment_kind: &str,
) -> AssignmentPropagation {
    let propagation = AssignmentPropagation::new(owner, assignment_kind, None, None);
    write_task_assignment_propagation(task, &propagation);
    propagation
}

pub(super) fn merge_assignment_delivery_metadata(
    task: &mut serde_json::Map<String, Value>,
    owner: &str,
    prompt_id: &str,
    delivery_method: &str,
    fallback: Option<&AssignmentPropagation>,
) {
    let mut propagation = read_task_assignment_propagation(task)
        .filter(|propagation| propagation.owner.trim() == owner.trim())
        .or_else(|| fallback.cloned())
        .unwrap_or_else(|| AssignmentPropagation::new(owner, "task", None, None));
    if !prompt_id.trim().is_empty() {
        propagation.prompt_id = Some(prompt_id.to_string());
    }
    if !delivery_method.trim().is_empty() {
        propagation.delivery_method = Some(delivery_method.to_string());
    }
    write_task_assignment_propagation(task, &propagation);
}

pub(super) fn persist_assignment_delivery_metadata(
    task_id: &str,
    owner: &str,
    prompt_id: &str,
    delivery_method: &str,
    fallback: Option<&AssignmentPropagation>,
) -> Result<(), String> {
    let Some(mut task) = read_task(task_id) else {
        return Err(format!("could not re-read task {task_id} before delivery"));
    };
    let current_assignee = task
        .get("assignee")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("");
    if current_assignee != owner.trim() {
        return Err(format!(
            "task assignee changed to '{current_assignee}' before delivery metadata was recorded"
        ));
    }
    merge_assignment_delivery_metadata(&mut task, owner, prompt_id, delivery_method, fallback);
    if write_task(task_id, &task) {
        Ok(())
    } else {
        Err(format!("write failed for task {task_id}"))
    }
}

/// Validate that a prepared delivery entry has a non-empty prompt_id.
/// Returns the prompt_id on success, or an error message on failure.
pub(super) fn validate_assignment_delivery_entry(
    entry: &PromptQueueEntry,
    task_id: &str,
    assignee: &str,
) -> Result<String, String> {
    let prompt_id = entry.prompt_id.clone().unwrap_or_default();
    if prompt_id.trim().is_empty() {
        return Err(format!(
            "Task {task_id} was assigned to {assignee}, but Brehon could not mint assignment delivery metadata before notification dispatch. \
             No prompt was sent; the task remains assigned_without_delivery."
        ));
    }
    Ok(prompt_id)
}

fn execution_policy_is_strict(policy: Option<&serde_json::Map<String, Value>>) -> bool {
    policy
        .and_then(|policy| policy.get("strict"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn validate_assignment_route(
    task_id: &str,
    task: &serde_json::Map<String, Value>,
    assignee: &str,
    worker_session: &Value,
    config: Option<&brehon_types::BrehonConfig>,
    force_policy: bool,
) -> Result<(), String> {
    if force_policy {
        return Ok(());
    }

    let route = worker_route(worker_session, config);
    let resolved_policy = resolve_execution_policy(task, config);
    let policy = resolved_policy.policy.as_ref();
    let strict = execution_policy_is_strict(policy);
    let worker_lane = route.lane.as_deref().unwrap_or("<unknown>");

    if route.assignment_mode == WorkerAssignmentMode::Reserved {
        let preferred_lane = policy_string(policy, "preferred_lane");
        if preferred_lane != route.lane.as_deref() {
            let accepted = if route.accepts.is_empty() {
                "explicitly targeted reserved work".to_string()
            } else {
                route.accepts.join(", ")
            };
            return Err(format!(
                "Cannot assign task {task_id} to worker '{assignee}': worker lane '{worker_lane}' is reserved for {accepted}. \
                 Set execution_policy.preferred_lane='{worker_lane}' on the task or use force_policy=true."
            ));
        }

        if !route.accepts.is_empty() {
            let task_classes = policy_work_classes(policy);
            let matches_accepts = task_classes
                .iter()
                .any(|class| route.accepts.iter().any(|accepted| accepted == class));
            if !matches_accepts {
                let actual = if task_classes.is_empty() {
                    "<missing>".to_string()
                } else {
                    task_classes.join(", ")
                };
                return Err(format!(
                    "Cannot assign task {task_id} to worker '{assignee}': worker lane '{worker_lane}' accepts work classes [{}], \
                     but task execution_policy.work_class is {actual}. Use force_policy=true to override.",
                    route.accepts.join(", ")
                ));
            }
        }
    }

    if strict {
        if let Some(preferred_lane) = policy_string(policy, "preferred_lane") {
            if Some(preferred_lane) != route.lane.as_deref() {
                return Err(format!(
                    "Cannot assign task {task_id} to worker '{assignee}': execution_policy.preferred_lane='{preferred_lane}' \
                     but worker lane is '{worker_lane}'. Use force_policy=true to override."
                ));
            }
        }
        if let Some(preferred_model) = policy_string(policy, "preferred_model") {
            if Some(preferred_model) != route.model.as_deref() {
                let actual = route.model.as_deref().unwrap_or("<unknown>");
                return Err(format!(
                    "Cannot assign task {task_id} to worker '{assignee}': execution_policy.preferred_model='{preferred_model}' \
                     but worker model is '{actual}'. Use force_policy=true to override."
                ));
            }
        }
        if let Some(preferred_reasoning) = policy_string(policy, "preferred_reasoning_effort") {
            if Some(preferred_reasoning) != route.reasoning_effort.as_deref() {
                let actual = route.reasoning_effort.as_deref().unwrap_or("<unknown>");
                return Err(format!(
                    "Cannot assign task {task_id} to worker '{assignee}': execution_policy.preferred_reasoning_effort='{preferred_reasoning}' \
                     but worker reasoning_effort is '{actual}'. Use force_policy=true to override."
                ));
            }
        }
    }

    Ok(())
}

fn assignment_routing_result(
    task: &serde_json::Map<String, Value>,
    config: Option<&brehon_types::BrehonConfig>,
) -> Option<Value> {
    let resolved = resolve_execution_policy(task, config);
    if resolved.source == ExecutionPolicySource::None && resolved.policy.is_none() {
        return None;
    }

    Some(serde_json::json!({
        "source": resolved.source.as_str(),
        "rule_id": resolved.rule_id,
        "effective_execution_policy": resolved.policy,
    }))
}

#[async_trait]
impl Tool for FactoryTool {
    fn name(&self) -> &str {
        "factory"
    }

    fn description(&self) -> &str {
        "Factory orchestration - worker management, status, and assignment. Supported actions: spawn_workers, worker_status, assign_workers, set_ownership. Common aliases accepted: spawn, assign, dispatch, status, help."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: spawn_workers, worker_status, assign_workers, set_ownership. Aliases: spawn, assign, dispatch, status, help"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of workers to spawn"
                },
                "worker": {
                    "type": "string",
                    "description": "Worker name"
                },
                "task_id": {
                    "type": "string",
                    "description": "Task ID"
                },
                "workers": {
                    "type": "string",
                    "description": "Comma-separated worker names"
                },
                "force_reassign": {
                    "type": "boolean",
                    "description": "Force reassignment even if worktree is dirty (archives instead of deleting)"
                },
                "force_policy": {
                    "type": "boolean",
                    "description": "Override execution_policy and reserved-lane assignment checks"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, McpError> {
        let raw_action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let action = match raw_action {
            "spawn" | "spawn_worker" | "spawn_workers" => "spawn_workers",
            "status" | "worker_status" => "worker_status",
            "assign" | "dispatch" | "assign_workers" => "assign_workers",
            "set_ownership" => "set_ownership",
            "help" => "help",
            _ => raw_action,
        };

        match action {
            "help" => {
                let result = serde_json::json!({
                    "status": "ok",
                    "actions": [
                        "spawn_workers",
                        "worker_status",
                        "assign_workers",
                        "set_ownership"
                    ],
                    "aliases": {
                        "spawn": "spawn_workers",
                        "spawn_worker": "spawn_workers",
                        "status": "worker_status",
                        "assign": "assign_workers",
                        "dispatch": "assign_workers"
                    },
                    "message": "Factory supports spawn_workers, worker_status, assign_workers, and set_ownership."
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "spawn_workers" => {
                let count = args.get("count").and_then(|v| v.as_i64()).unwrap_or(1);

                let result = serde_json::json!({
                    "status": "ok",
                    "message": "Worker spawn request queued",
                    "count": count
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "worker_status" => {
                let sessions = read_sessions();
                let project_config = load_project_config();
                let now = chrono::Utc::now();
                let mut busy_workers = 0usize;
                let mut idle_general_workers = 0usize;
                let mut workers: Vec<Value> = Vec::new();

                for session in sessions
                    .iter()
                    .filter(|s| s.get("role").and_then(|v| v.as_str()) == Some("worker"))
                {
                    let mut worker = session.clone();
                    let worker_name = worker
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let session_id = worker
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();

                    let active_tasks = active_tasks_for_worker(&worker_name, None);
                    let active_task_summaries: Vec<Value> = active_tasks
                        .iter()
                        .map(|task| {
                            serde_json::json!({
                                "task_id": task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?"),
                                "title": task.get("title").and_then(|v| v.as_str()).unwrap_or("?"),
                                "status": task.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                            })
                        })
                        .collect();
                    let active_task_count = active_task_summaries.len();
                    let has_active_tasks = active_task_count > 0;
                    let health = agent_health(&worker_name);
                    let unavailable = agent_is_unavailable(&worker_name);
                    let route = worker_route(&worker, project_config.as_ref());
                    let available_for_general_assignment = !unavailable
                        && !has_active_tasks
                        && route.assignment_mode != WorkerAssignmentMode::Reserved;
                    if available_for_general_assignment {
                        idle_general_workers += 1;
                    }

                    let availability = if !has_active_tasks {
                        if unavailable {
                            "unavailable".to_string()
                        } else {
                            "idle".to_string()
                        }
                    } else {
                        busy_workers += 1;
                        let primary_task = &active_task_summaries[0];
                        let task_id = primary_task
                            .get("task_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let task_status = primary_task
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        if unavailable {
                            format!("unavailable (orphaned active task {}: {})", task_id, task_status)
                        } else {
                            format!("busy ({}: {})", task_id, task_status)
                        }
                    };

                    // Heartbeat and output liveness
                    let last_seen = worker
                        .get("last_seen_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc));

                    let heartbeat_live = last_seen
                        .map(|seen| (now - seen).num_seconds() < HEARTBEAT_THRESHOLD_SECS)
                        .unwrap_or(false);

                    // Output liveness: check if any active task was updated recently
                    let last_task_update = active_tasks
                        .iter()
                        .filter_map(|task| {
                            task.get("updated_at")
                                .and_then(|v| v.as_str())
                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                        })
                        .max();

                    let last_event_at = match (last_seen, last_task_update) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };

                    let output_live = last_task_update
                        .map(|t| (now - t).num_seconds() < OUTPUT_THRESHOLD_SECS)
                        .unwrap_or(false);

                    let idle_duration_secs = last_event_at
                        .map(|t| (now - t).num_seconds().max(0))
                        .unwrap_or(0);

                    // Nudge state from event store
                    let nudge_section = if let Some(ref store) = self.event_store {
                        let (state, nudge_at, count) =
                            query_nudge_state(store.as_ref(), &session_id).await;
                        serde_json::json!({
                            "last_nudge_at": nudge_at,
                            "nudge_delivery_state": state,
                            "nudges_sent_count": count
                        })
                    } else {
                        serde_json::json!({
                            "last_nudge_at": null,
                            "nudge_delivery_state": null,
                            "nudges_sent_count": 0
                        })
                    };

                    // Worktree inspection
                    let merge_target = active_tasks.first().and_then(|task| {
                        task.get("merge_target")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    });
                    let worktree_info = inspect_worktree(&worker_name, merge_target.as_deref());

                    if let Some(obj) = worker.as_object_mut() {
                        obj.insert("active_tasks".into(), Value::Array(active_task_summaries));
                        obj.insert("active_task_count".into(), serde_json::json!(active_task_count));
                        obj.insert("availability".into(), Value::String(availability));
                        obj.insert(
                            "available_for_assignment".into(),
                            Value::Bool(!unavailable && !has_active_tasks),
                        );
                        obj.insert(
                            "available_for_general_assignment".into(),
                            Value::Bool(available_for_general_assignment),
                        );
                        obj.insert(
                            "assignment_mode".into(),
                            Value::String(assignment_mode_name(route.assignment_mode).to_string()),
                        );
                        obj.insert(
                            "reserved".into(),
                            Value::Bool(route.assignment_mode == WorkerAssignmentMode::Reserved),
                        );
                        obj.insert(
                            "accepted_work_classes".into(),
                            Value::Array(
                                route
                                    .accepts
                                    .iter()
                                    .cloned()
                                    .map(Value::String)
                                    .collect(),
                            ),
                        );
                        obj.insert(
                            "agent_health".into(),
                            health.unwrap_or_else(|| serde_json::json!({"status": "unknown"})),
                        );
                        obj.insert("heartbeat_live".into(), Value::Bool(heartbeat_live));
                        obj.insert("output_live".into(), Value::Bool(output_live));
                        obj.insert(
                            "last_event_at".into(),
                            last_event_at.map_or(Value::Null, |t| Value::String(t.to_rfc3339())),
                        );
                        obj.insert(
                            "idle_duration_secs".into(),
                            serde_json::json!(idle_duration_secs),
                        );
                        obj.insert("nudge".into(), nudge_section);
                        obj.insert("worktree".into(), worktree_info);
                    }
                    workers.push(worker);
                }

                let reviewers: Vec<Value> = sessions
                    .iter()
                    .filter(|s| s.get("role").and_then(|v| v.as_str()) == Some("reviewer"))
                    .cloned()
                    .collect();

                let result = serde_json::json!({
                    "status": "ok",
                    "workers": workers,
                    "worker_count": workers.len(),
                    "idle_worker_count": workers.len().saturating_sub(busy_workers),
                    "idle_general_worker_count": idle_general_workers,
                    "busy_worker_count": busy_workers,
                    "reviewers": reviewers,
                    "reviewer_count": reviewers.len()
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "assign_workers" => {
                if let Err(result) = require_supervisor_role() {
                    return Ok(result);
                }

                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(error_result("Missing required parameter: task_id")),
                };
                let workers_str = match args.get("workers").and_then(|v| v.as_str()) {
                    Some(w) if !w.is_empty() => w,
                    _ => {
                        // Fall back to single "worker" param
                        match args.get("worker").and_then(|v| v.as_str()) {
                            Some(w) if !w.is_empty() => w,
                            _ => {
                                return Ok(error_result(
                                    "Missing required parameter: workers (comma-separated) or worker",
                                ));
                            }
                        }
                    }
                };

                let worker_names: Vec<&str> = workers_str
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if worker_names.is_empty() {
                    return Ok(error_result("No worker names provided"));
                }

                // Assign the first worker as assignee on the task
                let assignee = worker_names[0];
                let _lock = match acquire_task_lock(task_id).await {
                    Ok(lock) => lock,
                    Err(err) => {
                        return Ok(error_result(format!(
                            "Failed to lock task {task_id}: {err}"
                        )));
                    }
                };
                let mut assigned = false;
                let live_workers = live_worker_names();
                let worker_sessions = read_sessions();
                let project_config = load_project_config();
                let mut task_merge_target: Option<String> = None;
                let mut merge_target_base_sync: Option<MergeTargetBaseSyncResult> = None;
                let mut assignment_seed_sync: Option<AssignmentSeedSyncResult> = None;
                let mut routing_result: Option<Value> = None;
                let mut research_jobs_queued: Vec<Value> = Vec::new();
                let mut research_warning: Option<String> = None;
                let mut assignment_propagation: Option<AssignmentPropagation> = None;

                if let Some(mut task) = read_task(task_id) {
                    let task_type = task
                        .get("task_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("task");
                    if matches!(task_type, "epic" | "initiative") {
                        return Ok(error_result(format!(
                            "Cannot assign {task_type} {task_id} to a worker. Assign concrete tasks instead."
                        )));
                    }
                    if let Some(scope_issue) = control_plane_scope_issue_for_task(&task) {
                        return Ok(error_result(format!(
                            "Cannot assign task {task_id} to a worker because it targets live Brehon control-plane state. {scope_issue} Handle this as supervisor-controlled maintenance instead."
                        )));
                    }

                    let current_status = task
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let previous_assignee = task
                        .get("assignee")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.is_empty())
                        .map(String::from)
                        .or_else(|| {
                            task.get("orphaned_assignee")
                                .and_then(|v| v.as_str())
                                .filter(|value| !value.is_empty())
                                .map(String::from)
                        });
                    let normalized_status = match normalize_task_status(current_status) {
                        Some(status) => status,
                        None => {
                            return Ok(error_result(format!(
                                "Cannot assign task {task_id}: unknown task status '{current_status}'."
                            )));
                        }
                    };
                    let recovery_status = task
                        .get("orphaned_status")
                        .and_then(|v| v.as_str())
                        .and_then(normalize_task_status)
                        .unwrap_or(normalized_status);
                    let force_reassign = args
                        .get(FORCE_REASSIGN_PARAM)
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let force_policy = args
                        .get("force_policy")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if task_has_active_or_unconsolidated_review(task_id) && !force_reassign {
                        return Ok(error_result(format!(
                            "Cannot assign task {task_id}: an active or unconsolidated review round still exists. \
                             Let the review finish or explicitly reset/release the review before reassigning worker ownership."
                        )));
                    }

                    if normalized_status == "changes_requested" {
                        if let Some(existing_owner) = previous_assignee
                            .as_deref()
                            .filter(|existing| *existing != assignee)
                            .filter(|existing| live_workers.contains(*existing))
                        {
                            return Ok(error_result(format!(
                                "Cannot assign task {task_id} to worker '{assignee}': task is already owned by live worker '{existing_owner}' while changes are requested. \
                                 Transferring a live owner can leave two worker panes acting on the same task. Assign '{existing_owner}' again, or recycle/mark that worker unavailable before assigning another worker."
                            )));
                        }
                    }

                    let orphaned_active_reassignment = matches!(
                        recovery_status,
                        "assigned" | "in_progress" | "changes_requested"
                    ) && previous_assignee
                        .as_deref()
                        .is_some_and(|existing| !live_workers.contains(existing));

                    if !matches!(normalized_status, "pending" | "changes_requested")
                        && !orphaned_active_reassignment
                    {
                        let message = if is_terminal_task_status(normalized_status) {
                            format!(
                                "Cannot assign task {task_id}: status '{normalized_status}' is terminal."
                            )
                        } else {
                            format!(
                                "Cannot assign task {task_id}: status must be 'pending' or \
                                 'changes_requested', got '{normalized_status}'."
                            )
                        };
                        return Ok(error_result(message));
                    }

                    let all_tasks = read_all_tasks();
                    let unmet_dependencies = unmet_dependency_ids_for_task(&task, &all_tasks);
                    if !unmet_dependencies.is_empty() {
                        return Ok(error_result(format!(
                            "Cannot assign task {task_id}: unmet dependencies: {}.",
                            unmet_dependencies.join(", ")
                        )));
                    }

                    if !live_workers.contains(assignee) {
                        return Ok(error_result(format!(
                            "Cannot assign task {task_id}: worker '{assignee}' is not currently registered."
                        )));
                    }

                    let worker_session = match worker_sessions.iter().find(|session| {
                        session.get("name").and_then(|v| v.as_str()) == Some(assignee)
                            && session.get("role").and_then(|v| v.as_str()) == Some("worker")
                    }) {
                        Some(session) => session,
                        None => {
                            return Ok(error_result(format!(
                                "Cannot assign task {task_id}: worker '{assignee}' session metadata is unavailable."
                            )));
                        }
                    };
                    if let Err(message) = validate_assignment_route(
                        task_id,
                        &task,
                        assignee,
                        worker_session,
                        project_config.as_ref(),
                        force_policy,
                    ) {
                        return Ok(error_result(message));
                    }
                    routing_result = assignment_routing_result(&task, project_config.as_ref());

                    let other_active_tasks = active_tasks_for_worker(assignee, Some(task_id));
                    if !other_active_tasks.is_empty() {
                        let summaries = other_active_tasks
                            .iter()
                            .map(|task| {
                                let other_id =
                                    task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
                                let other_status =
                                    task.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                                format!("{other_id} [{other_status}]")
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Ok(error_result(format!(
                            "Cannot assign task {task_id} to worker '{assignee}': worker already has active task(s): {summaries}. \
                             Workers are single-task while using per-worker worktrees."
                        )));
                    }

                    if orphaned_active_reassignment {
                        if let Some(old_assignee) = &previous_assignee {
                            // Fail-closed: state check errors block reassignment unless forced
                            match find_worktree_by_worker(old_assignee) {
                                Ok(Some((repo, worktree_path))) => {
                                    match check_worktree_state_with_git2(&repo, &worktree_path) {
                                        Ok(state) => {
                                            use brehon_git::WorktreeStateCheck;
                                            match state {
                                                WorktreeStateCheck::Clean => {
                                                    if let Err(e) = remove_worktree_with_git2(
                                                        &repo,
                                                        &worktree_path,
                                                    ) {
                                                        tracing::warn!(
                                                            "Failed to remove clean worktree for {}: {}",
                                                            old_assignee,
                                                            e
                                                        );
                                                    }
                                                }
                                                WorktreeStateCheck::Missing => {
                                                    // No action needed - worktree doesn't exist
                                                }
                                                WorktreeStateCheck::Dirty { details }
                                                | WorktreeStateCheck::MidOperation {
                                                    operation: details,
                                                } => {
                                                    if !force_reassign {
                                                        return Ok(error_result(format!(
                                                            "Cannot reassign task {task_id}: old worker '{old_assignee}' has dirty worktree ({}) \
                                                             Use force_reassign=true to archive the worktree and proceed.",
                                                            details
                                                        )));
                                                    }

                                                    match archive_worktree_with_git2(
                                                        &repo,
                                                        &worktree_path,
                                                        old_assignee,
                                                        task_id,
                                                        "reassignment",
                                                    ) {
                                                        Ok(archive_path) => {
                                                            tracing::info!(
                                                                "Archived dirty worktree for {} at {}",
                                                                old_assignee,
                                                                archive_path
                                                            );
                                                            task.insert(
                                                                "worktree_archived".into(),
                                                                Value::String(archive_path.clone()),
                                                            );
                                                        }
                                                        Err(e) => {
                                                            return Ok(error_result(format!(
                                                                "Failed to archive worktree for {}: {}",
                                                                old_assignee, e
                                                            )));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            // State check failed - fail closed unless forced
                                            if !force_reassign {
                                                return Ok(error_result(format!(
                                                    "Cannot reassign task {task_id}: worktree state check failed for '{old_assignee}': {e} \
                                                     Use force_reassign=true to proceed anyway."
                                                )));
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    // No worktree found - continue with reassignment
                                }
                                Err(e) => {
                                    // Ambiguous or other error - fail closed unless forced
                                    if !force_reassign {
                                        return Ok(error_result(format!(
                                            "Cannot reassign task {task_id}: failed to locate worktree for '{old_assignee}': {e} \
                                             Use force_reassign=true to proceed anyway."
                                        )));
                                    }
                                }
                            }
                        }
                    }

                    task_merge_target = task
                        .get("merge_target")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let task_latest_commit = task
                        .get("latest_commit")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.is_empty())
                        .map(String::from);
                    if let Some(ref merge_target) = task_merge_target {
                        match super::git_sync::sync_merge_target_branch_to_parent_base(
                            &task,
                            merge_target,
                        ) {
                            Ok(sync) => {
                                merge_target_base_sync = sync;
                            }
                            Err(err) => {
                                return Ok(error_result(format!(
                                    "Cannot assign task {task_id}: {err}"
                                )));
                            }
                        }
                    }

                    let assignment_seed = if normalized_status == "changes_requested" {
                        task_latest_commit
                            .as_deref()
                            .map(|commit| (AssignmentSeedKind::LatestCommit, commit))
                            .or_else(|| {
                                task_merge_target.as_deref().map(|merge_target| {
                                    (AssignmentSeedKind::MergeTarget, merge_target)
                                })
                            })
                    } else {
                        task_merge_target
                            .as_deref()
                            .map(|merge_target| (AssignmentSeedKind::MergeTarget, merge_target))
                    };

                    if let Some((target_kind, target_ref)) = assignment_seed {
                        match super::git_sync::sync_worker_worktree_to_assignment_seed(
                            assignee,
                            target_ref,
                            target_kind,
                        ) {
                            Ok(sync) => {
                                assignment_seed_sync = Some(sync);
                            }
                            Err(err) => {
                                return Ok(error_result(format!(
                                    "Cannot assign task {task_id}: {err}"
                                )));
                            }
                        }
                    }

                    task.insert("assignee".into(), Value::String(assignee.to_string()));
                    task.insert("status".into(), Value::String("assigned".to_string()));
                    assignment_propagation =
                        Some(stage_task_assignment_propagation(&mut task, assignee, "task"));

                    if orphaned_active_reassignment {
                        let worktree_archived = task
                            .get("worktree_archived")
                            .and_then(|v| v.as_str())
                            .map(|_| true)
                            .unwrap_or(false);
                        let worktree_state_final = if worktree_archived {
                            "archived"
                        } else {
                            "clean"
                        };
                        let recovery_note = match previous_assignee.as_deref() {
                            Some(existing) => {
                                if worktree_archived {
                                    format!(
                                        "Recovered orphaned task from {recovery_status}: previous assignee {existing} was no longer live. \
                                         Worktree archived. Reassigned to {assignee}."
                                    )
                                } else {
                                    format!(
                                        "Recovered orphaned task from {recovery_status}: previous assignee {existing} was no longer live. Reassigned to {assignee}."
                                    )
                                }
                            }
                            None => format!(
                                "Recovered orphaned task from {recovery_status}. Reassigned to {assignee}."
                            ),
                        };
                        task.insert("recovery_note".into(), Value::String(recovery_note));
                        task.remove("orphaned_assignee");
                        task.remove("orphaned_status");

                        // Emit WorkerReassigned domain event
                        if let Some(old_worker) = previous_assignee.as_ref() {
                            self.emit_event(
                                EventKind::WorkerReassigned {
                                    old_worker: old_worker.clone(),
                                    new_worker: assignee.to_string(),
                                    task_id: task_id.to_string(),
                                    reason: "orphaned_recovery".to_string(),
                                    worktree_state: worktree_state_final.to_string(),
                                },
                                task_id.to_string(),
                            )
                            .await;
                        }
                    }
                    assigned = write_task(task_id, &task);
                }

                if !assigned {
                    return Ok(error_result(format!(
                        "Task not found or write failed: {task_id}"
                    )));
                }
                increment_assignment_history(1);

                if project_config
                    .as_ref()
                    .is_some_and(|config| config.research.enabled)
                {
                    match crate::tools::research::run_automatic_routes_for_task(
                        task_id,
                        brehon_types::ResearchTrigger::BeforeAssignment,
                        &std::env::var("BREHON_AGENT_NAME")
                            .unwrap_or_else(|_| "supervisor".to_string()),
                    ) {
                        Ok(queued) => {
                            research_jobs_queued = queued;
                        }
                        Err(err) => {
                            research_warning = Some(err);
                        }
                    }
                }

                // Deliver task notification to the worker's inbox so they
                // actually receive the assignment (not just file-on-disk).
                let task_title = read_task(task_id)
                    .and_then(|t| t.get("title").and_then(|v| v.as_str()).map(String::from))
                    .unwrap_or_else(|| task_id.to_string());
                let from =
                    std::env::var("BREHON_AGENT_NAME").unwrap_or_else(|_| "supervisor".to_string());

                let mut notification = format!(
                    "You have been assigned task {task_id}: {task_title}\n\
                     Call the task tool with action=mine to see your assigned tasks, \
                     then begin working on them."
                );
                if let Some(ref merge_target) = task_merge_target {
                    let base_sync_message = merge_target_base_sync.as_ref().map(|sync| match sync.status {
                        MergeTargetBaseSyncStatus::AlreadyCurrent => format!(
                            "Merge target branch '{}' in epic worktree '{}' already contained base branch '{}'; no parent-base sync was needed.",
                            sync.integration_branch,
                            sync.integration_worktree.display(),
                            sync.base_branch
                        ),
                        MergeTargetBaseSyncStatus::Merged => format!(
                            "Merge target branch '{}' in epic worktree '{}' was synced with base branch '{}' before assignment ({} -> {}).",
                            sync.integration_branch,
                            sync.integration_worktree.display(),
                            sync.base_branch,
                            sync.head_before,
                            sync.head_after
                        ),
                    });
                    if let Some(sync) = assignment_seed_sync.as_ref() {
                        let sync_message = match sync.status {
                            MergeTargetSyncStatus::AlreadyCurrent => format!(
                                "Your worker branch '{}' already matched {} '{}'; no reset was needed.",
                                sync.worker_branch,
                                sync.target_kind.as_str(),
                                sync.target_ref
                            ),
                            MergeTargetSyncStatus::Reset => match sync.preserved_ref.as_deref() {
                                Some(preserved_ref) => format!(
                                    "Your worker branch '{}' was reset to {} '{}' before assignment ({} -> {}). Previous branch head was preserved at '{}'.",
                                    sync.worker_branch,
                                    sync.target_kind.as_str(),
                                    sync.target_ref,
                                    sync.head_before,
                                    sync.head_after,
                                    preserved_ref
                                ),
                                None => format!(
                                    "Your worker branch '{}' was reset to {} '{}' before assignment ({} -> {}).",
                                    sync.worker_branch,
                                    sync.target_kind.as_str(),
                                    sync.target_ref,
                                    sync.head_before,
                                    sync.head_after
                                ),
                            },
                        };
                        notification.push_str(&format!(
                            "\n\nIMPORTANT: This task has merge_target='{}'. {} {} Stay on your current dedicated worker branch/worktree at '{}'. \
                             Do NOT checkout '{}' or main in this pane. Final integration into '{}' still happens through the review/epic flow.",
                            merge_target,
                            base_sync_message.as_deref().unwrap_or(""),
                            sync_message,
                            sync.worktree_path.display(),
                            merge_target,
                            merge_target
                        ));
                    } else {
                        notification.push_str(&format!(
                            "\n\nIMPORTANT: This task has merge_target='{}'. {} Stay on your current dedicated worker branch/worktree. \
                             Do NOT checkout '{}' or main in this pane. Final integration into '{}' happens through the review/epic flow.",
                            merge_target,
                            base_sync_message.as_deref().unwrap_or(""),
                            merge_target,
                            merge_target
                        ));
                    }
                }
                if project_config
                    .as_ref()
                    .is_some_and(|config| config.research.attach.on_task_assignment)
                {
                    if let Some(task) = read_task(task_id) {
                        let task = Value::Object(task);
                        let handoff = crate::tools::research::render_task_research_handoff(
                            &task,
                            project_config.as_ref(),
                        );
                        if !handoff.trim().is_empty() {
                            notification.push_str("\n\n");
                            notification.push_str(handoff.trim());
                        }
                    }
                }
                if !research_jobs_queued.is_empty() {
                    let job_ids = research_jobs_queued
                        .iter()
                        .filter_map(|job| job.get("job_id").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(", ");
                    notification.push_str(&format!(
                        "\n\nResearch jobs queued for this task: {job_ids}. Do not wait for them synchronously; they will attach artifacts as they complete."
                    ));
                }
                // Persist the prompt_id before enqueue so delivery observability
                // cannot be lost if the worker receives the prompt immediately.
                let delivery_entry = prepare_delivery_message(assignee, &from, &notification);
                let prompt_id = match validate_assignment_delivery_entry(
                    &delivery_entry,
                    task_id,
                    assignee,
                ) {
                    Ok(id) => id,
                    Err(msg) => return Ok(error_result(msg)),
                };
                if let Err(err) = persist_assignment_delivery_metadata(
                    task_id,
                    assignee,
                    &prompt_id,
                    PROMPT_QUEUE_DELIVERY_METHOD,
                    assignment_propagation.as_ref(),
                ) {
                    return Ok(error_result(format!(
                        "Task {task_id} was assigned to {assignee}, but Brehon could not persist assignment delivery metadata before notification dispatch ({err}). \
                         No prompt was sent; the task remains assigned_without_delivery."
                    )));
                }
                let delivery = try_deliver_prepared_message(delivery_entry);

                // If enqueue failed after the pre-dispatch persist, update the
                // propagation so delivery_method reflects the actual outcome
                // rather than leaving a misleading "queued" marker.
                if !delivery.queued {
                    if let Err(err) = persist_assignment_delivery_metadata(
                        task_id,
                        assignee,
                        &prompt_id,
                        "persisted_not_enqueued",
                        assignment_propagation.as_ref(),
                    ) {
                        tracing::warn!(
                            task_id = %task_id,
                            assignee = %assignee,
                            error = %err,
                            "Failed to update propagation after delivery failure"
                        );
                    }
                }

                let refreshed_task = read_task(task_id);
                let mut result = serde_json::json!({
                    "status": "ok",
                    "task_id": task_id,
                    "assignee": assignee,
                    "all_workers": worker_names,
                    "inbox_delivered": delivery.queued,
                    "message": format!("Task {task_id} assigned to {assignee}")
                });
                if !delivery.prompt_id.is_empty() {
                    result["prompt_id"] = Value::String(delivery.prompt_id.clone());
                }
                if let Some(task) = refreshed_task.as_ref() {
                    if let Some(note) = task.get("recovery_note").and_then(|v| v.as_str()) {
                        result["recovery_note"] = Value::String(note.to_string());
                        result["recovered_orphaned_task"] = Value::Bool(true);
                    }
                    if let Some(archived) = task.get("worktree_archived").and_then(|v| v.as_str()) {
                        result["worktree_archived"] = Value::String(archived.to_string());
                    }
                    if let Some(merge_target) = task.get("merge_target").and_then(|v| v.as_str()) {
                        result["merge_target"] = Value::String(merge_target.to_string());
                    }
                }
                if let Some(sync) = assignment_seed_sync.as_ref() {
                    result["assignment_seed_sync"] = serde_json::json!({
                        "status": match sync.status {
                            MergeTargetSyncStatus::AlreadyCurrent => "already_current",
                            MergeTargetSyncStatus::Reset => "reset",
                        },
                        "kind": sync.target_kind.as_str(),
                        "worker_branch": sync.worker_branch,
                        "target_ref": sync.target_ref,
                        "head_before": sync.head_before,
                        "head_after": sync.head_after,
                        "worktree_path": sync.worktree_path,
                        "preserved_ref": sync.preserved_ref,
                    });
                    if sync.target_kind == AssignmentSeedKind::MergeTarget {
                        result["merge_target_sync"] = serde_json::json!({
                            "status": match sync.status {
                                MergeTargetSyncStatus::AlreadyCurrent => "already_current",
                                MergeTargetSyncStatus::Reset => "reset",
                            },
                            "worker_branch": sync.worker_branch,
                            "merge_target": sync.target_ref,
                            "head_before": sync.head_before,
                            "head_after": sync.head_after,
                            "worktree_path": sync.worktree_path,
                            "preserved_ref": sync.preserved_ref,
                        });
                    }
                }
                if let Some(sync) = merge_target_base_sync.as_ref() {
                    result["merge_target_base_sync"] = serde_json::json!({
                        "status": match sync.status {
                            MergeTargetBaseSyncStatus::AlreadyCurrent => "already_current",
                            MergeTargetBaseSyncStatus::Merged => "merged",
                        },
                        "merge_target": sync.integration_branch,
                        "base_branch": sync.base_branch,
                        "head_before": sync.head_before,
                        "head_after": sync.head_after,
                        "integration_worktree": sync.integration_worktree,
                    });
                }
                if !research_jobs_queued.is_empty() {
                    result["research_jobs_queued"] = Value::Array(research_jobs_queued);
                }
                if let Some(warning) = research_warning {
                    result["research_warning"] = Value::String(warning);
                }
                if let Some(routing) = routing_result {
                    result["routing"] = routing;
                }

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            "set_ownership" => {
                if let Err(result) = require_supervisor_role() {
                    return Ok(result);
                }

                let task_id = match args.get("task_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.is_empty() => id,
                    _ => return Ok(error_result("Missing required parameter: task_id")),
                };
                let worker = match args.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.is_empty() => w,
                    _ => return Ok(error_result("Missing required parameter: worker")),
                };

                let _lock = match acquire_task_lock(task_id).await {
                    Ok(lock) => lock,
                    Err(err) => {
                        return Ok(error_result(format!(
                            "Failed to lock task {task_id}: {err}"
                        )));
                    }
                };

                let mut set = false;
                if let Some(mut task) = read_task(task_id) {
                    let current_status = task
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let normalized_status = match normalize_task_status(current_status) {
                        Some(status) => status,
                        None => {
                            return Ok(error_result(format!(
                                "Cannot change ownership for task {task_id}: unknown task status '{current_status}'."
                            )));
                        }
                    };
                    if is_terminal_task_status(normalized_status) {
                        return Ok(error_result(format!(
                            "Cannot change ownership for task {task_id}: status '{normalized_status}' is terminal."
                        )));
                    }
                    let existing_assignee = task
                        .get("assignee")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let existing_assignee = existing_assignee.map(str::to_string);
                    let propagation_owner_matches = read_task_assignment_propagation(&task)
                        .as_ref()
                        .map(|propagation| propagation.owner.trim())
                        == Some(worker);
                    task.insert("assignee".into(), Value::String(worker.to_string()));
                    if existing_assignee.as_deref() != Some(worker) || !propagation_owner_matches {
                        stage_task_assignment_propagation(&mut task, worker, "task");
                    }
                    set = write_task(task_id, &task);
                }

                if !set {
                    return Ok(error_result(format!(
                        "Task not found or write failed: {task_id}"
                    )));
                }

                let result = serde_json::json!({
                    "status": "ok",
                    "task_id": task_id,
                    "owner": worker
                });

                Ok(text_result(
                    serde_json::to_string_pretty(&result)
                        .map_err(|e| McpError::Serialization(e.to_string()))?,
                ))
            }
            _ => Ok(error_result(format!(
                "Unknown factory action: {}. Supported actions: spawn_workers, worker_status, assign_workers, set_ownership. Aliases: spawn, assign, dispatch, status, help.",
                raw_action
            ))),
        }
    }
}
